//! The disaster-recovery node's copy of the pair's replicated log
//! (ROADMAP Phase 8, DR2), as a state machine: ticks and the pair's
//! answers change it and return what to send and which watches to tell,
//! which the task in `mod.rs` carries out. No I/O but the log's disk.
//!
//! It pulls ([`super::ship`]). Each round it asks both pair nodes after
//! its last entry: the one it pulls from for a batch, the other for its
//! commit index alone. It pulls from the node with the highest commit
//! index, the standby while it is within a batch of it, so the primary
//! does no more than answer probes. It asks again at once while more is
//! committed than it holds, and otherwise after `poll`. An answer missing
//! for long is asked again; after two misses it pulls from the other
//! node. When the entries it needs are compacted, it takes a snapshot,
//! chunk by chunk. It holds committed entries only, so its commit index
//! is its last entry (spike S38's `DrPrefix` and `DrNoConflict`).

use std::time::Duration;

use tokio::time::Instant;

use super::core::{Refusal, merge};
use super::log::{Command, Entry, Log, Snapshot};
use super::ship::{CHUNK_TIMEOUT, DrMsg, Shipper};
use crate::services::keyvalue::storage::{BucketDump, Op, Outcome};

/// The standby is pulled from while its commit index is within this many
/// entries of the best.
const NEAR: u64 = 256;

/// Tuning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DrTuning {
    /// The pair's nodes.
    pub(crate) pair: [u32; 2],
    /// Between rounds once caught up.
    pub(crate) poll: Duration,
    /// About the most entry bytes asked for at once.
    pub(crate) batch_bytes: u64,
    /// Warn while the copy trails the pair by longer than this.
    pub(crate) rpo_target: Duration,
    /// The log's bounds, as on the pair.
    pub(crate) log_bytes: u64,
    pub(crate) log_age: Duration,
}

/// What the task does after an input.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DrEffect {
    /// Send to pair node `to`.
    Send { to: u32, msg: DrMsg },
    /// Tell the bucket's watches.
    Notify {
        bucket: String,
        changed: Vec<String>,
        revision: u64,
    },
}

/// What the DR node knows of one pair node.
#[derive(Debug, Clone, Copy, Default)]
struct Source {
    commit: u64,
    standby: bool,
    heard: bool,
    /// An ask unanswered since.
    asked_at: Option<Instant>,
    misses: u32,
    /// Its committed log differs from this copy: never pulled from again.
    differs: bool,
}

/// A snapshot arriving.
#[derive(Debug)]
struct Receiving {
    from: usize,
    index: u64,
    epoch: u64,
    next_chunk: u32,
    buckets: Vec<BucketDump>,
    asked_at: Instant,
    misses: u32,
}

/// Where the copy stands (`bulkhead status`, metrics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct DrStatus {
    /// The pair node it pulls from.
    pub source: u32,
    /// The last entry it holds and applied.
    pub commit: u64,
    /// Compacted up to here.
    pub base: u64,
    /// The encoded size of the entries in its log.
    pub log_bytes: u64,
    /// How long since it last held everything the pair had committed (since
    /// it started, if never).
    pub lag: Duration,
    /// It is taking a snapshot.
    pub snapshotting: bool,
    /// It stopped: the pair was finalized at a version newer than this
    /// build, or both pair nodes' logs differ from its copy.
    pub stopped: bool,
    /// Activated (ROADMAP DR4): it leads the log at this epoch, takes
    /// writes, and pulls nothing from the pair.
    pub leading: Option<u64>,
    /// Handing back (DR4's failback): it takes no writes.
    pub frozen: bool,
    /// While it leads: the last entry each pair node holds as this copy
    /// does, from its asks as it reseeds (`None` until it asks after a
    /// snapshot).
    pub reseeded: [Option<u64>; 2],
}

/// The DR node's copy.
#[derive(Debug)]
pub(crate) struct DrReplica {
    cfg: DrTuning,
    log: Log,
    sources: [Source; 2],
    /// The index in `pair` of the node it pulls from.
    pulling: usize,
    next_round: Instant,
    receiving: Option<Receiving>,
    started: Instant,
    caught_up_at: Option<Instant>,
    warned: bool,
    stopped: bool,
    /// Activated: the epoch it leads at.
    leading: Option<u64>,
    /// Handing back: no writes.
    frozen: bool,
    /// Answers each pair node as it reseeds from it, while it leads: one
    /// each, so the two never replace the snapshot the other is taking.
    shippers: [Shipper; 2],
    /// How far each pair node has reseeded (see [`DrStatus::reseeded`]).
    reseeded: [Option<u64>; 2],
}

