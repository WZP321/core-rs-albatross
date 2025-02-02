#![allow(dead_code)]

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use bytes::{Buf, Bytes};
use futures::executor;
use futures::{
    channel::{mpsc, oneshot},
    sink::SinkExt,
    stream::{BoxStream, StreamExt},
};
#[cfg(test)]
use libp2p::core::transport::MemoryTransport;
use libp2p::{
    core,
    core::{muxing::StreamMuxerBox, transport::Boxed},
    dns,
    gossipsub::{
        error::PublishError, GossipsubEvent, GossipsubMessage, IdentTopic, MessageAcceptance,
        MessageId, TopicHash, TopicScoreParams,
    },
    identify::IdentifyEvent,
    identity::Keypair,
    kad::{
        store::RecordStore, GetRecordOk, InboundRequest, KademliaEvent, QueryId, QueryResult,
        Quorum, Record,
    },
    noise,
    ping::Success,
    swarm::{dial_opts::DialOpts, ConnectionLimits, NetworkInfo, SwarmBuilder, SwarmEvent},
    tcp, websocket, yamux, Multiaddr, PeerId, Swarm, Transport,
};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tracing::Instrument;

use beserial::{Deserialize, Serialize};
use nimiq_bls::CompressedPublicKey;
use nimiq_network_interface::{
    message::{Message, MessageType},
    network::{MsgAcceptance, Network as NetworkInterface, NetworkEvent, PubsubId, Topic},
    peer::Peer as PeerInterface,
    peer_map::ObservablePeerMap,
};
use nimiq_utils::time::OffsetTime;
use nimiq_validator_network::validator_record::SignedValidatorRecord;

use crate::{
    behaviour::{NimiqBehaviour, NimiqEvent, NimiqNetworkBehaviourError},
    connection_pool::behaviour::ConnectionPoolEvent,
    peer::Peer,
    Config, NetworkError,
};

/// Maximum simultaneous libp2p connections per peer
const MAX_CONNECTIONS_PER_PEER: u32 = 1;

type NimiqSwarm = Swarm<NimiqBehaviour>;
#[derive(Debug)]
pub(crate) enum NetworkAction {
    Dial {
        peer_id: PeerId,
        output: oneshot::Sender<Result<(), NetworkError>>,
    },
    DialAddress {
        address: Multiaddr,
        output: oneshot::Sender<Result<(), NetworkError>>,
    },
    DhtGet {
        key: Vec<u8>,
        output: oneshot::Sender<Result<Option<Vec<u8>>, NetworkError>>,
    },
    DhtPut {
        key: Vec<u8>,
        value: Vec<u8>,
        output: oneshot::Sender<Result<(), NetworkError>>,
    },
    Subscribe {
        topic_name: &'static str,
        buffer_size: usize,
        validate: bool,
        output: oneshot::Sender<
            Result<mpsc::Receiver<(GossipsubMessage, MessageId, PeerId)>, NetworkError>,
        >,
    },
    Unsubscribe {
        topic_name: &'static str,
        output: oneshot::Sender<Result<(), NetworkError>>,
    },
    Publish {
        topic_name: &'static str,
        data: Vec<u8>,
        output: oneshot::Sender<Result<MessageId, NetworkError>>,
    },
    NetworkInfo {
        output: oneshot::Sender<NetworkInfo>,
    },
    Validate {
        message_id: MessageId,
        source: PeerId,
        acceptance: MessageAcceptance,
        output: oneshot::Sender<Result<bool, NetworkError>>,
    },
    ReceiveFromAll {
        type_id: MessageType,
        output: mpsc::Sender<(Bytes, Arc<Peer>)>,
    },
    ListenOn {
        listen_addresses: Vec<Multiaddr>,
    },
    StartConnecting,
}

struct ValidateMessage<P: Clone> {
    pubsub_id: GossipsubId<P>,
    acceptance: MessageAcceptance,
    topic: &'static str,
}

impl<P: Clone> ValidateMessage<P> {
    pub fn new<T>(pubsub_id: GossipsubId<P>, acceptance: MsgAcceptance) -> Self
    where
        T: Topic + Sync,
    {
        Self {
            pubsub_id,
            acceptance: match acceptance {
                MsgAcceptance::Accept => MessageAcceptance::Accept,
                MsgAcceptance::Ignore => MessageAcceptance::Ignore,
                MsgAcceptance::Reject => MessageAcceptance::Reject,
            },
            topic: <T as Topic>::NAME,
        }
    }
}

#[derive(Default)]
struct TaskState {
    dht_puts: HashMap<QueryId, oneshot::Sender<Result<(), NetworkError>>>,
    dht_gets: HashMap<QueryId, oneshot::Sender<Result<Option<Vec<u8>>, NetworkError>>>,
    gossip_topics: HashMap<TopicHash, (mpsc::Sender<(GossipsubMessage, MessageId, PeerId)>, bool)>,
    is_bootstraped: bool,
}

#[derive(Clone, Debug)]
pub struct GossipsubId<P: Clone> {
    message_id: MessageId,
    propagation_source: P,
}

impl PubsubId<PeerId> for GossipsubId<PeerId> {
    fn propagation_source(&self) -> PeerId {
        self.propagation_source
    }
}

pub struct Network {
    local_peer_id: PeerId,
    events_tx: broadcast::Sender<NetworkEvent<Peer>>,
    action_tx: mpsc::Sender<NetworkAction>,
    peers: ObservablePeerMap<Peer>,
    validate_tx: mpsc::UnboundedSender<ValidateMessage<PeerId>>,
}

impl Network {
    /// Create a new libp2p network instance.
    ///
    /// # Arguments
    ///
    ///  - `clock`: The clock that is used to establish the network time. The discovery behavior will determine the
    ///             offset by exchanging their wall-time with other peers.
    ///  - `config`: The network configuration, containing key pair, and other behavior-specific configuration.
    ///
    pub async fn new(clock: Arc<OffsetTime>, config: Config) -> Self {
        let peers = ObservablePeerMap::new();
        let swarm = Self::new_swarm(clock, config, peers.clone());

        let local_peer_id = *Swarm::local_peer_id(&swarm);

        let (events_tx, _) = broadcast::channel(64);
        let (action_tx, action_rx) = mpsc::channel(64);
        let (validate_tx, validate_rx) = mpsc::unbounded();

        tokio::spawn(Self::swarm_task(
            swarm,
            events_tx.clone(),
            action_rx,
            validate_rx,
        ));

        Self {
            local_peer_id,
            events_tx,
            action_tx,
            peers,
            validate_tx,
        }
    }

