use libp2p::{
    gossipsub::{GossipsubConfig, GossipsubConfigBuilder},
    identity::Keypair,
    kad::{KademliaBucketInserts, KademliaConfig, KademliaStoreInserts},
    Multiaddr,
};
use std::time::Duration;

use nimiq_hash::Blake2bHash;

use crate::discovery::{behaviour::DiscoveryConfig, peer_contacts::PeerContact};

pub struct Config {
    pub keypair: Keypair,
    pub peer_contact: PeerContact,
    pub seeds: Vec<Multiaddr>,
    pub discovery: DiscoveryConfig,
    pub kademlia: KademliaConfig,
    pub gossipsub: GossipsubConfig,
}

impl Config {
    pub fn new(
        keypair: Keypair,
        peer_contact: PeerContact,
        seeds: Vec<Multiaddr>,
        genesis_hash: Blake2bHash,
    ) -> Self {
        // Hardcoding the minimum number of peers in mesh network before adding more
        // TODO: Maybe change this to a mesh limits configuration argument of this function
        let gossipsub = GossipsubConfigBuilder::default()
            .mesh_n_low(3)
            .validate_messages()
            .max_transmit_size(1_000_000) // TODO find a reasonable value for this parameter
            .validation_mode(libp2p::gossipsub::ValidationMode::Permissive)
            .heartbeat_interval(Duration::from_millis(700))
            .max_ihave_length(10_000)
            .build()
            .expect("Invalid Gossipsub config");

        let mut kademlia = KademliaConfig::default();
        kademlia.set_kbucket_inserts(KademliaBucketInserts::OnConnected);
        kademlia.set_record_ttl(Some(Duration::from_secs(5 * 60)));
        kademlia.set_publication_interval(Some(Duration::from_secs(60)));

        // Since we have a record TTL of 5 minutes, record replication is not needed right now
        kademlia.set_replication_interval(None);
        kademlia.set_record_filtering(KademliaStoreInserts::FilterBoth);

        Self {
            keypair,
            peer_contact,
            seeds,
            discovery: DiscoveryConfig::new(genesis_hash),
            kademlia,
            gossipsub,
        }
    }
}