type Out = Result<Vec<DrEffect>, String>;

impl DrReplica {
    /// The copy kept in `log`. Entries past its commit index were
    /// committed on the pair (only those ship), so they are applied now.
    pub(crate) fn new(cfg: DrTuning, mut log: Log, now: Instant) -> Result<Self, String> {
        if log.commit() < log.last() {
            log.commit_to(log.last())?;
        }
        Ok(Self {
            cfg,
            log,
            sources: [Source::default(); 2],
            pulling: 0,
            next_round: now,
            receiving: None,
            started: now,
            caught_up_at: None,
            warned: false,
            stopped: false,
            leading: None,
            frozen: false,
            shippers: [Shipper::new(now), Shipper::new(now)],
            reseeded: [None; 2],
        })
    }

    /// How long a missing answer is waited for.
    fn timeout(&self) -> Duration {
        (self.cfg.poll * 4).max(Duration::from_secs(2))
    }

    pub(crate) fn status(&self, now: Instant) -> DrStatus {
        DrStatus {
            source: self.cfg.pair[self.pulling],
            commit: self.log.commit(),
            base: self.log.base(),
            log_bytes: self.log.bytes(),
            lag: self.lag(now),
            snapshotting: self.receiving.is_some(),
            stopped: self.stopped,
            leading: self.leading,
            frozen: self.frozen,
            reseeded: self.reseeded,
        }
    }

    /// Hand back (DR4's failback), first step: take no more writes. Its
    /// last entry, which both pair nodes must hold before the witness
    /// hands back. `None` unless it leads.
    pub(crate) fn freeze(&mut self) -> Option<u64> {
        self.leading?;
        self.frozen = true;
        Some(self.log.last())
    }

    /// Take writes again: a hand-back that did not happen.
    pub(crate) fn unfreeze(&mut self) {
        self.frozen = false;
    }

    /// Handed back: pull from the pair again, whose new primary continues
    /// this copy's log at a higher epoch.
    pub(crate) fn unlead(&mut self, now: Instant) {
        self.leading = None;
        self.frozen = false;
        self.reseeded = [None; 2];
        self.sources = [Source::default(); 2];
        self.caught_up_at = Some(now);
        self.next_round = now;
    }

    /// How long it has trailed the pair; nothing once it leads.
    pub(crate) fn lag(&self, now: Instant) -> Duration {
        if self.leading.is_some() {
            return Duration::ZERO;
        }
        now.duration_since(self.behind_since())
    }

    /// Lead the log at `epoch` from what it holds (ROADMAP DR4): a snapshot
    /// part way is dropped, the pair is no longer pulled from or heard,
    /// and a first entry of the new epoch is committed (R4), unless the log
    /// has one already (a restart).
    pub(crate) fn lead(&mut self, epoch: u64) -> Result<(), String> {
        self.receiving = None;
        self.leading = Some(epoch);
        self.reseeded = [None; 2];
        if self.log.last_epoch() < epoch {
            let next = self.log.last() + 1;
            self.log.append(
                next,
                &[Entry {
                    epoch,
                    cmd: Command::Noop,
                }],
            )?;
            self.log.commit_to(next)?;
        }
        Ok(())
    }

    /// Answer pair node `from` (index `i`), reseeding from this copy, and
    /// note how far it has come.
    fn serve(&mut self, i: usize, from: u32, msg: DrMsg, now: Instant) -> Out {
        if let DrMsg::Ask {
            after, after_epoch, ..
        } = &msg
            && *after <= self.log.last()
            && self.log.epoch_at(*after) == Some(*after_epoch)
        {
            self.reseeded[i] = Some(*after);
        }
        let reply = match msg {
            DrMsg::Ask { .. } | DrMsg::AskSnapshot { .. } => {
                self.shippers[i].answer(&self.log, false, msg, now)?
            }
            _ => None,
        };
        Ok(reply
            .map(|msg| DrEffect::Send { to: from, msg })
            .into_iter()
            .collect())
    }

