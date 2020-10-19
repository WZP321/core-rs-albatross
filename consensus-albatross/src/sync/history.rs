use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Weak};

use failure::_core::cmp::Ordering;
use futures::future::BoxFuture;
use futures::stream::{BoxStream, FuturesUnordered};
use futures::task::{Context, Poll};
use futures::{future, stream, Future, FutureExt, Stream, StreamExt};
use tokio::sync::broadcast;

use block_albatross::{Block, MacroBlock};
use blockchain_albatross::history_store;
use blockchain_albatross::history_store::ExtendedTransaction;
use blockchain_albatross::Blockchain;
use hash::Blake2bHash;
use network_interface::prelude::{Network, Peer};
use network_interface::request_response::RequestError;
use primitives::policy;

use crate::consensus_agent::ConsensusAgent;
use crate::messages::{Epoch as EpochInfo, HistoryChunk, RequestBlockHashesFilter};
use crate::sync::sync_queue::SyncQueue;
use crate::ConsensusEvent;

struct PendingEpoch {
    block: MacroBlock,
    history_len: usize,
    history: Vec<ExtendedTransaction>,
}
impl PendingEpoch {
    fn is_complete(&self) -> bool {
        self.history_len == self.history.len()
    }

    fn epoch_number(&self) -> u32 {
        policy::epoch_at(self.block.header.block_number)
    }
}

pub struct Epoch {
    block: MacroBlock,
    history: Vec<ExtendedTransaction>,
}

struct SyncCluster<TPeer: Peer> {
    epoch_ids: Vec<Blake2bHash>,
    epoch_offset: usize,

    epoch_queue: SyncQueue<TPeer, Blake2bHash, EpochInfo>,
    history_queue: SyncQueue<TPeer, (u32, usize), (u32, HistoryChunk)>,

    pending_epochs: VecDeque<PendingEpoch>,
}

impl<TPeer: Peer + 'static> SyncCluster<TPeer> {
    const NUM_PENDING_EPOCHS: usize = 5;
    const NUM_PENDING_CHUNKS: usize = 12;

    fn new(
        epoch_ids: Vec<Blake2bHash>,
        epoch_offset: usize,
        peers: Vec<Weak<ConsensusAgent<TPeer>>>,
    ) -> Self {
        let epoch_queue = SyncQueue::new(
            epoch_ids.clone(),
            peers.clone(),
            Self::NUM_PENDING_EPOCHS,
            |id, peer| async move { peer.request_epoch(id).await.ok() }.boxed(),
        );
        let history_queue = SyncQueue::new(
            Vec::<(u32, usize)>::new(),
            peers,
            Self::NUM_PENDING_CHUNKS,
            move |(epoch_number, chunk_index), peer| {
                async move {
                    peer.request_history_chunk(epoch_number, chunk_index)
                        .await
                        .ok()
                        .map(|chunk| (epoch_number, chunk))
                }
                .boxed()
            },
        );
        Self {
            epoch_ids,
            epoch_offset,
            epoch_queue,
            history_queue,
            pending_epochs: VecDeque::with_capacity(Self::NUM_PENDING_EPOCHS),
        }
    }

    fn on_epoch_received(&mut self, epoch: EpochInfo) {
        // TODO Verify macro blocks and their ordering

        // Queue history chunks for the given epoch for download.
        let block_number = epoch.block.header.block_number;
        let history_chunk_ids = (0..(epoch.history_len as usize / history_store::CHUNK_SIZE))
            .map(|i| (block_number, i))
            .collect();
        self.history_queue.add_ids(history_chunk_ids);

        // We keep the epoch in pending_epochs while the history is downloading.
        self.pending_epochs.push_back(PendingEpoch {
            block: epoch.block,
            history_len: epoch.history_len as usize,
            history: Vec::new(),
        });
    }

    fn on_history_chunk_received(&mut self, epoch_number: u32, history_chunk: HistoryChunk) {
        // Find epoch in pending_epochs.
        let first_epoch_number = self.pending_epochs[0].epoch_number();
        let epoch_index = (epoch_number - first_epoch_number) as usize;
        let epoch = &mut self.pending_epochs[epoch_index];

        // TODO This assumes that we have already filtered responses with no chunk.
        // Add the received history chunk to the pending epoch.
        let mut chunk = history_chunk.chunk.expect("History chunk missing").history;
        epoch.history.append(&mut chunk);
    }

    fn add_peer(&mut self, peer: Weak<ConsensusAgent<TPeer>>) {
        // TODO keep only one list of peers
        self.epoch_queue.add_peer(Weak::clone(&peer));
        self.history_queue.add_peer(peer);
    }

    fn split_off(&mut self, at: usize) -> Self {
        let ids = self.epoch_ids.split_off(at);
        let offset = self.epoch_offset + at;

        // Remove the split-off ids from our epoch queue.
        self.epoch_queue.truncate_ids(at);

        Self::new(ids, offset, self.epoch_queue.peers.clone())
    }
}