    fn new_transport(keypair: &Keypair) -> std::io::Result<Boxed<(PeerId, StreamMuxerBox)>> {
        // Websocket over TCP/DNS
        #[cfg(not(test))]
        let transport = websocket::WsConfig::new(dns::TokioDnsConfig::system(
            tcp::TokioTcpConfig::new().nodelay(true),
        )?);

        // Memory transport for testing
        // TODO: Use websocket over the memory transport
        #[cfg(test)]
        let transport = websocket::WsConfig::new(dns::TokioDnsConfig::system(
            tcp::TokioTcpConfig::new().nodelay(true),
        )?)
        .or_transport(MemoryTransport::default());

        let noise_keys = noise::Keypair::<noise::X25519Spec>::new()
            .into_authentic(keypair)
            .unwrap();

        let mut yamux = yamux::YamuxConfig::default();
        yamux.set_window_update_mode(yamux::WindowUpdateMode::on_read());

        Ok(transport
            .upgrade(core::upgrade::Version::V1)
            .authenticate(noise::NoiseConfig::xx(noise_keys).into_authenticated())
            .multiplex(yamux)
            .timeout(std::time::Duration::from_secs(20))
            .boxed())
    }

    fn new_swarm(
        clock: Arc<OffsetTime>,
        config: Config,
        peers: ObservablePeerMap<Peer>,
    ) -> Swarm<NimiqBehaviour> {
        let local_peer_id = PeerId::from(config.keypair.public());

        let transport = Self::new_transport(&config.keypair).unwrap();

        let behaviour = NimiqBehaviour::new(config, clock, peers);

        let limits = ConnectionLimits::default()
            .with_max_pending_incoming(Some(16))
            .with_max_pending_outgoing(Some(16))
            .with_max_established_incoming(Some(4800))
            .with_max_established_outgoing(Some(4800))
            .with_max_established_per_peer(Some(MAX_CONNECTIONS_PER_PEER));

        // TODO add proper config
        SwarmBuilder::new(transport, behaviour, local_peer_id)
            .connection_limits(limits)
            .executor(Box::new(|fut| {
                tokio::spawn(fut);
            }))
            .build()
    }

    pub fn local_peer_id(&self) -> &PeerId {
        &self.local_peer_id
    }

    async fn swarm_task(
        mut swarm: NimiqSwarm,
        events_tx: broadcast::Sender<NetworkEvent<Peer>>,
        mut action_rx: mpsc::Receiver<NetworkAction>,
        mut validate_rx: mpsc::UnboundedReceiver<ValidateMessage<PeerId>>,
    ) {
        let mut task_state = TaskState::default();

        let peer_id = Swarm::local_peer_id(&swarm);
        let task_span = tracing::trace_span!("swarm task", peer_id=?peer_id);

        async move {
            loop {
                tokio::select! {
                    validate_msg = validate_rx.next() => {
                        if let Some(validate_msg) = validate_msg {
                            let topic = validate_msg.topic;
                            let result: Result<bool, PublishError> = swarm
                                .behaviour_mut()
                                .gossipsub
                                .report_message_validation_result(
                                    &validate_msg.pubsub_id.message_id,
                                    &validate_msg.pubsub_id.propagation_source,
                                    validate_msg.acceptance,
                                );

                            match result {
                                Ok(true) => {}, // success
                                Ok(false) => log::debug!("Validation took too long: the {} message is no longer in the message cache", topic),
                                Err(e) => log::error!("Network error while relaying {} message: {}", topic, e),
                            }
                        }
                    },
                    event = swarm.next() => {
                        if let Some(event) = event {
                            Self::handle_event(event, &events_tx, &mut swarm, &mut task_state);
                        }
                    },
                    action = action_rx.next() => {
                        if let Some(action) = action {
                            Self::perform_action(action, &mut swarm, &mut task_state);
                        }
                        else {
                            // `action_rx.next()` will return `None` if all senders (i.e. the `Network` object) are dropped.
                            break;
                        }
                    },
                };
            }
        }
        .instrument(task_span)
        .await
    }

