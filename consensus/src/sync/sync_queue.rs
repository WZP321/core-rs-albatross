use std::cmp;
use std::cmp::Ordering;
use std::collections::binary_heap::PeekMut;
use std::collections::{BinaryHeap, VecDeque};
use std::fmt::Debug;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::Waker;

use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::task::{Context, Poll};
use futures::{ready, Future, Stream, StreamExt};

use nimiq_network_interface::peer::Peer;

use crate::consensus_agent::ConsensusAgent;

#[pin_project]
#[derive(Debug)]
struct OrderWrapper<TId, TOutput> {
    id: TId,
    #[pin]
    data: TOutput, // A future or a future's output
    index: usize,
    peer: usize,      // The peer the data is requested from
    num_tries: usize, // The number of tries this id has been requested
}

impl<TId: Clone, TOutput: Future> Future for OrderWrapper<TId, TOutput> {
    type Output = OrderWrapper<TId, TOutput::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let id = self.id.clone();
        let index = self.index;
        let peer = self.peer;
        let num_tries = self.num_tries;
        self.project().data.poll(cx).map(|output| OrderWrapper {
            id,
            data: output,
            index,
            peer,
            num_tries,
        })
    }
}

struct QueuedOutput<TOutput> {
    data: TOutput,
    index: usize,
}

impl<TOutput> PartialEq for QueuedOutput<TOutput> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}
impl<TOutput> Eq for QueuedOutput<TOutput> {}
impl<TOutput> PartialOrd for QueuedOutput<TOutput> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl<TOutput> Ord for QueuedOutput<TOutput> {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max heap, so compare backwards here.
        other.index.cmp(&self.index)
    }
}

pub struct SyncQueuePeer<TPeer: Peer> {
    pub(crate) peer_id: TPeer::Id,
    pub(crate) agent: Weak<ConsensusAgent<TPeer>>,
}

impl<TPeer: Peer> Clone for SyncQueuePeer<TPeer> {
    fn clone(&self) -> Self {
        Self {
            peer_id: self.peer_id.clone(),
            agent: self.agent.clone(),
        }
    }
}

/// The SyncQueue will request a list of ids from a set of peers
/// and implements an ordered stream over the resulting objects.
/// The stream returns an error if an id could not be resolved.
pub struct SyncQueue<TPeer: Peer, TId, TOutput> {
    pub(crate) peers: Vec<SyncQueuePeer<TPeer>>,
    desired_pending_size: usize,
    ids_to_request: VecDeque<TId>,
    pending_futures: FuturesUnordered<OrderWrapper<TId, BoxFuture<'static, Option<TOutput>>>>,
    queued_outputs: BinaryHeap<QueuedOutput<TOutput>>,
    next_incoming_index: usize,
    next_outgoing_index: usize,
    current_peer_index: usize,
    request_fn: fn(TId, Weak<ConsensusAgent<TPeer>>) -> BoxFuture<'static, Option<TOutput>>,
    waker: Option<Waker>,
}

