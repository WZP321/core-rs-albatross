use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use bytes::Bytes;
use futures::{
    channel::{mpsc, oneshot},
    future::FutureExt,
    task::{Context, Poll, Waker},
};
use libp2p::{
    swarm::{
        ConnectionHandler, ConnectionHandlerEvent, ConnectionHandlerUpgrErr, KeepAlive,
        NegotiatedSubstream, SubstreamProtocol,
    },
    PeerId,
};
use thiserror::Error;

use beserial::SerializingError;
use nimiq_network_interface::{message::MessageType, peer::CloseReason};

use crate::dispatch::message_dispatch::MessageDispatch;
use crate::peer::Peer;

use super::protocol::MessageProtocol;

#[derive(Clone, Debug)]
pub enum HandlerInEvent {
    Close {
        reason: CloseReason,
    },
    PeerConnected {
        peer_id: PeerId,
        outbound: bool,
        receive_from_all: HashMap<MessageType, mpsc::Sender<(Bytes, Arc<Peer>)>>,
    },
}

#[derive(Clone, Debug)]
pub enum HandlerOutEvent {
    PeerJoined {
        peer: Arc<Peer>,
    },
    PeerLeft {
        peer_id: PeerId,
        reason: CloseReason,
    },
}

#[derive(Debug, Error)]
pub enum HandlerError {
    #[error("Serialization error: {0}")]
    Serializing(#[from] SerializingError),

    #[error("Connection closed: reason={:?}", {0})]
    ConnectionClosed { reason: CloseReason },
}

// TODO: Refactor state into enum
pub struct ConnectionPoolHandler {
    peer_id: Option<PeerId>,

    peer: Option<Arc<Peer>>,

    // Receives the close reason when `close()` is called on the peer.
    close_rx: Option<oneshot::Receiver<CloseReason>>,

    waker: Option<Waker>,

    events: VecDeque<ConnectionHandlerEvent<MessageProtocol, (), HandlerOutEvent, HandlerError>>,

    // The socket. This is only set after we negotiated the substream and before we instantiated the peer.
    socket: Option<MessageDispatch<NegotiatedSubstream>>,

    /// The sub-stream while we're polling it for closing.
    closing: Option<CloseReason>,

    // The global message receivers are stored here, until we create the MessageDispatch
    receive_from_all: Option<HashMap<MessageType, mpsc::Sender<(Bytes, Arc<Peer>)>>>,
}

impl ConnectionPoolHandler {
    pub fn new() -> Self {
        Self {
            peer_id: None,
            peer: None,
            close_rx: None,
            waker: None,
            events: VecDeque::new(),
            socket: None,
            closing: None,
            receive_from_all: None,
        }
    }

    fn wake(&self) {
        if let Some(waker) = &self.waker {
            waker.wake_by_ref();
        }
    }
}

impl ConnectionHandler for ConnectionPoolHandler {
    type InEvent = HandlerInEvent;
    type OutEvent = HandlerOutEvent;
    type Error = HandlerError;
    type InboundProtocol = MessageProtocol;
    type OutboundProtocol = MessageProtocol;
    type InboundOpenInfo = ();
    type OutboundOpenInfo = ();

    fn listen_protocol(&self) -> SubstreamProtocol<MessageProtocol, ()> {
        SubstreamProtocol::new(MessageProtocol::default(), ())
    }

    fn inject_fully_negotiated_inbound(
        &mut self,
        socket: MessageDispatch<NegotiatedSubstream>,
        _info: (),
    ) {
        log::trace!("inject_fully_negotiated_inbound");

        if self.peer.is_none() && self.socket.is_none() {
            self.socket = Some(socket);
            self.wake();
        } else {
            log::debug!("Connection already established. Ignoring inbound.");
        }
    }

    fn inject_fully_negotiated_outbound(
        &mut self,
        socket: MessageDispatch<NegotiatedSubstream>,
        _info: (),
    ) {
        log::trace!("inject_fully_negotiated_outbound");

        if self.peer.is_none() && self.socket.is_none() {
            self.socket = Some(socket);
            self.wake();
        } else {
            log::debug!("Connection already established. Ignoring outbound.");
        }
    }

    fn inject_event(&mut self, event: HandlerInEvent) {
        log::trace!("inject_event: {:?}", event);

        match event {
            HandlerInEvent::Close { reason } => {
                if let Some(peer) = &self.peer {
                    if self.closing.is_some() {
                        log::trace!("Socket closing pending");
                    } else {
                        log::debug!("ConnectionPoolHandler: Closing peer: {:?}", peer);
                        self.closing = Some(reason);
                        self.close_rx = None;
                    }
                }
            }
            HandlerInEvent::PeerConnected {
                peer_id,
                outbound,
                receive_from_all,
            } => {
                // Both peer_id and receive_from_all should not have been set yet.
                assert!(self.peer_id.is_none());
                assert!(self.receive_from_all.is_none());

                self.peer_id = Some(peer_id);
                self.receive_from_all = Some(receive_from_all);

                if outbound {
                    // Next open the outbound, but only if our connection is outbound
                    log::debug!("Requesting outbound substream to: {:?}", self.peer_id);

                    self.events
                        .push_back(ConnectionHandlerEvent::OutboundSubstreamRequest {
                            protocol: SubstreamProtocol::new(MessageProtocol::default(), ()),
                        });
                }

                self.wake();
            }
        }
    }