    fn handle_event(
        event: SwarmEvent<NimiqEvent, NimiqNetworkBehaviourError>,
        events_tx: &broadcast::Sender<NetworkEvent<Peer>>,
        swarm: &mut NimiqSwarm,
        state: &mut TaskState,
    ) {
        match event {
            SwarmEvent::ConnectionEstablished {
                peer_id,
                endpoint,
                num_established,
                concurrent_dial_errors,
            } => {
                tracing::info!(
                    "Connection established with peer {}, {:?}, connections established: {:?}",
                    peer_id,
                    endpoint,
                    num_established
                );

                if let Some(dial_errors) = concurrent_dial_errors {
                    for (addr, error) in dial_errors {
                        tracing::debug!(
                            "Failed to reach address: {}, peer_id={:?}, error={:?}",
                            addr,
                            peer_id,
                            error
                        );
                        swarm.behaviour_mut().remove_peer_address(peer_id, addr);
                    }
                }

                // Save dialed peer addresses
                if endpoint.is_dialer() {
                    let listen_addr = endpoint.get_remote_address();

                    tracing::debug!("Saving peer {} listen address: {:?}", peer_id, listen_addr);

                    swarm
                        .behaviour_mut()
                        .add_peer_address(peer_id, listen_addr.clone());

                    // Bootstrap Kademlia if we're performing our first connection
                    if !state.is_bootstraped {
                        log::debug!("Bootstrapping DHT");
                        if swarm.behaviour_mut().dht.bootstrap().is_err() {
                            tracing::error!("Bootstrapping DHT error: No known peers");
                        }
                        state.is_bootstraped = true;
                    }
                }
            }

            SwarmEvent::ConnectionClosed {
                peer_id,
                endpoint,
                num_established,
                cause,
            } => {
                tracing::info!(
                    "Connection closed with peer {}, {:?}, connections established: {:?}",
                    peer_id,
                    endpoint,
                    num_established
                );

                if let Some(cause) = cause {
                    tracing::info!("Connection closed because: {:?}", cause);
                }

                let behavior = swarm.behaviour_mut();

                // Remove Peer
                if let Some(peer) = behavior.pool.peers.remove(&peer_id) {
                    // Remove peer addresses from the DHT if they are present
                    let mut addresses: Vec<Multiaddr> = vec![];
                    if let Some(record) = behavior.pool.contacts.read().get(&peer_id) {
                        addresses.extend::<Vec<Multiaddr>>(record.addresses().cloned().collect());
                    }
                    for address in addresses {
                        behavior.remove_peer_address(peer_id, address);
                    }
                    events_tx.send(NetworkEvent::<Peer>::PeerLeft(peer)).ok();
                }
            }

            SwarmEvent::IncomingConnection {
                local_addr,
                send_back_addr,
            } => {
                tracing::debug!(
                    "Incoming connection from address {:?} to listen address {:?}",
                    send_back_addr,
                    local_addr
                );
            }

            SwarmEvent::IncomingConnectionError {
                local_addr,
                send_back_addr,
                error,
            } => {
                tracing::debug!(
                    "Incoming connection error from address {:?} to listen address {:?}: {:?}",
                    send_back_addr,
                    local_addr,
                    error
                );
            }

            SwarmEvent::Dialing(peer_id) => {
                // This event is only triggered if the network behaviour performs the dial
                tracing::debug!("Dialing peer {}", peer_id);
            }

            SwarmEvent::Behaviour(event) => {
                match event {
                    NimiqEvent::Dht(event) => {
                        match event {
                            KademliaEvent::OutboundQueryCompleted { id, result, .. } => {
                                match result {
                                    QueryResult::GetRecord(result) => {
                                        if let Some(output) = state.dht_gets.remove(&id) {
                                            let result = result.map_err(Into::into).map(
                                                |GetRecordOk { mut records, .. }| {
                                                    // TODO: What do we do, if we get multiple records?
                                                    records.pop().map(|r| r.record.value)
                                                },
                                            );
                                            output.send(result).ok();
                                        } else {
                                            tracing::warn!(query_id = ?id, "GetRecord query result for unknown query ID");
                                        }
                                    }
                                    QueryResult::PutRecord(result) => {
                                        // dht_put resolved
                                        if let Some(output) = state.dht_puts.remove(&id) {
                                            output
                                                .send(result.map(|_| ()).map_err(Into::into))
                                                .ok();
                                        } else {
                                            tracing::warn!(query_id = ?id, "PutRecord query result for unknown query ID");
                                        }
                                    }
                                    QueryResult::Bootstrap(result) => match result {
                                        Ok(result) => {
                                            tracing::debug!(result = ?result, "DHT bootstrap successful")
                                        }
                                        Err(e) => tracing::error!("DHT bootstrap error: {:?}", e),
                                    },
                                    _ => {}
                                }
                            }
                            KademliaEvent::InboundRequest {
                                request:
                                    InboundRequest::PutRecord {
                                        source: _,
                                        connection: _,
                                        record: Some(record),
                                    },
                            } => {
                                if let Ok(compressed_pk) =
                                    <[u8; 285]>::try_from(record.key.as_ref())
                                {
                                    if let Ok(pk) = (CompressedPublicKey {
                                        public_key: compressed_pk,
                                    })
                                    .uncompress()
                                    {
                                        if let Ok(signed_record) =
                                            SignedValidatorRecord::<PeerId>::deserialize_from_vec(
                                                &record.value,
                                            )
                                        {
                                            if signed_record.verify(&pk) {
                                                if swarm
                                                    .behaviour_mut()
                                                    .dht
                                                    .store_mut()
                                                    .put(record)
                                                    .is_ok()
                                                {
                                                    return;
                                                } else {
                                                    log::error!("Could not store record in DHT record store");
                                                    return;
                                                };
                                            } else {
                                                log::warn!("DHT record signature verification failed. Record public key: {:?}", pk);
                                                return;
                                            }
                                        }
                                    }
                                }
                                log::warn!(
                                    "DHT record verification failed: Invalid public key received"
                                );
                            }
                            _ => {}
                        }
                    }
                    NimiqEvent::Discovery(_e) => {}
                    NimiqEvent::Gossip(event) => match event {
                        GossipsubEvent::Message {
                            propagation_source,
                            message_id,
                            message,
                        } => {
                            if let Some(topic_info) = state.gossip_topics.get_mut(&message.topic) {
                                let (output, validate) = topic_info;
                                if !&*validate {
                                    swarm
                                        .behaviour_mut()
                                        .gossipsub
                                        .report_message_validation_result(
                                            &message_id,
                                            &propagation_source,
                                            MessageAcceptance::Accept,
                                        )
                                        .ok();
                                }

                                let topic = message.topic.clone();
                                if let Err(e) =
                                    output.try_send((message, message_id, propagation_source))
                                {
                                    tracing::error!(
                                        "Failed to dispatch gossipsub '{}' message: {:?}",
                                        topic.as_str(),
                                        e
                                    )
                                }
                            } else {
                                tracing::warn!(topic = ?message.topic, "unknown topic hash");
                            }
                        }
                        GossipsubEvent::Subscribed { peer_id, topic } => {
                            tracing::debug!(peer_id = ?peer_id, topic = ?topic, "peer subscribed to topic");
                        }
                        GossipsubEvent::Unsubscribed { peer_id, topic } => {
                            tracing::debug!(peer_id = ?peer_id, topic = ?topic, "peer unsubscribed");
                        }
                        GossipsubEvent::GossipsubNotSupported { peer_id } => {
                            tracing::debug!(peer_id = ?peer_id, "gossipsub not supported");
                        }
                    },
                    NimiqEvent::Identify(event) => {
                        match event {
                            IdentifyEvent::Received { peer_id, info } => {
                                tracing::debug!(
                                    "Received identity from peer {} at address {:?}: {:?}",
                                    peer_id,
                                    info.observed_addr,
                                    info
                                );

                                // Save identified peer listen addresses
                                for listen_addr in info.listen_addrs {
                                    swarm.behaviour_mut().add_peer_address(peer_id, listen_addr);

                                    // Bootstrap Kademlia if we're adding our first address
                                    if !state.is_bootstraped {
                                        log::debug!("Bootstrapping DHT");
                                        if swarm.behaviour_mut().dht.bootstrap().is_err() {
                                            tracing::error!(
                                                "Bootstrapping DHT error: No known peers"
                                            );
                                        }
                                        state.is_bootstraped = true;
                                    }
                                }
                            }
                            IdentifyEvent::Pushed { peer_id } => {
                                tracing::trace!("Pushed identity to peer {}", peer_id);
                            }
                            IdentifyEvent::Sent { peer_id } => {
                                tracing::trace!("Sent identity to peer {}", peer_id);
                            }
                            IdentifyEvent::Error { peer_id, error } => {
                                tracing::error!(
                                    "Error while identifying remote peer {}: {:?}",
                                    peer_id,
                                    error
                                );
                            }
                        }
                    }
                    NimiqEvent::Ping(event) => {
                        match event.result {
                            Err(e) => {
                                tracing::error!("Ping failed with peer {}, {:?}", event.peer, e);
                                // Remove the peer from the peer map
                                if let Some(peer) =
                                    swarm.behaviour_mut().pool.peers.remove(&event.peer)
                                {
                                    events_tx.send(NetworkEvent::<Peer>::PeerLeft(peer)).ok();
                                }
                            }
                            Ok(Success::Pong) => {
                                tracing::trace!("Responded Ping from peer {}", event.peer);
                            }
                            Ok(Success::Ping { rtt }) => {
                                tracing::trace!(
                                    "Sent Ping and received response to/from peer {}, round trip time {:?}",
                                    event.peer,
                                    rtt
                                );
                            }
                        };
                    }
                    NimiqEvent::Pool(event) => {
                        match event {
                            ConnectionPoolEvent::PeerJoined { peer } => {
                                events_tx.send(NetworkEvent::<Peer>::PeerJoined(peer)).ok();
                            }
                        };
                    }
                }
            }
            _ => {}
        }
    }