    /// A write to replicated bucket `bucket`, while it leads: appended at
    /// its epoch and committed at once, there being no other copy to wait
    /// for. Refused `Unavailable` unless it leads.
    pub(crate) fn write(
        &mut self,
        bucket: String,
        quota: Option<u64>,
        op: Op,
    ) -> (Result<Outcome, Refusal>, Vec<DrEffect>) {
        let mut out = Vec::new();
        let Some(epoch) = self.leading.filter(|_| !self.frozen) else {
            return (Err(Refusal::Unavailable), out);
        };
        let next = self.log.last() + 1;
        let entry = Entry {
            epoch,
            cmd: Command::Write { bucket, quota, op },
        };
        let committed = match self
            .log
            .append(next, &[entry])
            .and_then(|()| self.log.commit_to(next))
        {
            Ok(c) => c,
            Err(e) => return (Err(Refusal::Failed(e)), out),
        };
        let mut result = Err(Refusal::Unknown);
        for c in committed {
            result = match c.write {
                None => continue,
                Some((bucket, Ok(applied))) => {
                    if let Some(revision) = applied.revision {
                        out.push(DrEffect::Notify {
                            bucket,
                            changed: applied.changed,
                            revision,
                        });
                    }
                    Ok(applied.outcome)
                }
                Some((_, Err(e))) => Err(Refusal::Failed(e)),
            };
        }
        (result, out)
    }

    /// When it last held everything the pair had committed (when it
    /// started, if never).
    pub(crate) fn behind_since(&self) -> Instant {
        self.caught_up_at.unwrap_or(self.started)
    }

    /// Stop pulling: what it holds stays as it is.
    pub(crate) fn stop(&mut self) {
        self.stopped = true;
    }

    /// Periodic work: a round when one is due, asks again after a timeout,
    /// compaction, and the lag warning.
    pub(crate) fn tick(&mut self, now: Instant) -> Out {
        let mut out = Vec::new();
        self.warn(now);
        if self.stopped {
            return Ok(out);
        }
        let to = self
            .log
            .compact_target(now, self.cfg.log_bytes, self.cfg.log_age);
        if to > self.log.base() {
            self.log.compact(to)?;
        }
        if self.leading.is_some() {
            for shipper in &mut self.shippers {
                shipper.tick(now);
            }
            return Ok(out);
        }
        let timeout = self.timeout();
        if let Some(r) = &mut self.receiving {
            if now.duration_since(r.asked_at) >= timeout.max(CHUNK_TIMEOUT) {
                r.misses += 1;
                if r.misses >= 2 {
                    // The other node starts a snapshot of its own.
                    let from = 1 - r.from;
                    self.receiving = None;
                    if !self.sources[from].differs {
                        self.pulling = from;
                    }
                    self.ask_snapshot(0, 0, now, &mut out);
                } else {
                    let (index, chunk) = (r.index, r.next_chunk);
                    self.ask_snapshot(index, chunk, now, &mut out);
                }
            }
            return Ok(out);
        }
        let pulled = self.sources[self.pulling];
        match pulled.asked_at {
            Some(at) if now.duration_since(at) >= timeout => {
                let s = &mut self.sources[self.pulling];
                s.misses += 1;
                s.asked_at = None;
                if s.misses >= 2 {
                    // Not heard: its commit index no longer counts.
                    s.heard = false;
                    s.misses = 0;
                    let other = 1 - self.pulling;
                    if !self.sources[other].differs {
                        self.pulling = other;
                    }
                }
                self.round(now, &mut out);
            }
            Some(_) => {}
            None if now >= self.next_round => self.round(now, &mut out),
            None => {}
        }
        Ok(out)
    }

    fn warn(&mut self, now: Instant) {
        let lag = self.lag(now);
        let target = self.cfg.rpo_target;
        if lag > target && !self.warned {
            self.warned = true;
            tracing::warn!(
                lag_s = lag.as_secs(),
                target_s = target.as_secs(),
                commit = self.log.commit(),
                "replication: the DR copy trails the pair past its target"
            );
        } else if lag <= target && self.warned {
            self.warned = false;
            tracing::info!(
                lag_s = lag.as_secs(),
                commit = self.log.commit(),
                "replication: the DR copy is back within its target"
            );
        }
    }