    fn inject_dial_upgrade_error(
        &mut self,
        _info: Self::OutboundOpenInfo,
        error: ConnectionHandlerUpgrErr<SerializingError>,
    ) {
        log::error!("Dial upgrade error: {}", error);
    }

    fn connection_keep_alive(&self) -> KeepAlive {
        KeepAlive::Yes
    }

    fn poll(
        &mut self,
        cx: &mut Context,
    ) -> Poll<ConnectionHandlerEvent<MessageProtocol, (), HandlerOutEvent, HandlerError>> {
        #[allow(clippy::never_loop)]
        loop {
            // Emit event
            if let Some(event) = self.events.pop_front() {
                return Poll::Ready(event);
            }

            if let Some(peer) = &self.peer {
                // Poll the oneshot receiver that signals us when the peer is closed
                if let Some(close_rx) = &mut self.close_rx {
                    match close_rx.poll_unpin(cx) {
                        Poll::Ready(Ok(reason)) => {
                            log::debug!("ConnectionPoolHandler: Closing peer: {:?}", peer);
                            self.closing = Some(reason);
                        }
                        Poll::Ready(Err(e)) => panic!("close_rx returned error: {}", e), // Channel was closed without message.
                        Poll::Pending => {}
                    }
                }

                // If we're currently closing the socket, call poll_close on it, until it finishes.
                if let Some(reason) = self.closing {
                    log::trace!("Polling socket to close: reason={:?}", reason);

                    match peer.poll_close(cx) {
                        Poll::Ready(Ok(())) => {
                            // Finished closing the socket
                            log::trace!("Finished closing socket");

                            let peer_id = peer.id;
                            self.closing = None;
                            self.peer = None;

                            // Gracefully close the connection
                            return Poll::Ready(ConnectionHandlerEvent::Custom(
                                HandlerOutEvent::PeerLeft { peer_id, reason },
                            ));
                        }
                        Poll::Ready(Err(e)) => {
                            // Error while closing. Log the error and emit the close event.
                            log::error!("Error while closing socket: {}", e);
                            return Poll::Ready(ConnectionHandlerEvent::Close(
                                HandlerError::ConnectionClosed { reason },
                            ));
                        }
                        Poll::Pending => {
                            log::trace!("Socket closing pending");
                            return Poll::Pending;
                        }
                    }
                }

                // Poll the socket for incoming messages
                match peer.poll_inbound(cx) {
                    Poll::Ready(Err(e)) => {
                        // Socket error
                        log::error!("{}", e);

                        return Poll::Ready(ConnectionHandlerEvent::Close(
                            HandlerError::ConnectionClosed {
                                reason: CloseReason::Error,
                            },
                        ));
                    }

                    Poll::Ready(Ok(())) => {
                        // The message stream ended.
                        log::debug!("Remote closed connection");

                        // Gracefully close the connection
                        return Poll::Ready(ConnectionHandlerEvent::Custom(
                            HandlerOutEvent::PeerLeft {
                                peer_id: peer.id,
                                reason: CloseReason::RemoteClosed,
                            },
                        ));
                    }

                    Poll::Pending => {}
                }

                // Send queued messages.
                if let Poll::Ready(Err(e)) = peer.poll_outbound(cx) {
                    log::error!("Error processing outbound messages: {}", e);

                    return Poll::Ready(ConnectionHandlerEvent::Close(
                        HandlerError::ConnectionClosed {
                            reason: CloseReason::Error,
                        },
                    ));
                }
            }

            // Wait for outbound and inbound to be established and the peer ID to be injected.
            if self.socket.is_none() || self.peer_id.is_none() {
                break;
            }

            assert!(self.peer.is_none());
            assert!(self.close_rx.is_none());
            assert!(self.receive_from_all.is_some());

            // Take inbound and outbound and create a peer from it.
            let peer_id = self.peer_id.unwrap();
            let mut socket = self.socket.take().unwrap();

            // Create a channel that is used to receive the close signal from the `Peer` struct (when `Peer::close` is called).
            let (close_tx, close_rx) = oneshot::channel();

            // Register the global message receivers with this message dispatch.
            let receive_from_all = self.receive_from_all.take().expect("global receivers");
            socket.receive_multiple_raw(receive_from_all);

            let peer = Arc::new(Peer::new(peer_id, socket, close_tx));
            log::debug!("New peer: {:?}", peer);

            self.close_rx = Some(close_rx);
            self.peer = Some(Arc::clone(&peer));

            // Send peer to behaviour
            return Poll::Ready(ConnectionHandlerEvent::Custom(
                HandlerOutEvent::PeerJoined { peer },
            ));
        }

        store_waker!(self, waker, cx);

        Poll::Pending
    }
}
