use std::{
    collections::HashMap,
    fmt::{
        Debug,
        Formatter,
    },
    sync::Arc,
};

use anyhow::anyhow;

use fuel_core_interfaces::p2p::{
    BlockBroadcast,
    ConsensusBroadcast,
    GossipsubMessageAcceptance,
    GossipsubMessageId,
    NetworkData,
    P2pDb,
    P2pRequestEvent,
    TransactionBroadcast,
};
use libp2p::{
    gossipsub::{
        MessageAcceptance,
        MessageId,
    },
    request_response::RequestId,
    PeerId,
};
use tokio::{
    sync::{
        broadcast,
        mpsc::{
            Receiver,
            Sender,
        },
        Mutex,
    },
    task::JoinHandle,
};
use tracing::{
    info,
    warn,
};

use crate::{
    codecs::bincode::BincodeCodec,
    config::P2PConfig,
    gossipsub::messages::{
        GossipsubBroadcastRequest,
        GossipsubMessage,
    },
    request_response::messages::{
        OutboundResponse,
        RequestMessage,
        ResponseChannelItem,
    },
    service::{
        FuelP2PEvent,
        FuelP2PService,
    },
};

type ConsensusWithMsgId = GossipData<ConsensusBroadcast>;
type TransactionWithMsgId = GossipData<TransactionBroadcast>;
type BlockWithMsgId = GossipData<BlockBroadcast>;

type MessageIdWithPeer = (MessageId, PeerId);

pub struct NetworkOrchestrator {
    p2p_config: P2PConfig,

    /// receives messages from different Fuel components
    rx_request_event: Receiver<P2pRequestEvent>,
    rx_outbound_responses: Receiver<Option<(OutboundResponse, RequestId)>>,

    // senders
    tx_consensus: Sender<GossipData<ConsensusBroadcast>>,
    tx_transaction: broadcast::Sender<TransactionWithMsgId>,
    tx_block: Sender<BlockWithMsgId>,
    tx_outbound_responses: Sender<Option<(OutboundResponse, RequestId)>>,
    db: Arc<dyn P2pDb>,
}

#[derive(Debug, Clone)]
pub struct GossipData<T> {
    pub data: Option<T>,
    pub peer_id: PeerId,
    pub message_id: MessageId,
}

impl<T> GossipData<T> {
    pub fn new(value: T, peer_id: PeerId, message_id: MessageId) -> Self {
        Self {
            data: Some(value),
            peer_id,
            message_id,
        }
    }
}

impl<T: Debug + Send + 'static> NetworkData<T> for GossipData<T> {
    fn take_data(&mut self) -> Option<T> {
        self.data.take()
    }

    fn message_id(&self) -> Vec<u8> {
        self.message_id.0.clone()
    }

    fn peer_id(&self) -> Vec<u8> {
        self.peer_id.to_bytes()
    }
}

impl NetworkOrchestrator {
    pub fn new(
        p2p_config: P2PConfig,
        rx_request_event: Receiver<P2pRequestEvent>,

        tx_consensus: Sender<ConsensusWithMsgId>,
        tx_transaction: broadcast::Sender<TransactionWithMsgId>,
        tx_block: Sender<BlockWithMsgId>,

        db: Arc<dyn P2pDb>,
    ) -> Self {
        let (tx_outbound_responses, rx_outbound_responses) =
            tokio::sync::mpsc::channel(100);

        Self {
            p2p_config,
            rx_request_event,
            rx_outbound_responses,
            tx_block,
            tx_consensus,
            tx_transaction,
            tx_outbound_responses,
            db,
        }
    }