impl<TPeer: Peer + 'static> Stream for SyncCluster<TPeer> {
    type Item = Result<Epoch, ()>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.pending_epochs.len() < Self::NUM_PENDING_EPOCHS {
            while let Poll::Ready(Some(result)) = self.epoch_queue.poll_next_unpin(cx) {
                match result {
                    Ok(epoch) => self.on_epoch_received(epoch),
                    Err(_) => return Poll::Ready(Some(Err(()))), // TODO Error
                }
            }
        }

        while let Poll::Ready(Some(result)) = self.history_queue.poll_next_unpin(cx) {
            match result {
                Ok((epoch_number, history_chunk)) => {
                    self.on_history_chunk_received(epoch_number, history_chunk);

                    // Emit finished epochs.
                    if self.pending_epochs[0].is_complete() {
                        let epoch = self.pending_epochs.pop_front().unwrap();
                        let epoch = Epoch {
                            block: epoch.block,
                            history: epoch.history,
                        };
                        return Poll::Ready(Some(Ok(epoch)));
                    }
                }
                Err(_) => return Poll::Ready(Some(Err(()))), // TODO Error
            }
        }

        // We're done if there are no more epochs to process.
        if self.epoch_queue.is_empty() && self.pending_epochs.is_empty() {
            return Poll::Ready(None);
        }

        Poll::Pending
    }
}

impl<TPeer: Peer> PartialEq for SyncCluster<TPeer> {
    fn eq(&self, other: &Self) -> bool {
        self.epoch_offset == other.epoch_offset
            && self.epoch_queue.num_peers() == other.epoch_queue.num_peers()
            && self.epoch_ids == other.epoch_ids
    }
}
impl<TPeer: Peer> Eq for SyncCluster<TPeer> {}
impl<TPeer: Peer> PartialOrd for SyncCluster<TPeer> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(&other))
    }
}
impl<TPeer: Peer> Ord for SyncCluster<TPeer> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.epoch_offset
            .cmp(&other.epoch_offset) // Lower offset first
            .then_with(|| {
                other
                    .epoch_queue
                    .num_peers()
                    .cmp(&self.epoch_queue.num_peers())
            }) // Higher peer count first
            .then_with(|| other.epoch_ids.len().cmp(&self.epoch_ids.len())) // More ids first
            .then_with(|| self.epoch_ids.cmp(&other.epoch_ids)) //
            .reverse() // We want the best cluster to be *last*
    }
}

struct EpochIds<TPeer: Peer> {
    ids: Vec<Blake2bHash>,
    offset: usize,
    sender: Weak<ConsensusAgent<TPeer>>,
}

struct HistorySync<TNetwork: Network> {
    blockchain: Arc<Blockchain>,
    epoch_ids: BoxStream<'static, EpochIds<TNetwork::PeerType>>,
    sync_clusters: Vec<SyncCluster<TNetwork::PeerType>>,
}

impl<TNetwork: Network> HistorySync<TNetwork> {
    const CONCURRENT_HASH_REQUESTS: usize = 10;
    const MAX_CLUSTERS: usize = 100;

    pub fn new(
        consensus_event_rx: broadcast::Receiver<ConsensusEvent<TNetwork>>,
        blockchain: Arc<Blockchain>,
    ) -> Self {
        let blockchain1 = Arc::clone(&blockchain);
        let peer_stream = consensus_event_rx
            // We're only interested in PeerJoined events and the ConsensusAgent in it.
            .filter_map(|event| async {
                match event {
                    Ok(ConsensusEvent::PeerJoined(agent)) => Some(agent),
                    _ => None,
                }
            })
            // Request epoch ids from the new ConsensusAgent.
            .map(move |agent| {
                Self::request_epoch_ids(Arc::clone(&blockchain1), Arc::downgrade(&agent))
            })
            // Request concurrently.
            .buffer_unordered(Self::CONCURRENT_HASH_REQUESTS)
            // Only keep successful responses.
            .filter_map(|result| future::ready(result))
            .boxed();

        Self {
            blockchain,
            epoch_ids: peer_stream,
            sync_clusters: Vec::new(),
        }
    }