    fn perform_action(action: NetworkAction, swarm: &mut NimiqSwarm, state: &mut TaskState) {
        // FIXME implement compact debug format for NetworkAction
        // tracing::trace!(action = ?action, "performing action");

        match action {
            NetworkAction::Dial { peer_id, output } => {
                output
                    .send(
                        Swarm::dial(swarm, DialOpts::peer_id(peer_id).build()).map_err(Into::into),
                    )
                    .ok();
            }
            NetworkAction::DialAddress { address, output } => {
                output
                    .send(
                        Swarm::dial(swarm, DialOpts::unknown_peer_id().address(address).build())
                            .map_err(Into::into),
                    )
                    .ok();
            }
            NetworkAction::DhtGet { key, output } => {
                let query_id = swarm
                    .behaviour_mut()
                    .dht
                    .get_record(key.into(), Quorum::One);
                state.dht_gets.insert(query_id, output);
            }
            NetworkAction::DhtPut { key, value, output } => {
                let local_peer_id = Swarm::local_peer_id(swarm);

                let record = Record {
                    key: key.into(),
                    value,
                    publisher: Some(*local_peer_id),
                    expires: None, // TODO: Records should expire at some point in time
                };

                match swarm.behaviour_mut().dht.put_record(record, Quorum::One) {
                    Ok(query_id) => {
                        // Remember put operation to resolve when we receive a `QueryResult::PutRecord`
                        state.dht_puts.insert(query_id, output);
                    }
                    Err(e) => {
                        output.send(Err(e.into())).ok();
                    }
                }
            }
            NetworkAction::Subscribe {
                topic_name,
                buffer_size,
                validate,
                output,
            } => {
                let topic = IdentTopic::new(topic_name);

                match swarm.behaviour_mut().gossipsub.subscribe(&topic) {
                    // New subscription. Insert the sender into our subscription table.
                    Ok(true) => {
                        let (tx, rx) = mpsc::channel(buffer_size);

                        state.gossip_topics.insert(topic.hash(), (tx, validate));

                        match swarm
                            .behaviour_mut()
                            .gossipsub
                            .set_topic_params(topic, TopicScoreParams::default())
                        {
                            Ok(_) => output.send(Ok(rx)).ok(),
                            Err(e) => output
                                .send(Err(NetworkError::TopicScoreParams {
                                    topic_name,
                                    error: e,
                                }))
                                .ok(),
                        };
                    }

                    // Apparently we're already subscribed.
                    Ok(false) => {
                        output
                            .send(Err(NetworkError::AlreadySubscribed { topic_name }))
                            .ok();
                    }

                    // Subscribe failed. Send back error.
                    Err(e) => {
                        output.send(Err(e.into())).ok();
                    }
                }
            }
            NetworkAction::Unsubscribe { topic_name, output } => {
                let topic = IdentTopic::new(topic_name);

                if state.gossip_topics.get_mut(&topic.hash()).is_some() {
                    match swarm.behaviour_mut().gossipsub.unsubscribe(&topic) {
                        // Unsubscription. Remove the topic from the subscription table.
                        Ok(true) => {
                            drop(state.gossip_topics.remove(&topic.hash()).unwrap().0);

                            output.send(Ok(())).ok();
                        }

                        // Apparently we're already unsubscribed.
                        Ok(false) => {
                            drop(state.gossip_topics.remove(&topic.hash()).unwrap().0);

                            output
                                .send(Err(NetworkError::AlreadyUnsubscribed { topic_name }))
                                .ok();
                        }

                        // Unsubscribe failed. Send back error.
                        Err(e) => {
                            output.send(Err(e.into())).ok();
                        }
                    }
                } else {
                    // If the topic wasn't in the topics list, we're not subscribed to it.
                    output
                        .send(Err(NetworkError::AlreadyUnsubscribed { topic_name }))
                        .ok();
                }
            }
            NetworkAction::Publish {
                topic_name,
                data,
                output,
            } => {
                let topic = IdentTopic::new(topic_name);

                output
                    .send(
                        swarm
                            .behaviour_mut()
                            .gossipsub
                            .publish(topic, data)
                            .map_err(Into::into),
                    )
                    .ok();
            }
            NetworkAction::NetworkInfo { output } => {
                output.send(Swarm::network_info(swarm)).ok();
            }
            NetworkAction::Validate {
                message_id,
                source,
                acceptance,
                output,
            } => {
                output
                    .send(
                        swarm
                            .behaviour_mut()
                            .gossipsub
                            .report_message_validation_result(&message_id, &source, acceptance)
                            .map_err(Into::into),
                    )
                    .ok();
            }
            NetworkAction::ReceiveFromAll { type_id, output } => {
                swarm.behaviour_mut().pool.receive_from_all(type_id, output);
            }
            NetworkAction::ListenOn { listen_addresses } => {
                for listen_address in listen_addresses {
                    Swarm::listen_on(swarm, listen_address)
                        .expect("Failed to listen on provided address");
                }
            }
            NetworkAction::StartConnecting => {
                swarm.behaviour_mut().pool.start_connecting();
            }
        }
    }

