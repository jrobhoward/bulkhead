//! The witness: grants promotions and resumes, waiting out the other
//! node's lease (R3), one request at a time (R5), and keeps the record of
//! a partner out of date for replicated buckets (R13).
//!
//! A grant or a change to the record is decided, kept on disk, then
//! applied (R10): the state in memory never runs ahead of disk, so no
//! answer carries an epoch or a record a restart could lose. While a
//! change is being kept, the witness answers nothing from the pair: a
//! renewal of the node being replaced could otherwise extend its lease
//! after the grant was decided, and a second request would break R5.
//! Unanswered, both are as if lost.
//!
//! The record (spike S20's R1): the primary sets it, at its epoch, before
//! it commits a replicated write without its partner, and clears it once
//! the partner has caught up. While it stands, the witness refuses to
//! promote the partner, which may lack committed writes. A promotion
//! clears it: the new primary had every committed write, and starts
//! committing only with its partner's acknowledgement.
//!
//! A witness that is also the disaster-recovery node (ROADMAP DR4, spike
//! S38's operator fencing) can be fenced: from then on it renews nothing
//! and grants nothing ([`Witness::fence`]). Once the pair's lease has run
//! out as it knows it, it grants itself a new epoch ([`Witness::activate`]),
//! after which the pair's nodes are stale by the rules above alone: a
//! renewal at an older epoch is fenced, and no request is granted, since a
//! resume is not from the promoted node and a promotion is at no higher
//! epoch. Fencing is in memory only: a restart ends it, and the operator
//! asks again.

use std::collections::HashMap;
use std::time::Duration;

use tokio::time::Instant;

use super::{DrIntent, FailoverMsg, WitnessConfig, WitnessState};

#[derive(Debug)]
pub(super) struct Witness {
    pub(super) cfg: WitnessConfig,
    pub(super) epoch: u64,
    pub(super) promoted: u32,
    /// The partner of `promoted` is recorded out of date.
    pub(super) outdated: bool,
    /// The last renewal the witness granted each node.
    pub(super) renewed: HashMap<u32, Instant>,
    /// `promoted` stepped down at `epoch` in a planned switchover and gave
    /// up its lease (`Release`): its partner's request need not
    /// wait the lease out. Cleared by every grant; kept in memory only, so
    /// a restarted witness waits as before.
    pub(super) released: bool,
    /// A change decided and being kept on disk.
    keeping: bool,
    /// Fenced for an activation (DR4), and a pair node heard since, if any.
    pub(super) fencing: Option<Option<u32>>,
    /// While fencing for a DR node apart (DR4b): when the pair's lease is
    /// gone, and so when its next ask is granted.
    gone: Option<Instant>,
    /// The pair node that abandoned the last activation, until the next
    /// fence (DR4b).
    abandoned: Option<u32>,
    /// The last activation it fenced for (DR4b): a fence of no later one
    /// is only answered. In memory: a restart ends the session, so no
    /// fence sent before it arrives after it.
    fenced_for: Option<u64>,
}

/// Why the witness will not activate the DR node.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ActivateRefused {
    /// The DR node is promoted already.
    #[error("the DR node is active already, at epoch {0}")]
    Active(u64),
    /// Another activation is fencing.
    #[error("an activation is under way")]
    Fencing,
    /// A pair node reached the witness while it fenced: site 1 is not down.
    #[error(
        "node {0} reached the witness while it fenced, so site 1 is not down; \
         the activation was abandoned (fence that node, then ask again)"
    )]
    Heard(u32),
    /// This node is not the witness, or not the DR node.
    #[error("{0}")]
    NotHere(String),
    /// The witness, apart from this DR node, has not promoted it in time
    /// (ROADMAP DR4b): out of reach, or still waiting.
    #[error(
        "the witness (node {0}) has not promoted this node within {1} s: out of reach, or still \
         waiting out the pair's lease; this node goes on following the witness and leads if it \
         does promote it (`bulkhead dr` shows it)"
    )]
    Unconfirmed(u32, u64),
    /// The new epoch could not be kept on disk.
    #[error("the witness's state was not kept: {0}")]
    NotKept(String),
}