    /// Ask the node pulled from for a batch, and the other for its commit
    /// index.
    fn round(&mut self, now: Instant, out: &mut Vec<DrEffect>) {
        let after = self.log.last();
        let after_epoch = self.log.last_epoch();
        let rpo_target_ms = u64::try_from(self.cfg.rpo_target.as_millis()).unwrap_or(u64::MAX);
        for i in 0..2 {
            if self.sources[i].differs {
                continue;
            }
            let pulled = i == self.pulling;
            // A probe still unanswered and not yet late is not repeated.
            let waiting = self.sources[i]
                .asked_at
                .is_some_and(|at| now.duration_since(at) < self.timeout());
            if !pulled && waiting {
                continue;
            }
            self.sources[i].asked_at = Some(now);
            out.push(DrEffect::Send {
                to: self.cfg.pair[i],
                msg: DrMsg::Ask {
                    after,
                    after_epoch,
                    max_bytes: if pulled { self.cfg.batch_bytes } else { 0 },
                    rpo_target_ms,
                },
            });
        }
        self.next_round = now + self.cfg.poll;
    }

    /// The session with pair node `i` changed: a snapshot chunk asked of
    /// it on the old one is lost, so ask for it again now (entries are
    /// asked again within a poll anyway).
    pub(crate) fn reconnected(&mut self, i: usize, now: Instant) -> Vec<DrEffect> {
        let mut out = Vec::new();
        if self.leading.is_none()
            && let Some(r) = &self.receiving
            && r.from == i
            && i == self.pulling
        {
            let (index, chunk) = (r.index, r.next_chunk);
            self.ask_snapshot(index, chunk, now, &mut out);
        }
        out
    }

    fn ask_snapshot(&mut self, index: u64, chunk: u32, now: Instant, out: &mut Vec<DrEffect>) {
        if chunk == 0 {
            self.receiving = Some(Receiving {
                from: self.pulling,
                index,
                epoch: 0,
                next_chunk: 0,
                buckets: Vec::new(),
                asked_at: now,
                misses: 0,
            });
        } else if let Some(r) = &mut self.receiving {
            r.asked_at = now;
        }
        out.push(DrEffect::Send {
            to: self.cfg.pair[self.pulling],
            msg: DrMsg::AskSnapshot { index, chunk },
        });
    }

    /// A message from pair node `from`.
    pub(crate) fn answer(&mut self, from: u32, msg: DrMsg, now: Instant) -> Out {
        let mut out = Vec::new();
        let Some(i) = self.cfg.pair.iter().position(|&n| n == from) else {
            return Ok(out);
        };
        if self.leading.is_some() {
            // Once it leads, the pair's nodes are stale (DR4): it answers
            // their asks as they reseed, and takes nothing from them.
            return self.serve(i, from, msg, now);
        }
        if self.stopped {
            return Ok(out);
        }
        match msg {
            DrMsg::Entries {
                standby,
                commit,
                after,
                entries,
            } => {
                self.heard(i, standby, commit);
                if after == self.log.last() && !entries.is_empty() && self.receiving.is_none() {
                    self.apply(after, &entries, &mut out)?;
                }
                self.answered(i, now, &mut out);
            }
            DrMsg::Behind {
                standby,
                commit,
                after,
            } => {
                self.heard(i, standby, commit);
                if i == self.pulling && after == self.log.last() && self.receiving.is_none() {
                    self.sources[i].asked_at = None;
                    self.ask_snapshot(0, 0, now, &mut out);
                } else {
                    self.answered(i, now, &mut out);
                }
            }
            DrMsg::Snapshot {
                index,
                index_epoch,
                chunk,
                last,
                buckets,
            } => self.recv_snapshot(i, index, index_epoch, chunk, last, buckets, now, &mut out)?,
            DrMsg::Mismatch { commit, after } => {
                tracing::error!(
                    node = from,
                    index = after,
                    commit,
                    "replication: a pair node's committed log differs from the DR copy; \
                     not pulling from it"
                );
                self.sources[i].differs = true;
                self.sources[i].asked_at = None;
                if self.sources[1 - i].differs {
                    self.stopped = true;
                } else if i == self.pulling {
                    self.pulling = 1 - i;
                    self.round(now, &mut out);
                }
            }
            DrMsg::Ask { .. } | DrMsg::AskSnapshot { .. } | DrMsg::Deploys { .. } => {}
        }
        Ok(out)
    }