    pub async fn network_info(&self) -> Result<NetworkInfo, NetworkError> {
        let (output_tx, output_rx) = oneshot::channel();

        self.action_tx
            .clone()
            .send(NetworkAction::NetworkInfo { output: output_tx })
            .await?;
        Ok(output_rx.await?)
    }

    pub async fn listen_on(&self, listen_addresses: Vec<Multiaddr>) {
        self.action_tx
            .clone()
            .send(NetworkAction::ListenOn { listen_addresses })
            .await
            .map_err(|e| tracing::error!("Failed to send NetworkAction::ListenOnAddress: {:?}", e))
            .ok();
    }

    pub async fn start_connecting(&self) {
        self.action_tx
            .clone()
            .send(NetworkAction::StartConnecting)
            .await
            .map_err(|e| tracing::error!("Failed to send NetworkAction::StartConnecting: {:?}", e))
            .ok();
    }
}

#[async_trait]
impl NetworkInterface for Network {
    type PeerType = Peer;
    type AddressType = Multiaddr;
    type Error = NetworkError;
    type PubsubId = GossipsubId<PeerId>;

    fn get_peer_updates(
        &self,
    ) -> (
        Vec<Arc<Self::PeerType>>,
        BroadcastStream<NetworkEvent<Self::PeerType>>,
    ) {
        self.peers.subscribe()
    }

    fn get_peers(&self) -> Vec<Arc<Self::PeerType>> {
        self.peers.get_peers()
    }

    fn get_peer(&self, peer_id: PeerId) -> Option<Arc<Self::PeerType>> {
        self.peers.get_peer(&peer_id)
    }

    fn subscribe_events(&self) -> BroadcastStream<NetworkEvent<Self::PeerType>> {
        BroadcastStream::new(self.events_tx.subscribe())
    }

    /// Implements `receive_from_all`, but instead of selecting over all peer message streams, we register a channel in
    /// the network. The sender is copied to new peers when they're instantiated.
    fn receive_from_all<'a, T: Message>(&self) -> BoxStream<'a, (T, Arc<Peer>)> {
        let mut action_tx = self.action_tx.clone();

        // Future to register the channel.
        let register_future = async move {
            let (tx, rx) = mpsc::channel(0);

            action_tx
                .send(NetworkAction::ReceiveFromAll {
                    type_id: T::TYPE_ID.into(),
                    output: tx,
                })
                .await
                .expect("Sending action to network task failed.");

            rx
        };

        // XXX Drive the register future to completion. This is needed because we want the receivers
        // to be properly set up when this function returns. It should be ok to block here as we're
        // only calling this during client initialization.
        // A better way to do this would be make receive_from_all() async.
        let receive_stream = executor::block_on(register_future);

        receive_stream
            .filter_map(|(data, peer)| async move {
                // Map the (data, peer) stream to (message, peer) by deserializing the messages.
                match <T as Deserialize>::deserialize(&mut data.reader()) {
                    Ok(message) => Some((message, peer)),
                    Err(e) => {
                        tracing::error!(
                            "Failed to deserialize {} message from {}: {}",
                            std::any::type_name::<T>(),
                            peer.id(),
                            e
                        );
                        None
                    }
                }
            })
            .boxed()
    }