/// What the witness does with a message.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decision {
    /// Answer at once.
    Answer(FailoverMsg),
    /// Grant: keep the state it leads to on disk, then [`Witness::apply`].
    Grant(Grant),
    /// Change the record: keep it on disk, then [`Witness::apply_record`].
    Record(Record),
    /// A grant to or from a DR node apart (DR4b): keep it on disk, then
    /// [`Witness::apply`], and answer with [`Witness::dr_state`].
    GrantThenState(Grant),
}

/// A grant decided and not yet applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Grant {
    /// The node granted.
    pub(super) to: u32,
    /// The request granted, echoed in the answer.
    pub(super) seq: u64,
    /// The epoch it is granted at.
    pub(super) epoch: u64,
    /// The node promoted at that epoch.
    pub(super) promoted: u32,
    /// The record after the grant: cleared by a promotion, kept by a
    /// resume.
    pub(super) outdated: bool,
}

impl Grant {
    /// The witness's state once the grant is applied.
    pub(super) fn state(&self) -> WitnessState {
        WitnessState::new(self.epoch, self.promoted, self.outdated)
    }
}

/// A change to the record, decided and not yet applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Record {
    /// The primary that asked, at the witness's epoch.
    pub(super) to: u32,
    /// Its request, echoed in the answer.
    pub(super) seq: u64,
    /// The record asked for.
    pub(super) outdated: bool,
    /// The witness's state once it is applied.
    pub(super) state: WitnessState,
}

impl Witness {
    /// A witness starting at epoch 1 with `cfg.first` and no record, or
    /// resuming the state kept on disk. A resumed witness counts the node it promoted as renewed
    /// now: its renewals before the restart were lost, and none can be later
    /// than this, so grants wait at least as long as with the true record
    /// (R10, which the model keeps across crashes).
    pub(super) fn start(cfg: WitnessConfig) -> Self {
        let state = cfg
            .resume
            .unwrap_or_else(|| WitnessState::new(1, cfg.first, false));
        let mut renewed = HashMap::new();
        if cfg.resume.is_some() {
            renewed.insert(state.promoted, Instant::now());
        }
        Self {
            cfg,
            epoch: state.epoch,
            promoted: state.promoted,
            outdated: state.outdated,
            renewed,
            released: false,
            keeping: false,
            fencing: None,
            gone: None,
            abandoned: None,
            fenced_for: None,
        }
    }

    /// Whether a node outside the pair is promoted: the DR node, activated
    /// (DR4).
    pub(super) fn activated(&self) -> bool {
        !self.cfg.pair.contains(&self.promoted)
    }

    /// Start fencing for an activation: from now on every renewal is
    /// answered `Fenced` and every request `Refused`. When the pair's lease
    /// is gone as this witness knows it: its last renewal of either node
    /// (or its restart) plus lease and margin.
    pub(super) fn fence(&mut self, now: Instant) -> Result<Instant, ActivateRefused> {
        if self.activated() {
            return Err(ActivateRefused::Active(self.epoch));
        }
        if self.fencing.is_some() {
            return Err(ActivateRefused::Fencing);
        }
        self.fencing = Some(None);
        let hold = self.cfg.lease + self.cfg.margin;
        Ok(self
            .cfg
            .pair
            .iter()
            .filter_map(|n| self.renewed.get(n))
            .map(|r| *r + hold)
            .max()
            .map_or(now, |gone| gone.max(now)))
    }

    /// The grant that activates node `dr` at a new epoch, once fencing has
    /// heard no pair node; keep it on disk, then [`Witness::apply`]. A pair
    /// node heard while fencing abandons the activation.
    pub(super) fn activate(&mut self, dr: u32) -> Result<Grant, ActivateRefused> {
        match self.fencing {
            Some(None) if !self.keeping => {
                self.keeping = true;
                Ok(Grant {
                    to: dr,
                    seq: 0,
                    epoch: self.epoch + 1,
                    promoted: dr,
                    outdated: false,
                })
            }
            Some(Some(node)) => {
                self.fencing = None;
                Err(ActivateRefused::Heard(node))
            }
            _ => Err(ActivateRefused::Fencing),
        }
    }

    fn partner(&self, node: u32) -> Option<u32> {
        match self.cfg.pair {
            [a, b] if a == node => Some(b),
            [a, b] if b == node => Some(a),
            _ => None,
        }
    }

