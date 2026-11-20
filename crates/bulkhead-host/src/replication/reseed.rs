//! Reseeding a stale pair node from the active disaster-recovery node
//! (ROADMAP DR4's failback), as a state machine over the node's log: the
//! node replaces its replicated buckets and its whole log with a snapshot
//! of the DR node's, then pulls the DR node's committed entries after it,
//! as the DR node itself pulls from the pair (`dr.rs`), from this one
//! source. Decided with the user on 2026-11-19: a full snapshot first,
//! so nothing the node held after the DR node's point survives, its commit
//! index included (spike S38: S20's rules only ever raise one), with no
//! truncation of a log in place.
//!
//! The DR node answers from its log with the same messages it answers the
//! pair with when it is the one asking (`ship.rs`), and counts how far
//! each pair node has come, from its asks; it hands back only once both
//! hold everything it committed.

use std::time::Duration;

use tokio::time::Instant;

use super::core::merge;
use super::log::{Log, Snapshot};
use super::ship::{DrMsg, MAX_BATCH_BYTES};
use crate::services::keyvalue::storage::BucketDump;

/// How long an answer is waited for before the ask is sent again.
const TIMEOUT: Duration = Duration::from_secs(2);

/// Between asks once caught up.
const POLL: Duration = Duration::from_millis(200);

/// What the replication task does after an input.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReseedEffect {
    /// Send to the DR node.
    Send(DrMsg),
    /// Tell the bucket's watches.
    Notify {
        bucket: String,
        changed: Vec<String>,
        revision: u64,
    },
}

/// Where the reseed is.
#[derive(Debug)]
enum Phase {
    /// Taking the DR node's snapshot, chunk by chunk; `index` 0 until its
    /// first chunk says which.
    Snapshot {
        index: u64,
        epoch: u64,
        next_chunk: u32,
        buckets: Vec<BucketDump>,
    },
    /// Pulling the entries after it.
    Entries,
}

/// A stale node's reseed from DR node `dr`.
#[derive(Debug)]
pub(crate) struct Reseed {
    dr: u32,
    phase: Phase,
    asked_at: Option<Instant>,
    next_ask: Instant,
    /// The DR node's commit index, from its last answer.
    source_commit: Option<u64>,
}

type Out = Result<Vec<ReseedEffect>, String>;

impl Reseed {
    /// Start over from a snapshot of `dr`'s buckets.
    pub(crate) fn new(dr: u32, now: Instant) -> Self {
        Self {
            dr,
            phase: Self::fresh(),
            asked_at: None,
            next_ask: now,
            source_commit: None,
        }
    }

    fn fresh() -> Phase {
        Phase::Snapshot {
            index: 0,
            epoch: 0,
            next_chunk: 0,
            buckets: Vec::new(),
        }
    }

    /// Whether it holds every entry the DR node had committed when it last
    /// answered.
    pub(crate) fn caught_up(&self, log: &Log) -> bool {
        matches!(self.phase, Phase::Entries) && self.source_commit.is_some_and(|c| log.last() >= c)
    }

    /// Ask again if an answer is overdue, or once a poll has passed.
    pub(crate) fn tick(&mut self, log: &Log, now: Instant) -> Out {
        let mut out = Vec::new();
        let due = match self.asked_at {
            Some(at) => now.duration_since(at) >= TIMEOUT,
            None => now >= self.next_ask,
        };
        if due {
            self.ask(log, now, &mut out);
        }
        Ok(out)
    }

    fn ask(&mut self, log: &Log, now: Instant, out: &mut Vec<ReseedEffect>) {
        self.asked_at = Some(now);
        let msg = match &self.phase {
            Phase::Snapshot {
                index, next_chunk, ..
            } => DrMsg::AskSnapshot {
                index: *index,
                chunk: *next_chunk,
            },
            Phase::Entries => DrMsg::Ask {
                after: log.last(),
                after_epoch: log.last_epoch(),
                max_bytes: MAX_BATCH_BYTES,
                // No lag target: the DR node warns of none for a reseed.
                rpo_target_ms: u64::MAX,
            },
        };
        out.push(ReseedEffect::Send(msg));
    }

    /// An answer from the DR node.
    pub(crate) fn answer(&mut self, log: &mut Log, msg: DrMsg, now: Instant) -> Out {
        let mut out = Vec::new();
        match msg {
            DrMsg::Snapshot {
                index,
                index_epoch,
                chunk,
                last,
                buckets,
            } => self.recv_snapshot(
                log,
                (index, index_epoch, chunk, last),
                buckets,
                now,
                &mut out,
            )?,
            DrMsg::Entries {
                commit,
                after,
                entries,
                ..
            } => {
                if !matches!(self.phase, Phase::Entries) || after != log.last() {
                    return Ok(out);
                }
                self.source_commit = Some(commit);
                self.asked_at = None;
                if !entries.is_empty() {
                    log.append(after + 1, &entries)?;
                    for c in log.commit_to(log.last())? {
                        if let Some((bucket, Ok(applied))) = c.write
                            && let Some(revision) = applied.revision
                        {
                            out.push(ReseedEffect::Notify {
                                bucket,
                                changed: applied.changed,
                                revision,
                            });
                        }
                    }
                }
                if log.last() < commit {
                    self.ask(log, now, &mut out);
                } else {
                    self.next_ask = now + POLL;
                }
            }
            DrMsg::Behind { commit, .. } | DrMsg::Mismatch { commit, .. } => {
                // Compacted past, or differing: a snapshot again.
                self.source_commit = Some(commit);
                self.phase = Self::fresh();
                self.ask(log, now, &mut out);
            }
            DrMsg::Ask { .. } | DrMsg::AskSnapshot { .. } | DrMsg::Deploys { .. } => {}
        }
        Ok(out)
    }

    fn recv_snapshot(
        &mut self,
        log: &mut Log,
        (index, index_epoch, chunk, last): (u64, u64, u32, bool),
        buckets: Vec<BucketDump>,
        now: Instant,
        out: &mut Vec<ReseedEffect>,
    ) -> Result<(), String> {
        let Phase::Snapshot {
            index: at,
            epoch,
            next_chunk,
            buckets: held,
        } = &mut self.phase
        else {
            return Ok(());
        };
        if chunk == 0 {
            *at = index;
            *epoch = index_epoch;
            *next_chunk = 0;
            held.clear();
        } else if index != *at || chunk != *next_chunk {
            return Ok(());
        }
        merge(held, buckets);
        *next_chunk += 1;
        self.asked_at = None;
        if !last {
            self.ask(log, now, out);
            return Ok(());
        }
        let Phase::Snapshot {
            index,
            epoch,
            buckets,
            ..
        } = std::mem::replace(&mut self.phase, Phase::Entries)
        else {
            return Ok(());
        };
        let before = log.snapshot().ok();
        let snap = Snapshot {
            index,
            epoch,
            buckets,
        };
        log.install(&snap)?;
        if let Some(before) = before {
            let mut notices = Vec::new();
            super::core::notify_differences(&before.buckets, &snap.buckets, &mut notices);
            out.extend(notices.into_iter().filter_map(|e| match e {
                super::core::Effect::Notify {
                    bucket,
                    changed,
                    revision,
                } => Some(ReseedEffect::Notify {
                    bucket,
                    changed,
                    revision,
                }),
                _ => None,
            }));
        }
        tracing::info!(
            dr = self.dr,
            index,
            "replication: reseeded from the DR node's snapshot; taking its entries after it"
        );
        self.ask(log, now, out);
        Ok(())
    }
}

#[cfg(test)]
#[path = "reseed_tests.rs"]
mod reseed_tests;
