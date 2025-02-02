#![feature(ip)] // For IpAddr::is_global

#[macro_use]
extern crate beserial_derive;

#[macro_use]
extern crate nimiq_macros;

mod behaviour;
mod config;
mod connection_pool;
pub mod discovery;
pub mod dispatch;
mod error;
mod network;
pub mod peer;

pub const MESSAGE_PROTOCOL: &[u8] = b"/nimiq/message/0.0.1";
pub const DISCOVERY_PROTOCOL: &[u8] = b"/nimiq/discovery/0.0.1";

pub use libp2p::{self, identity::Keypair, swarm::NetworkInfo, Multiaddr, PeerId};

pub use config::Config;
pub use error::NetworkError;
pub use network::Network;