    async fn request_epoch_ids(
        blockchain: Arc<Blockchain>,
        weak_agent: Weak<ConsensusAgent<TNetwork::PeerType>>,
    ) -> Option<EpochIds<TNetwork::PeerType>> {
        let agent = match Weak::upgrade(&weak_agent) {
            Some(agent) => agent,
            None => return None,
        };

        let (locator, epoch_number) = {
            let election_head = blockchain.election_head();
            (
                election_head.hash(),
                policy::epoch_at(election_head.header.block_number),
            )
        };

        agent
            .request_block_hashes(
                vec![locator],
                1000, // TODO: Use other value
                RequestBlockHashesFilter::ElectionOnly,
            )
            .await
            .map(|block_hashes| EpochIds {
                ids: block_hashes.hashes,
                offset: epoch_number as usize + 1,
                sender: weak_agent,
            })
            .ok()
    }

    fn cluster_epoch_ids(&mut self, epoch_ids: EpochIds<TNetwork::PeerType>) {
        let mut id_index = 0;
        let mut new_clusters = Vec::new();

        for cluster in &mut self.sync_clusters {
            // Check if given epoch_ids and the current cluster potentially overlap.
            if cluster.epoch_offset <= epoch_ids.offset
                && cluster.epoch_offset + cluster.epoch_ids.len() > epoch_ids.offset
            {
                // Compare ids in the overlapping regions.
                let start_offset = epoch_ids.offset - cluster.epoch_offset;
                let len = usize::min(
                    cluster.epoch_ids.len() - start_offset,
                    epoch_ids.ids.len() - id_index,
                );
                let match_until = cluster.epoch_ids[start_offset..start_offset + len]
                    .iter()
                    .zip(&epoch_ids.ids[id_index..id_index + len])
                    .position(|(first, second)| first != second)
                    .unwrap_or(len);

                // If there is no match at all, skip to the next cluster.
                if match_until > 0 {
                    // If there is only a partial match, split the current cluster. The current cluster
                    // is truncated to the matching overlapping part and the removed ids are put in a new
                    // cluster. Buffer up the new clusters and insert them after we finish iterating over
                    // peer_clusters.
                    if match_until < len {
                        new_clusters.push(cluster.split_off(start_offset + match_until));
                    }

                    // The peer's epoch ids matched at least a part of this (now potentially truncated) cluster.
                    // Add the peer to the cluster and remove the matched ids by advancing id_index.
                    cluster.add_peer(Weak::clone(&epoch_ids.sender));
                    id_index += match_until;
                }
            }
        }

        // Add remaining ids to a new cluster with only the sending peer in it.
        if id_index < epoch_ids.ids.len() {
            new_clusters.push(SyncCluster::new(
                Vec::from(&epoch_ids.ids[id_index..]),
                epoch_ids.offset + id_index,
                vec![epoch_ids.sender],
            ));
        }

        // Add buffered clusters and sort them.
        self.sync_clusters.append(&mut new_clusters);
        self.sync_clusters.sort_unstable();
    }
}

impl<TNetwork: Network> Future for HistorySync<TNetwork> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Stop pulling in new epoch_ids if we hit a maximum a number of clusters to prevent DoS.
        if self.sync_clusters.len() < Self::MAX_CLUSTERS {
            while let Poll::Ready(Some(epoch_ids)) = self.epoch_ids.poll_next_unpin(cx) {
                self.cluster_epoch_ids(epoch_ids);
            }
        }

        // Poll the best cluster.
        // The best cluster is the last element in sync_clusters, so removing it is cheap.
        while !self.sync_clusters.is_empty() {
            let best_cluster = self.sync_clusters.last_mut().unwrap();
            let push_result = match ready!(best_cluster.poll_next_unpin(cx)) {
                Some(Ok(epoch)) => self
                    .blockchain
                    .push_history_sync(Block::Macro(epoch.block), &epoch.history)
                    .ok(),
                Some(Err(_)) | None => None,
            };
            // No PushResult means that either the cluster is finished or there was an error
            // retrieving or pushing an epoch. Evict current best cluster and move to next one.
            if push_result.is_none() {
                self.sync_clusters.pop();
            }
        }

        // FIXME Should probably never terminate. Turn into a stream instead to signal initial sync?
        Poll::Ready(())
    }
}