    async fn subscribe<'a, T>(
        &self,
    ) -> Result<BoxStream<'a, (T::Item, Self::PubsubId)>, Self::Error>
    where
        T: Topic + Sync,
    {
        let (tx, rx) = oneshot::channel();

        self.action_tx
            .clone()
            .send(NetworkAction::Subscribe {
                topic_name: <T as Topic>::NAME,
                buffer_size: <T as Topic>::BUFFER_SIZE,
                validate: <T as Topic>::VALIDATE,
                output: tx,
            })
            .await?;

        // Receive the mpsc::Receiver, but propagate errors first.
        let subscribe_rx = rx.await??;

        Ok(Box::pin(subscribe_rx.map(|(msg, msg_id, source)| {
            let item: <T as Topic>::Item = Deserialize::deserialize_from_vec(&msg.data).unwrap();
            let id = GossipsubId {
                message_id: msg_id,
                propagation_source: source,
            };
            (item, id)
        })))
    }

    async fn unsubscribe<'a, T>(&self) -> Result<(), Self::Error>
    where
        T: Topic + Sync,
    {
        let (output_tx, output_rx) = oneshot::channel();

        self.action_tx
            .clone()
            .send(NetworkAction::Unsubscribe {
                topic_name: <T as Topic>::NAME,
                output: output_tx,
            })
            .await?;

        output_rx.await?
    }

    async fn publish<T>(&self, item: <T as Topic>::Item) -> Result<(), Self::Error>
    where
        T: Topic + Sync,
    {
        let (output_tx, output_rx) = oneshot::channel();

        let mut buf = vec![];
        item.serialize(&mut buf)?;

        self.action_tx
            .clone()
            .send(NetworkAction::Publish {
                topic_name: <T as Topic>::NAME,
                data: buf,
                output: output_tx,
            })
            .await?;

        let _message_id = output_rx.await??;

        Ok(())
    }

    fn validate_message<T>(&self, pubsub_id: Self::PubsubId, acceptance: MsgAcceptance)
    where
        T: Topic + Sync,
    {
        self.validate_tx
            .unbounded_send(ValidateMessage::new::<T>(pubsub_id, acceptance))
            .expect("Failed to send reported message validation result");
    }

    async fn dht_get<K, V>(&self, k: &K) -> Result<Option<V>, Self::Error>
    where
        K: AsRef<[u8]> + Send + Sync,
        V: Deserialize + Send + Sync,
    {
        let (output_tx, output_rx) = oneshot::channel();
        self.action_tx
            .clone()
            .send(NetworkAction::DhtGet {
                key: k.as_ref().to_owned(),
                output: output_tx,
            })
            .await?;

        if let Some(data) = output_rx.await?? {
            Ok(Some(Deserialize::deserialize_from_vec(&data)?))
        } else {
            Ok(None)
        }
    }

    async fn dht_put<K, V>(&self, k: &K, v: &V) -> Result<(), Self::Error>
    where
        K: AsRef<[u8]> + Send + Sync,
        V: Serialize + Send + Sync,
    {
        let (output_tx, output_rx) = oneshot::channel();

        let mut buf = vec![];
        v.serialize(&mut buf)?;

        self.action_tx
            .clone()
            .send(NetworkAction::DhtPut {
                key: k.as_ref().to_owned(),
                value: buf,
                output: output_tx,
            })
            .await?;
        output_rx.await?
    }

    async fn dial_peer(&self, peer_id: PeerId) -> Result<(), NetworkError> {
        let (output_tx, output_rx) = oneshot::channel();
        self.action_tx
            .clone()
            .send(NetworkAction::Dial {
                peer_id,
                output: output_tx,
            })
            .await?;
        output_rx.await?
    }

    async fn dial_address(&self, address: Multiaddr) -> Result<(), NetworkError> {
        let (output_tx, output_rx) = oneshot::channel();
        self.action_tx
            .clone()
            .send(NetworkAction::DialAddress {
                address,
                output: output_tx,
            })
            .await?;
        output_rx.await?
    }

    fn get_local_peer_id(&self) -> <Self::PeerType as PeerInterface>::Id {
        self.local_peer_id
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use futures::{Stream, StreamExt};
    use libp2p::{
        gossipsub::GossipsubConfigBuilder,
        identity::Keypair,
        multiaddr::{multiaddr, Multiaddr},
        swarm::KeepAlive,
        PeerId,
    };
    use rand::{thread_rng, Rng};

    use beserial::{Deserialize, Serialize};
    use nimiq_network_interface::network::{MsgAcceptance, NetworkEvent, Topic};
    use nimiq_network_interface::{
        message::Message,
        network::Network as NetworkInterface,
        peer::{CloseReason, Peer as PeerInterface},
    };
    use nimiq_utils::time::OffsetTime;

    use crate::{
        discovery::{
            behaviour::DiscoveryConfig,
            peer_contacts::{PeerContact, Protocols, Services},
        },
        peer::Peer,
    };

    use super::{Config, Network};

    #[derive(Clone, Debug, Deserialize, Serialize)]
    struct TestMessage {
        id: u32,
    }

    impl Message for TestMessage {
        const TYPE_ID: u64 = 42;
    }

    #[derive(Clone, Debug, Deserialize, Serialize)]
    struct TestMessage2 {
        #[beserial(len_type(u8))]
        x: String,
    }

    impl Message for TestMessage2 {
        const TYPE_ID: u64 = 43;
    }

    fn network_config(address: Multiaddr) -> Config {
        let keypair = Keypair::generate_ed25519();

        let mut peer_contact = PeerContact {
            addresses: vec![address],
            public_key: keypair.public(),
            services: Services::all(),
            timestamp: None,
        };
        peer_contact.set_current_time();

        let gossipsub = GossipsubConfigBuilder::default()
            .validation_mode(libp2p::gossipsub::ValidationMode::Permissive)
            .build()
            .expect("Invalid Gossipsub config");

        Config {
            keypair,
            peer_contact,
            seeds: Vec::new(),
            discovery: DiscoveryConfig {
                genesis_hash: Default::default(),
                update_interval: Duration::from_secs(60),
                min_recv_update_interval: Duration::from_secs(30),
                update_limit: 64,
                protocols_filter: Protocols::all(),
                services_filter: Services::all(),
                min_send_update_interval: Duration::from_secs(30),
                house_keeping_interval: Duration::from_secs(60),
                keep_alive: KeepAlive::No,
            },
            kademlia: Default::default(),
            gossipsub,
        }
    }

    fn assert_peer_joined(event: &NetworkEvent<Peer>, peer_id: &PeerId) {
        if let NetworkEvent::PeerJoined(peer) = event {
            assert_eq!(&peer.id, peer_id);
        } else {
            panic!("Event is not a NetworkEvent::PeerJoined: {:?}", event);
        }
    }

    fn assert_peer_left(event: &NetworkEvent<Peer>, peer_id: &PeerId) {
        if let NetworkEvent::PeerLeft(peer) = event {
            assert_eq!(&peer.id, peer_id);
        } else {
            panic!("Event is not a NetworkEvent::PeerLeft: {:?}", event);
        }
    }

    #[derive(Clone, Debug)]
    struct TestNetwork {
        next_address: u64,
        addresses: Vec<Multiaddr>,
    }

    impl TestNetwork {
        pub fn new() -> Self {
            Self {
                next_address: thread_rng().gen::<u64>(),
                addresses: vec![],
            }
        }

        pub async fn spawn(&mut self) -> Network {
            let address = multiaddr![Memory(self.next_address)];
            self.next_address += 1;

            let clock = Arc::new(OffsetTime::new());
            let net = Network::new(clock, network_config(address.clone())).await;
            net.listen_on(vec![address.clone()]).await;

            tracing::debug!(address = ?address, peer_id = ?net.local_peer_id, "creating node");

            if let Some(dial_address) = self.addresses.first() {
                let mut events = net.subscribe_events();

                tracing::debug!(address = ?dial_address, "dialing peer");
                net.dial_address(dial_address.clone()).await.unwrap();

                tracing::debug!("waiting for join event");
                let event = events.next().await;
                tracing::trace!(event = ?event);
            }

            self.addresses.push(address);

            net
        }

        pub async fn spawn_2() -> (Network, Network) {
            let mut net = Self::new();

            let net1 = net.spawn().await;
            let net2 = net.spawn().await;

            (net1, net2)
        }
    }

    async fn create_connected_networks() -> (Network, Network) {
        tracing::debug!("creating connected test networks:");
        let addr1 = multiaddr![Memory(thread_rng().gen::<u64>())];
        let addr2 = multiaddr![Memory(thread_rng().gen::<u64>())];

        let net1 = Network::new(Arc::new(OffsetTime::new()), network_config(addr1.clone())).await;
        net1.listen_on(vec![addr1.clone()]).await;

        let net2 = Network::new(Arc::new(OffsetTime::new()), network_config(addr2.clone())).await;
        net2.listen_on(vec![addr2.clone()]).await;

        tracing::debug!(address = ?addr1, peer_id = ?net1.local_peer_id, "Network 1");
        tracing::debug!(address = ?addr2, peer_id = ?net2.local_peer_id, "Network 2");

        let mut events1 = net1.subscribe_events();
        let mut events2 = net2.subscribe_events();

        tracing::debug!("dialing peer 1 from peer 2...");
        net2.dial_address(addr1).await.unwrap();

        tracing::debug!("waiting for join events");

        let event1 = events1.next().await.unwrap().unwrap();
        tracing::trace!(event1 = ?event1);
        assert_peer_joined(&event1, &net2.local_peer_id);

        let event2 = events2.next().await.unwrap().unwrap();
        tracing::trace!(event2 = ?event2);
        assert_peer_joined(&event2, &net1.local_peer_id);

        (net1, net2)
    }

    async fn create_network_with_n_peers(n_peers: usize) -> Vec<Network> {
        let mut networks = Vec::new();
        let mut addresses = Vec::new();
        let mut rng = rand::thread_rng();

        // Create all the networks and addresses
        for peer in 0..n_peers {
            let addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/{}/ws", 9000 + peer)
                .parse()
                .unwrap();

            tracing::debug!("Creating network: {}", peer);

            addresses.push(addr.clone());

            let network =
                Network::new(Arc::new(OffsetTime::new()), network_config(addr.clone())).await;
            network.listen_on(vec![addr.clone()]).await;

            tracing::debug!(address = ?addr, peer_id = ?network.local_peer_id, "Network {}",peer);
            networks.push(network);
        }

        // Connect them
        for peer in 1..n_peers {
            // Dial the previous peer
            tracing::debug!("Dialing Peer: {}", peer);
            networks[peer as usize]
                .dial_address(addresses[(peer - 1) as usize].clone())
                .await
                .unwrap();
        }

        let timeout = tokio::time::Duration::from_secs((n_peers * 2).try_into().unwrap());
        tokio::time::sleep(timeout).await;

        // Verify that each network has all the other peers connected
        for peer in 0..n_peers {
            assert_eq!(networks[peer as usize].get_peers().len(), n_peers - 1);
            assert_eq!(
                networks[peer as usize]
                    .network_info()
                    .await
                    .unwrap()
                    .num_peers(),
                n_peers - 1
            );
        }

        // Now disconnect and reconnect a random peer from all peers
        for peer in 0..n_peers {
            let network1 = &networks[peer as usize];
            let peer_id1 = network1.local_peer_id();
            let mut events1 = network1.subscribe_events();

            let mut close_peer = rng.gen_range(0..n_peers);
            while peer == close_peer {
                close_peer = rng.gen_range(0..n_peers);
            }
            let network2 = &networks[close_peer as usize];
            let peer_id2 = network2.local_peer_id();
            let mut events2 = network2.subscribe_events();

            // Verify that both networks have all the other peers connected
            assert_eq!(network1.get_peers().len(), n_peers - 1);
            assert_eq!(network2.get_peers().len(), n_peers - 1);
            assert_eq!(
                network1.network_info().await.unwrap().num_peers(),
                n_peers - 1
            );
            assert_eq!(
                network2.network_info().await.unwrap().num_peers(),
                n_peers - 1
            );

            // Disconnect a random peer
            tracing::debug!("Disconnecting peer {} from peer {}", close_peer, peer);
            let current_peer = network1.get_peer(*peer_id2).unwrap();
            current_peer.close(CloseReason::Other);

            // Assert the peer has left both networks
            let close_event1 = events1.next().await.unwrap().unwrap();
            assert_peer_left(&close_event1, peer_id2);
            drop(events1);

            let close_event2 = events2.next().await.unwrap().unwrap();
            assert_peer_left(&close_event2, peer_id1);
            drop(events2);

            // Verify that the networks lost a connection
            assert_eq!(network1.get_peers().len(), n_peers - 2);
            assert_eq!(network2.get_peers().len(), n_peers - 2);
            assert_eq!(
                network1.network_info().await.unwrap().num_peers(),
                n_peers - 2
            );
            assert_eq!(
                network2.network_info().await.unwrap().num_peers(),
                n_peers - 2
            );

            // Now reconnect the peer
            events1 = network1.subscribe_events();
            events2 = network2.subscribe_events();
            tracing::debug!("Reconnecting peer: {}", close_peer);
            network1
                .dial_address(addresses[close_peer as usize].clone())
                .await
                .unwrap();

            // Assert the peer rejoined the network
            let join_event1 = events1.next().await.unwrap().unwrap();
            assert_peer_joined(&join_event1, peer_id2);

            let join_event2 = events2.next().await.unwrap().unwrap();
            assert_peer_joined(&join_event2, peer_id1);

            // Verify all peers are connected again
            assert_eq!(network1.get_peers().len(), n_peers - 1);
            assert_eq!(network2.get_peers().len(), n_peers - 1);
            assert_eq!(
                network1.network_info().await.unwrap().num_peers(),
                n_peers - 1
            );
            assert_eq!(
                network2.network_info().await.unwrap().num_peers(),
                n_peers - 1
            );
        }

        networks
    }

    #[tokio::test]
    async fn connections_stress_and_reconnect() {
        // pretty_env_logger::init();
        // tracing_subscriber::fmt::init();

        let peers: usize = 15;
        let networks = create_network_with_n_peers(peers).await;

        assert_eq!(peers, networks.len());
    }

    #[tokio::test]
    async fn two_networks_can_connect() {
        let (net1, net2) = create_connected_networks().await;
        assert_eq!(net1.get_peers().len(), 1);
        assert_eq!(net2.get_peers().len(), 1);

        let peer2 = net1.get_peer(*net2.local_peer_id()).unwrap();
        let peer1 = net2.get_peer(*net1.local_peer_id()).unwrap();
        assert_eq!(peer2.id(), net2.local_peer_id);
        assert_eq!(peer1.id(), net1.local_peer_id);
    }

    #[tokio::test]
    async fn one_peer_can_talk_to_another() {
        let (net1, net2) = create_connected_networks().await;

        let peer2 = net1.get_peer(*net2.local_peer_id()).unwrap();
        let peer1 = net2.get_peer(*net1.local_peer_id()).unwrap();

        let mut msgs = peer1.receive::<TestMessage>();

        peer2.send(TestMessage { id: 4711 }).await.unwrap();

        tracing::debug!("send complete");

        let msg = msgs.next().await.unwrap();

        assert_eq!(msg.id, 4711);
    }

    #[tokio::test]
    async fn one_peer_can_send_multiple_messages() {
        // tracing_subscriber::fmt::init();

        let (net1, net2) = create_connected_networks().await;

        let peer2 = net1.get_peer(*net2.local_peer_id()).unwrap();
        let peer1 = net2.get_peer(*net1.local_peer_id()).unwrap();

        let mut msgs1 = peer1.receive::<TestMessage>();
        let mut msgs2 = peer1.receive::<TestMessage2>();

        peer2.send(TestMessage { id: 4711 }).await.unwrap();
        peer2
            .send(TestMessage2 {
                x: "foobar".to_string(),
            })
            .await
            .unwrap();

        tracing::debug!("send complete");

        let msg = msgs1.next().await.unwrap();
        assert_eq!(msg.id, 4711);

        let msg = msgs2.next().await.unwrap();
        assert_eq!(msg.x, "foobar");
    }

    #[tokio::test]
    async fn both_peers_can_talk_with_each_other() {
        let (net1, net2) = create_connected_networks().await;

        let peer2 = net1.get_peer(*net2.local_peer_id()).unwrap();
        let peer1 = net2.get_peer(*net1.local_peer_id()).unwrap();

        let mut in1 = peer1.receive::<TestMessage>();
        let mut in2 = peer2.receive::<TestMessage>();

        peer1.send(TestMessage { id: 1337 }).await.unwrap();
        peer2.send(TestMessage { id: 420 }).await.unwrap();

        let msg1 = in2.next().await.unwrap();
        let msg2 = in1.next().await.unwrap();

        assert_eq!(msg1.id, 1337);
        assert_eq!(msg2.id, 420);
    }

    #[tokio::test]
    async fn connections_are_properly_closed() {
        // tracing_subscriber::fmt::init();

        let (net1, net2) = create_connected_networks().await;

        let peer1 = net2.get_peer(*net1.local_peer_id()).unwrap();

        let mut events1 = net1.subscribe_events();
        let mut events2 = net2.subscribe_events();

        peer1.close(CloseReason::Other);
        tracing::debug!("closed peer");

        let event1 = events1.next().await.unwrap().unwrap();
        assert_peer_left(&event1, net2.local_peer_id());
        tracing::trace!(event1 = ?event1);

        let event2 = events2.next().await.unwrap().unwrap();
        assert_peer_left(&event2, net1.local_peer_id());
        tracing::trace!(event2 = ?event2);

        assert_eq!(net1.get_peers().len(), 0);
        assert_eq!(net2.get_peers().len(), 0);
    }

    #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
    pub struct TestRecord {
        x: i32,
    }

    #[tokio::test]
    async fn dht_put_and_get() {
        // tracing_subscriber::fmt::init();

        let (net1, net2) = create_connected_networks().await;

        // FIXME: Add delay while networks share their addresses
        tokio::time::sleep(Duration::from_secs(2)).await;

        let put_record = TestRecord { x: 420 };

        net1.dht_put(b"foo", &put_record).await.unwrap();

        let fetched_record = net2.dht_get::<_, TestRecord>(b"foo").await.unwrap();

        assert_eq!(fetched_record, Some(put_record));
    }

    pub struct TestTopic;

    impl Topic for TestTopic {
        type Item = TestRecord;

        const BUFFER_SIZE: usize = 8;
        const NAME: &'static str = "hello_world";
        const VALIDATE: bool = true;
    }

    fn consume_stream<T: std::fmt::Debug>(
        mut stream: impl Stream<Item = T> + Unpin + Send + 'static,
    ) {
        tokio::spawn(async move { while stream.next().await.is_some() {} });
    }

    // Currently does not make sense, as validate message does no longer
    // return if a message was still in the cache or not.
    #[ignore]
    #[tokio::test]
    async fn test_gossipsub() {
        // tracing_subscriber::fmt::init();

        let mut net = TestNetwork::new();

        let net1 = net.spawn().await;
        let net2 = net.spawn().await;

        // Our Gossipsub configuration requires a minimum of 6 peers for the mesh network
        for _ in 0..5i32 {
            let net_n = net.spawn().await;
            let stream_n = net_n.subscribe::<TestTopic>().await.unwrap();
            consume_stream(stream_n);
        }

        let test_message = TestRecord { x: 42 };

        let mut messages = net1.subscribe::<TestTopic>().await.unwrap();
        consume_stream(net2.subscribe::<TestTopic>().await.unwrap());

        tokio::time::sleep(Duration::from_secs(10)).await;

        net2.publish::<TestTopic>(test_message.clone())
            .await
            .unwrap();

        tracing::info!("Waiting for Gossipsub message...");
        let (received_message, message_id) = messages.next().await.unwrap();
        tracing::info!("Received Gossipsub message: {:?}", received_message);

        assert_eq!(received_message, test_message);

        // Make sure messages are validated before they are pruned from the memcache
        std::thread::sleep(Duration::from_millis(4500));
        net1.validate_message::<TestTopic>(message_id, MsgAcceptance::Accept);

        // Call the network_info async function after filling up a topic message buffer to verify that the
        // network drops messages without stalling it's functionality.
        for i in 0..10i32 {
            let msg = TestRecord { x: i };
            net2.publish::<TestTopic>(msg.clone()).await.unwrap();
        }
        net1.network_info().await.unwrap();
    }
}