    fn heard(&mut self, i: usize, standby: bool, commit: u64) {
        let s = &mut self.sources[i];
        s.heard = true;
        s.standby = standby;
        s.commit = commit;
        s.misses = 0;
    }

    /// Append committed entries after `after` and apply them.
    fn apply(
        &mut self,
        after: u64,
        entries: &[super::log::Entry],
        out: &mut Vec<DrEffect>,
    ) -> Result<(), String> {
        self.log.append(after + 1, entries)?;
        let committed = self.log.commit_to(self.log.last())?;
        for c in committed {
            if let Some((bucket, Ok(applied))) = c.write
                && let Some(revision) = applied.revision
            {
                out.push(DrEffect::Notify {
                    bucket,
                    changed: applied.changed,
                    revision,
                });
            }
        }
        Ok(())
    }

    /// An answer from source `i`: choose the source, note a catch-up, and
    /// ask again at once while more is committed than this copy holds.
    fn answered(&mut self, i: usize, now: Instant, out: &mut Vec<DrEffect>) {
        self.sources[i].asked_at = None;
        let last = self.log.last();
        let best = self
            .sources
            .iter()
            .filter(|s| s.heard && !s.differs)
            .map(|s| s.commit)
            .max()
            .unwrap_or(0);
        if last >= best {
            self.caught_up_at = Some(now);
        }
        if self.receiving.is_some() {
            return;
        }
        let was = self.pulling;
        self.pulling = self.choose(best);
        if i == was && best > last {
            self.round(now, out);
        }
    }

    /// The standby while it is within a batch's worth of entries of the
    /// best commit index; otherwise the node with the best.
    fn choose(&self, best: u64) -> usize {
        let usable = |s: &Source| s.heard && !s.differs;
        let near = |s: &Source| s.commit + NEAR >= best;
        if let Some(i) = (0..2).find(|&i| {
            let s = &self.sources[i];
            usable(s) && s.standby && near(s)
        }) {
            return i;
        }
        (0..2)
            .filter(|&i| usable(&self.sources[i]))
            .max_by_key(|&i| self.sources[i].commit)
            .unwrap_or(self.pulling)
    }

    #[allow(clippy::too_many_arguments)]
    fn recv_snapshot(
        &mut self,
        i: usize,
        index: u64,
        index_epoch: u64,
        chunk: u32,
        last: bool,
        buckets: Vec<BucketDump>,
        now: Instant,
        out: &mut Vec<DrEffect>,
    ) -> Result<(), String> {
        let Some(r) = self.receiving.as_mut().filter(|r| r.from == i) else {
            return Ok(());
        };
        if chunk == 0 {
            // A new snapshot, or the first chunk of the one asked.
            r.index = index;
            r.epoch = index_epoch;
            r.next_chunk = 0;
            r.buckets.clear();
        } else if index != r.index || chunk != r.next_chunk {
            return Ok(());
        }
        r.misses = 0;
        merge(&mut r.buckets, buckets);
        r.next_chunk += 1;
        if !last {
            let (index, chunk) = (r.index, r.next_chunk);
            self.ask_snapshot(index, chunk, now, out);
            return Ok(());
        }
        let Some(r) = self.receiving.take() else {
            return Ok(());
        };
        if r.index <= self.log.last() {
            // Nothing newer than this copy already holds.
            self.answered(i, now, out);
            return Ok(());
        }
        let before = self.log.snapshot()?;
        let snap = Snapshot {
            index: r.index,
            epoch: r.epoch,
            buckets: r.buckets,
        };
        self.log.install(&snap)?;
        let mut notices = Vec::new();
        super::core::notify_differences(&before.buckets, &snap.buckets, &mut notices);
        out.extend(notices.into_iter().filter_map(|e| match e {
            super::core::Effect::Notify {
                bucket,
                changed,
                revision,
            } => Some(DrEffect::Notify {
                bucket,
                changed,
                revision,
            }),
            _ => None,
        }));
        tracing::info!(
            index = r.index,
            "replication: the DR copy caught up from a snapshot"
        );
        self.answered(i, now, out);
        Ok(())
    }
}

#[cfg(test)]
#[path = "dr_tests.rs"]
mod dr_tests;