    pub async fn run(mut self) -> anyhow::Result<Self> {
        let mut p2p_service = FuelP2PService::new(
            self.p2p_config.clone(),
            BincodeCodec::new(self.p2p_config.max_block_size),
        )?;

        loop {
            tokio::select! {
                next_response = self.rx_outbound_responses.recv() => {
                    if let Some(Some((response, request_id))) = next_response {
                        let _ = p2p_service.send_response_msg(request_id, response);
                    }
                },
                p2p_event = p2p_service.next_event() => {
                    match p2p_event {
                        Some(FuelP2PEvent::GossipsubMessage { message, message_id, peer_id,.. }) => {

                            match message {
                                GossipsubMessage::NewTx(tx) => {
                                    let _ = self.tx_transaction.send(GossipData::new(TransactionBroadcast::NewTransaction(tx), peer_id, message_id) );
                                },
                                GossipsubMessage::NewBlock(block) => {
                                    let _ = self.tx_block.send(GossipData::new(BlockBroadcast::NewBlock(block), peer_id, message_id));
                                },
                                GossipsubMessage::ConsensusVote(vote) => {
                                    let _ = self.tx_consensus.send(GossipData::new(ConsensusBroadcast::NewVote(vote), peer_id, message_id));
                                },
                            }
                        },
                        Some(FuelP2PEvent::RequestMessage { request_message, request_id }) => {
                            match request_message {
                                RequestMessage::RequestBlock(block_height) => {
                                    let db = self.db.clone();
                                    let tx_outbound_response = self.tx_outbound_responses.clone();

                                    tokio::spawn(async move {
                                        let res = db.get_sealed_block(block_height).await.map(|block| (OutboundResponse::ResponseBlock(block), request_id));
                                        let _ = tx_outbound_response.send(res);
                                    });
                                }
                            }
                        },
                        _ => {}
                    }
                },
                module_request_msg = self.rx_request_event.recv() => {
                    if let Some(request_event) = module_request_msg {
                        match request_event {
                            P2pRequestEvent::RequestBlock { height, response } => {
                                let request_msg = RequestMessage::RequestBlock(height);
                                let channel_item = ResponseChannelItem::ResponseBlock(response);
                                let _ = p2p_service.send_request_msg(None, request_msg, channel_item);
                            },
                            P2pRequestEvent::BroadcastNewBlock { block } => {
                                let broadcast = GossipsubBroadcastRequest::NewBlock(block);
                                let _ = p2p_service.publish_message(broadcast);
                            },
                            P2pRequestEvent::BroadcastNewTransaction { transaction } => {
                                let broadcast = GossipsubBroadcastRequest::NewTx(transaction);
                                let _ = p2p_service.publish_message(broadcast);
                            },
                            P2pRequestEvent::BroadcastConsensusVote { vote } => {
                                let broadcast = GossipsubBroadcastRequest::ConsensusVote(vote);
                                let _ = p2p_service.publish_message(broadcast);
                            },
                            P2pRequestEvent::GossipsubMessageReport { message, acceptance } => {
                                let msg_id = message.message_id().into();
                                let peer_id = message.peer_id().try_into().unwrap();

                                let acceptance = match acceptance {
                                    GossipsubMessageAcceptance::Accept => MessageAcceptance::Accept,
                                    GossipsubMessageAcceptance::Reject => MessageAcceptance::Reject,
                                    GossipsubMessageAcceptance::Ignore => MessageAcceptance::Ignore
                                };

                                match p2p_service.report_message_validation_result(&msg_id, &peer_id, acceptance) {
                                    Ok(true) => {
                                        info!(target: "fuel-libp2p", "Sent a report for MessageId: {} from PeerId: {}", msg_id, peer_id);
                                    }
                                    Ok(false) => {
                                        warn!(target: "fuel-libp2p", "Message with MessageId: {} not found in the Gossipsub Message Cache", msg_id);
                                    }
                                    Err(e) => {
                                        warn!(target: "fuel-libp2p", "Failed to publish Message with MessageId: {} with Error: {:?}", msg_id, e);
                                    }
                                }

                            }
                            P2pRequestEvent::Stop => break,
                        }
                    } else {
                        warn!(target: "fuel-libp2p", "Failed to receive P2PRequestEvent");
                    }
                }
            }
        }

        Ok(self)
    }
}