    pub(super) fn handle(&mut self, from: u32, msg: FailoverMsg) -> Option<Decision> {
        if self.cfg.dr == Some(from) && !self.cfg.pair.contains(&from) {
            return self.handle_dr(from, msg, Instant::now());
        }
        let partner = self.partner(from)?;
        if self.keeping {
            return None;
        }
        let now = Instant::now();
        let hold = self.cfg.lease + self.cfg.margin;
        let answer = |msg| Some(Decision::Answer(msg));
        if self.activated() {
            // The DR node is active (DR4): the pair's nodes are told so,
            // and nothing is renewed or granted.
            return match msg {
                FailoverMsg::Renew { .. }
                | FailoverMsg::Record { .. }
                | FailoverMsg::Request { .. } => answer(FailoverMsg::DrActive {
                    epoch: self.epoch,
                    dr: self.promoted,
                }),
                _ => None,
            };
        }
        if let Some(heard) = &mut self.fencing {
            // Fencing for an activation (DR4): nothing renewed or granted.
            return match msg {
                FailoverMsg::Renew { .. } | FailoverMsg::Record { .. } => {
                    *heard = Some(from);
                    answer(FailoverMsg::Fenced { epoch: self.epoch })
                }
                FailoverMsg::Request { .. } => {
                    *heard = Some(from);
                    answer(FailoverMsg::Refused { epoch: self.epoch })
                }
                _ => None,
            };
        }
        match msg {
            FailoverMsg::Renew { seq, epoch } => {
                if epoch == self.epoch && from == self.promoted {
                    self.renewed.insert(from, now);
                    answer(FailoverMsg::LeaseOk { seq })
                } else {
                    answer(FailoverMsg::Fenced { epoch: self.epoch })
                }
            }
            FailoverMsg::Request {
                seq,
                resume,
                epoch,
                confirmed_ago_ms,
            } => {
                // A node recorded out of date is not promoted (R13).
                let allowed = if resume {
                    from == self.promoted
                } else {
                    epoch + 1 > self.epoch && !(self.outdated && from != self.promoted)
                };
                if !allowed {
                    return answer(FailoverMsg::Refused { epoch: self.epoch });
                }
                // The partner's lease is gone by the witness's record of
                // renewals and by the requester's last confirmation (R3),
                // or at once if the partner released it (R14).
                let confirmed = now
                    .checked_sub(Duration::from_millis(confirmed_ago_ms))
                    .unwrap_or(now);
                let mut gone_at = confirmed + hold;
                if let Some(renewed) = self.renewed.get(&partner) {
                    gone_at = gone_at.max(*renewed + hold);
                }
                if self.released && !resume && partner == self.promoted {
                    gone_at = now;
                }
                if now < gone_at {
                    let wait = gone_at.duration_since(now);
                    return answer(FailoverMsg::NotYet {
                        wait_ms: u64::try_from(wait.as_millis()).unwrap_or(u64::MAX) + 1,
                    });
                }
                self.keeping = true;
                Some(Decision::Grant(if resume {
                    Grant {
                        to: from,
                        seq,
                        epoch: self.epoch,
                        promoted: self.promoted,
                        outdated: self.outdated,
                    }
                } else {
                    Grant {
                        to: from,
                        seq,
                        epoch: epoch + 1,
                        promoted: from,
                        outdated: false,
                    }
                }))
            }
            FailoverMsg::Release { epoch } => {
                // Only the node promoted at the witness's epoch (R14). In
                // order behind its renewals and before any later request of
                // its own, on the one connection between them.
                if epoch == self.epoch && from == self.promoted {
                    self.released = true;
                }
                None
            }
            FailoverMsg::Record {
                seq,
                epoch,
                outdated,
            } => {
                // Only the node promoted at the witness's epoch (R13).
                if epoch != self.epoch || from != self.promoted {
                    return answer(FailoverMsg::Fenced { epoch: self.epoch });
                }
                if outdated == self.outdated {
                    return answer(FailoverMsg::Recorded { seq, outdated });
                }
                self.keeping = true;
                Some(Decision::Record(Record {
                    to: from,
                    seq,
                    outdated,
                    state: WitnessState::new(self.epoch, self.promoted, outdated),
                }))
            }
            _ => None,
        }
    }

