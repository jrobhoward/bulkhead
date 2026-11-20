//! Shipping the replicated log to a disaster-recovery node (ROADMAP Phase
//! 8, DR2): the messages between it and the pair, and a pair node's side.
//!
//! The DR node pulls (`dr.rs`): it asks a pair node for the committed
//! entries after its own last one, or for a snapshot when those are
//! compacted. A pair node answers from its log and keeps nothing for the
//! DR node but a snapshot being sent and when it last found it caught up.
//! Only committed entries ship (spike S38, `spikes/s38/README.md`: an
//! entry shipped before it commits can be replaced by a failover, and the
//! DR node's copy would then contradict an acknowledged write). A
//! committed entry stays committed (S20's `CommittedStable`), so any pair
//! node may answer, whatever its standing: S38's `Ship(n)`.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use super::core::chunks;
use super::log::{Entry, Log};
use crate::services::keyvalue::storage::BucketDump;

/// Most entries in one answer.
const BATCH: usize = 1024;

/// The most entry bytes an answer carries, whatever the DR node asks:
/// well under the transport's frame cap.
pub(crate) const MAX_BATCH_BYTES: u64 = 4 << 20;

/// About the most bytes of a snapshot chunk.
pub(crate) const CHUNK_BYTES: usize = 1 << 20;

/// How long a snapshot chunk is waited for before it is asked again, if
/// the session with the node asked has not changed meanwhile. Frames
/// between two nodes go on one ordered stream that QUIC delivers whole, and
/// a frame from an ended session is discarded, so an ask is lost only when
/// the session ends: the asker asks again then, at once. On a live
/// session asking again only queues another copy of the chunk behind the
/// first: over DR5's drill's WAN (50 ms each way, 5% loss) a 1 MiB chunk
/// took 10 to 20 s, and chunks asked again every two seconds piled up
/// faster than they went, holding up the answers behind them. This is only
/// the backstop.
pub(crate) const CHUNK_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a snapshot being sent is kept after its last chunk was asked.
const SNAPSHOT_KEPT: Duration = Duration::from_secs(60);

/// Messages between a DR node and the pair's nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum DrMsg {
    /// From the DR node: the committed entries after `after`, whose entry
    /// is of epoch `after_epoch`, up to about `max_bytes` (0: only the
    /// commit index, a probe). `rpo_target_ms` is the DR node's lag
    /// target, for the pair node's own warning.
    Ask {
        after: u64,
        after_epoch: u64,
        max_bytes: u64,
        rpo_target_ms: u64,
    },
    /// Committed entries from `after + 1`, and the sender's commit index;
    /// `standby` if it is the pair's standby.
    Entries {
        standby: bool,
        commit: u64,
        after: u64,
        entries: Vec<Entry>,
    },
    /// The entries after `after` are compacted: catch up from a snapshot.
    Behind {
        standby: bool,
        commit: u64,
        after: u64,
    },
    /// From the DR node: chunk `chunk` of the snapshot at `index` (any
    /// snapshot, for chunk 0).
    AskSnapshot { index: u64, chunk: u32 },
    /// Chunk `chunk` of the buckets at commit index `index`, of epoch
    /// `index_epoch`.
    Snapshot {
        index: u64,
        index_epoch: u64,
        chunk: u32,
        last: bool,
        buckets: Vec<BucketDump>,
    },
    /// The entry at `after` is not of `after_epoch`: the DR node's copy and
    /// this node's committed log differ, so nothing ships from here.
    Mismatch { commit: u64, after: u64 },
    /// Deployments (ROADMAP DR3), either way: the node's own messages,
    /// passed to the channel it took ([`crate::Runtime::dr_deploys`]).
    Deploys { body: Vec<u8> },
}

/// A snapshot being sent to the DR node.
#[derive(Debug)]
struct Sending {
    index: u64,
    epoch: u64,
    chunks: Arc<Vec<Vec<BucketDump>>>,
    used_at: Instant,
}

/// A pair node's side of shipping.
#[derive(Debug)]
pub(crate) struct Shipper {
    sending: Option<Sending>,
    started: Instant,
    /// When an ask last found the DR node holding every entry this node
    /// had committed.
    caught_up_at: Option<Instant>,
    /// The DR node's lag target, from its asks.
    rpo_target: Option<Duration>,
    warned: bool,
}