pub struct Service {
    /// Network Orchestrator that handles p2p network and inter-module communication
    network_orchestrator: Arc<Mutex<Option<NetworkOrchestrator>>>,
    /// Holds the spawned task when Netowrk Orchestrator is started
    join: Mutex<Option<JoinHandle<Result<NetworkOrchestrator, anyhow::Error>>>>,
    /// Used for notifying the Network Orchestrator to stop
    tx_request_event: Sender<P2pRequestEvent>,
}

impl Service {
    pub fn new(
        p2p_config: P2PConfig,
        db: Arc<dyn P2pDb>,
        tx_request_event: Sender<P2pRequestEvent>,
        rx_request_event: Receiver<P2pRequestEvent>,
        tx_consensus: Sender<ConsensusWithMsgId>,
        tx_transaction: broadcast::Sender<TransactionWithMsgId>,
        tx_block: Sender<BlockWithMsgId>,
    ) -> Self {
        let network_orchestrator = NetworkOrchestrator::new(
            p2p_config,
            rx_request_event,
            tx_consensus,
            tx_transaction,
            tx_block,
            db,
        );

        Self {
            join: Mutex::new(None),
            network_orchestrator: Arc::new(Mutex::new(Some(network_orchestrator))),
            tx_request_event,
        }
    }

    pub async fn start(&self) -> anyhow::Result<()> {
        let mut join = self.join.lock().await;

        if join.is_none() {
            if let Some(network_orchestrator) =
                self.network_orchestrator.lock().await.take()
            {
                *join = Some(tokio::spawn(async { network_orchestrator.run().await }));

                Ok(())
            } else {
                Err(anyhow!("Starting Network Orchestrator that is stopping"))
            }
        } else {
            Err(anyhow!("Network Orchestrator already started"))
        }
    }

    pub async fn stop(&self) -> Option<JoinHandle<()>> {
        let join_handle = self.join.lock().await.take();

        if let Some(join_handle) = join_handle {
            let network_orchestrator = self.network_orchestrator.clone();
            let _ = self.tx_request_event.send(P2pRequestEvent::Stop).await;
            Some(tokio::spawn(async move {
                if let Ok(res) = join_handle.await {
                    *network_orchestrator.lock().await = res.ok();
                }
            }))
        } else {
            None
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use async_trait::async_trait;
    use fuel_core_interfaces::model::{
        BlockHeight,
        FuelBlock,
        FuelBlockConsensus,
        SealedFuelBlock,
    };
    use tokio::time::{
        sleep,
        Duration,
    };

    #[derive(Clone, Debug)]
    struct FakeDb;

    #[async_trait]
    impl P2pDb for FakeDb {
        async fn get_sealed_block(
            &self,
            _height: BlockHeight,
        ) -> Option<Arc<SealedFuelBlock>> {
            let block = FuelBlock {
                header: Default::default(),
                transactions: vec![],
            };

            Some(Arc::new(SealedFuelBlock {
                block,
                consensus: FuelBlockConsensus {
                    required_stake: 100_000,
                    validators: Default::default(),
                },
            }))
        }
    }

    #[tokio::test]
    async fn start_stop_works() {
        let p2p_config = P2PConfig::default_with_network("start_stop_works");
        let db: Arc<dyn P2pDb> = Arc::new(FakeDb);

        let (tx_request_event, rx_request_event) = tokio::sync::mpsc::channel(100);
        let (tx_consensus, _) = tokio::sync::mpsc::channel(100);
        let (tx_transaction, _) = tokio::sync::broadcast::channel(100);
        let (tx_block, _) = tokio::sync::mpsc::channel(100);

        let service = Service::new(
            p2p_config,
            db.clone(),
            tx_request_event,
            rx_request_event,
            tx_consensus,
            tx_transaction,
            tx_block,
        );

        // Node with p2p service started
        assert!(service.start().await.is_ok());
        sleep(Duration::from_secs(1)).await;
        // Node with p2p service stopped
        assert!(service.stop().await.is_some());
        sleep(Duration::from_secs(1)).await;

        // Node with p2p service successfully restarted
        assert!(service.start().await.is_ok());
    }
}