impl<TPeer, TId, TOutput> SyncQueue<TPeer, TId, TOutput>
where
    TPeer: Peer,
    TId: Clone + Debug,
    TOutput: Send + Unpin,
{
    pub fn new(
        ids: Vec<TId>,
        peers: Vec<SyncQueuePeer<TPeer>>,
        desired_pending_size: usize,
        request_fn: fn(TId, Weak<ConsensusAgent<TPeer>>) -> BoxFuture<'static, Option<TOutput>>,
    ) -> Self {
        log::trace!(
            "Creating SyncQueue for {} with {} ids and {} peers",
            std::any::type_name::<TOutput>(),
            ids.len(),
            peers.len(),
        );

        SyncQueue {
            peers,
            desired_pending_size,
            ids_to_request: VecDeque::from(ids),
            pending_futures: FuturesUnordered::new(),
            queued_outputs: BinaryHeap::new(),
            next_incoming_index: 0,
            next_outgoing_index: 0,
            current_peer_index: 0,
            request_fn,
            waker: None,
        }
    }

    fn get_next_peer(&mut self, start_index: usize) -> Option<Weak<ConsensusAgent<TPeer>>> {
        while !self.peers.is_empty() {
            let index = start_index % self.peers.len();
            match Weak::upgrade(&self.peers[index].agent) {
                Some(peer) => {
                    return Some(Arc::downgrade(&peer));
                }
                None => {
                    self.peers.remove(index);
                }
            }
        }
        None
    }

    fn try_push_futures(&mut self) {
        // Determine number of new futures required to maintain desired_pending_size.
        let num_ids_to_request = cmp::min(
            self.ids_to_request.len(), // At most all of the ids
            // The number of pending futures can be higher than the desired pending size
            // (e.g., if there is an error and we re-request)
            self.desired_pending_size
                .saturating_sub(self.pending_futures.len() + self.queued_outputs.len()),
        );

        // Drain ids and produce futures.
        for _ in 0..num_ids_to_request {
            // Get next peer in line. Abort if there are no more peers.
            let peer = match self.get_next_peer(self.current_peer_index) {
                Some(peer) => peer,
                None => return,
            };

            let id = self.ids_to_request.pop_front().unwrap();

            log::trace!(
                "Requesting {:?} @ {} from peer {}",
                id,
                self.next_incoming_index,
                self.current_peer_index
            );

            let wrapper = OrderWrapper {
                data: (self.request_fn)(id.clone(), peer),
                id,
                index: self.next_incoming_index,
                peer: self.current_peer_index,
                num_tries: 1,
            };

            self.next_incoming_index += 1;
            self.current_peer_index = (self.current_peer_index + 1) % self.peers.len();

            self.pending_futures.push(wrapper);
        }

        if num_ids_to_request > 0 {
            log::trace!(
                "Requesting {} ids (ids_to_request={}, remaining_until_limit={}, pending_futures={}, queued_outputs={}, num_peers={})",
                num_ids_to_request,
                self.ids_to_request.len(),
                self.desired_pending_size
                    .saturating_sub(self.pending_futures.len() + self.queued_outputs.len()),
                self.pending_futures.len(),
                self.queued_outputs.len(),
                self.peers.len(),
            );

            if let Some(waker) = self.waker.take() {
                waker.wake();
            }
        }
    }

    pub fn add_peer(&mut self, peer_id: TPeer::Id, peer: Weak<ConsensusAgent<TPeer>>) {
        self.peers.push(SyncQueuePeer {
            peer_id,
            agent: peer,
        });
    }

    pub fn remove_peer(&mut self, peer_id: &TPeer::Id) {
        self.peers.retain(|element| element.peer_id != *peer_id)
    }

    pub fn has_peer(&self, peer_id: TPeer::Id) -> bool {
        self.peers.iter().any(|o_peer| o_peer.peer_id == peer_id)
    }

    pub fn add_ids(&mut self, ids: Vec<TId>) {
        for id in ids {
            self.ids_to_request.push_back(id);
        }

        // Adding new ids needs to wake the task that is polling the SyncQueue.
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    /// Truncates the stored ids, retaining only the first `len` elements.
    /// The elements are counted from the *original* start of the ids vector.
    pub fn truncate_ids(&mut self, len: usize) {
        self.ids_to_request
            .truncate(len.saturating_sub(self.next_incoming_index));
    }

    pub fn num_peers(&self) -> usize {
        self.peers.len()
    }

    pub fn len(&self) -> usize {
        self.ids_to_request.len() + self.pending_futures.len() + self.queued_outputs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn num_items_started(&self) -> usize {
        self.next_incoming_index
    }

    pub fn num_items_finished(&self) -> usize {
        self.next_outgoing_index
    }
}

impl<TPeer, TId, TOutput> Stream for SyncQueue<TPeer, TId, TOutput>
where
    TPeer: Peer,
    TId: Clone + Unpin + Debug,
    TOutput: Send + Unpin,
{
    type Item = Result<TOutput, TId>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        store_waker!(self, waker, cx);

        // Try to request more objects.
        self.try_push_futures();

        // Check to see if we've already received the next value.
        let this = &mut *self;
        if let Some(next_output) = this.queued_outputs.peek_mut() {
            if next_output.index == this.next_outgoing_index {
                this.next_outgoing_index += 1;
                return Poll::Ready(Some(Ok(PeekMut::pop(next_output).data)));
            }
        }

        loop {
            match ready!(self.pending_futures.poll_next_unpin(cx)) {
                Some(result) => {
                    match result.data {
                        Some(output) => {
                            if result.index == self.next_outgoing_index {
                                self.next_outgoing_index += 1;
                                return Poll::Ready(Some(Ok(output)));
                            } else {
                                self.queued_outputs.push(QueuedOutput {
                                    data: output,
                                    index: result.index,
                                });
                            }
                        }
                        None => {
                            // If we tried all peers for this hash, return an error.
                            // TODO max number of tries
                            if result.num_tries >= self.peers.len() {
                                return Poll::Ready(Some(Err(result.id)));
                            }

                            // Re-request from different peer. Return an error if there are no more peers.
                            let next_peer = (result.peer + 1) % self.peers.len();
                            let peer = match self.get_next_peer(next_peer) {
                                Some(peer) => peer,
                                None => return Poll::Ready(Some(Err(result.id))),
                            };

                            log::debug!(
                                "Re-requesting {:?} @ {} from peer {}",
                                result.id,
                                result.index,
                                next_peer
                            );

                            let wrapper = OrderWrapper {
                                data: (self.request_fn)(result.id.clone(), peer),
                                id: result.id,
                                index: result.index,
                                peer: next_peer,
                                num_tries: result.num_tries + 1,
                            };

                            self.pending_futures.push(wrapper);
                        }
                    }
                }
                None => {
                    return if self.ids_to_request.is_empty() || self.peers.is_empty() {
                        Poll::Ready(None)
                    } else {
                        self.try_push_futures();
                        Poll::Pending
                    }
                }
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.len();
        (len, Some(len))
    }
}