impl Shipper {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            sending: None,
            started: now,
            caught_up_at: None,
            rpo_target: None,
            warned: false,
        }
    }

    /// How long since the DR node last held everything this node had
    /// committed (since this node started, if never).
    pub(crate) fn lag(&self, now: Instant) -> Duration {
        now.duration_since(self.caught_up_at.unwrap_or(self.started))
    }

    /// Answer a message from the DR node, from `log`.
    pub(crate) fn answer(
        &mut self,
        log: &Log,
        standby: bool,
        msg: DrMsg,
        now: Instant,
    ) -> Result<Option<DrMsg>, String> {
        let commit = log.commit();
        match msg {
            DrMsg::Ask {
                after,
                after_epoch,
                max_bytes,
                rpo_target_ms,
            } => {
                self.rpo_target = Some(Duration::from_millis(rpo_target_ms));
                if after >= commit {
                    self.caught_up_at = Some(now);
                }
                if after < log.base() {
                    return Ok(Some(DrMsg::Behind {
                        standby,
                        commit,
                        after,
                    }));
                }
                if after <= commit && log.epoch_at(after) != Some(after_epoch) {
                    tracing::error!(
                        index = after,
                        "replication: the DR node's copy differs from this node's committed log; \
                         not shipping to it"
                    );
                    return Ok(Some(DrMsg::Mismatch { commit, after }));
                }
                let mut entries = Vec::new();
                if max_bytes > 0 && after < commit {
                    let bytes =
                        usize::try_from(max_bytes.min(MAX_BATCH_BYTES)).unwrap_or(usize::MAX);
                    entries = log.entries(after + 1, BATCH, bytes)?;
                    entries.truncate(usize::try_from(commit - after).unwrap_or(usize::MAX));
                }
                Ok(Some(DrMsg::Entries {
                    standby,
                    commit,
                    after,
                    entries,
                }))
            }
            DrMsg::AskSnapshot { index, chunk } => {
                let kept = self
                    .sending
                    .as_ref()
                    .is_some_and(|s| s.index == index && chunk > 0);
                if !kept {
                    let snap = log.snapshot()?;
                    self.sending = Some(Sending {
                        index: snap.index,
                        epoch: snap.epoch,
                        chunks: Arc::new(chunks(snap.buckets, CHUNK_BYTES)),
                        used_at: now,
                    });
                }
                let Some(s) = self.sending.as_mut() else {
                    return Ok(None);
                };
                s.used_at = now;
                // A chunk asked of a snapshot this node no longer holds
                // starts the new one over.
                let chunk = if kept { chunk } else { 0 };
                let i = usize::try_from(chunk).unwrap_or(usize::MAX);
                Ok(Some(DrMsg::Snapshot {
                    index: s.index,
                    index_epoch: s.epoch,
                    chunk,
                    last: i + 1 >= s.chunks.len(),
                    buckets: s.chunks.get(i).cloned().unwrap_or_default(),
                }))
            }
            // The DR node's answers, not for a pair node.
            DrMsg::Entries { .. }
            | DrMsg::Behind { .. }
            | DrMsg::Snapshot { .. }
            | DrMsg::Mismatch { .. }
            // Routed to the node before the replication task (`deliver_dr`).
            | DrMsg::Deploys { .. } => Ok(None),
        }
    }

    /// Periodic work: drop a snapshot no longer asked for, and warn once
    /// while the DR node trails past its target.
    pub(crate) fn tick(&mut self, now: Instant) {
        if self
            .sending
            .as_ref()
            .is_some_and(|s| now.duration_since(s.used_at) >= SNAPSHOT_KEPT)
        {
            self.sending = None;
        }
        let Some(target) = self.rpo_target else {
            return;
        };
        let lag = self.lag(now);
        if lag > target && !self.warned {
            self.warned = true;
            tracing::warn!(
                lag_s = lag.as_secs(),
                target_s = target.as_secs(),
                "replication: the DR node trails this node past its target"
            );
        } else if lag <= target && self.warned {
            self.warned = false;
            tracing::info!(
                lag_s = lag.as_secs(),
                "replication: the DR node is back within its target"
            );
        }
    }
}

#[cfg(test)]
#[path = "ship_tests.rs"]
mod ship_tests;