    /// An ask from the DR node apart, `dr` (DR4b): act on it, then answer
    /// with the state it leads to. A fence starts as [`Witness::fence`];
    /// the activation is decided on the first ask once the pair's lease is
    /// gone (the DR node asks every poll), or abandoned if a pair node was
    /// heard meanwhile. A hand-back is [`Witness::hand_back`]. Every ask
    /// may come again: what is done already is only answered. While a
    /// change is being kept, nothing is answered.
    fn handle_dr(&mut self, dr: u32, msg: FailoverMsg, now: Instant) -> Option<Decision> {
        let FailoverMsg::DrAsk { intent } = msg else {
            return None;
        };
        if self.keeping {
            return None;
        }
        match intent {
            DrIntent::State => {}
            DrIntent::Fence { attempt } => {
                if !self.activated()
                    && self.fencing.is_none()
                    && self.fenced_for.is_none_or(|last| attempt > last)
                    && let Ok(gone) = self.fence(now)
                {
                    self.gone = Some(gone);
                    self.abandoned = None;
                    self.fenced_for = Some(attempt);
                }
            }
            DrIntent::HandBack { to } => {
                if self.promoted == dr
                    && let Ok(grant) = self.hand_back(to)
                {
                    return Some(Decision::GrantThenState(grant));
                }
            }
        }
        if self.gone.is_some_and(|gone| now >= gone) {
            self.gone = None;
            match self.activate(dr) {
                Ok(grant) => return Some(Decision::GrantThenState(grant)),
                Err(ActivateRefused::Heard(node)) => self.abandoned = Some(node),
                Err(_) => {}
            }
        }
        Some(Decision::Answer(self.dr_state()))
    }

    /// What a DR node apart follows (DR4b).
    pub(super) fn dr_state(&self) -> FailoverMsg {
        FailoverMsg::DrState {
            epoch: self.epoch,
            promoted: self.promoted,
            fencing: self.fencing.is_some(),
            abandoned: self.abandoned,
        }
    }

    /// The grant that hands back from the active DR node to pair node `to`
    /// at a new epoch (DR4's failback), its partner not recorded out of
    /// date: both have reseeded from the DR node, which checked it; keep
    /// it on disk, then [`Witness::apply`]. `to` then resumes when it next
    /// asks.
    pub(super) fn hand_back(&mut self, to: u32) -> Result<Grant, ActivateRefused> {
        if !self.activated() {
            return Err(ActivateRefused::NotHere("the DR node is not active".into()));
        }
        if !self.cfg.pair.contains(&to) || self.keeping {
            return Err(ActivateRefused::NotHere(format!(
                "node {to} is not of the pair {:?}",
                self.cfg.pair
            )));
        }
        self.keeping = true;
        Ok(Grant {
            to,
            seq: 0,
            epoch: self.epoch + 1,
            promoted: to,
            outdated: false,
        })
    }

    /// Apply `grant`, now kept on disk, and return the answer to send.
    pub(super) fn apply(&mut self, grant: Grant) -> FailoverMsg {
        self.keeping = false;
        self.released = false;
        self.fencing = None;
        self.gone = None;
        self.epoch = grant.epoch;
        self.promoted = grant.promoted;
        self.outdated = grant.outdated;
        if let Some(partner) = self.partner(grant.to) {
            self.renewed.remove(&partner);
        }
        self.renewed.insert(grant.to, Instant::now());
        FailoverMsg::Granted {
            seq: grant.seq,
            epoch: grant.epoch,
            outdated: grant.outdated,
        }
    }

    /// Apply `record`, now kept on disk, and return the answer to send.
    pub(super) fn apply_record(&mut self, record: Record) -> FailoverMsg {
        self.keeping = false;
        self.outdated = record.outdated;
        FailoverMsg::Recorded {
            seq: record.seq,
            outdated: record.outdated,
        }
    }

    /// Drop a change that could not be kept; the requester asks again (an
    /// activation is abandoned).
    pub(super) fn abandon(&mut self) {
        self.keeping = false;
        self.fencing = None;
        self.gone = None;
    }
}

#[cfg(test)]
#[path = "witness_tests.rs"]
mod witness_tests;
