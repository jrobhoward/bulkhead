//! The misbehaviour policy for native instances (ROADMAP PS5).
//!
//! The node counts, per instance, the refusals it answers (`access-denied`
//! from a capability the instance was not granted, and refusals by the
//! messaging ACLs, together) and, when a rate is set, the requests it
//! answers each second. Past either, the instance is killed:
//! `crashed: policy(refusals)` or `crashed: policy(rate)`. A correct app
//! meets a refusal rarely (a library probing a socket); one that keeps
//! probing what it may not reach is either broken or hostile.
//!
//! One [`Policy`] is shared by the instance's task and its sender
//! channels' tasks; each answer is classified once ([`refusal`]). An
//! answer that is no refusal, on an instance with no rate, takes no lock.
//!
//! The node, not this crate, counts these kills, with protocol breaks,
//! towards quarantining the app (node config `[limits] quarantine_after`).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bulkhead_native::protocol::{
    Answer, BlobError, CallError, CastError, ConfigError, GroupsError, HttpError, KvError,
    MetricError, NetError, RegisterError, SecretError, SqlError,
};

use crate::config::HostObjects;
use crate::instance::InstanceHandle;
use crate::message::ExitReason;
use crate::metrics::AppStats;

/// What kind of refusal an answer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// A capability, bucket, group, secret, datasource, address or
    /// destination the instance was not granted.
    AccessDenied,
    /// A call or cast the messaging ACLs refused.
    Acl,
}

impl Refusal {
    /// As metrics label it.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::AccessDenied => "access-denied",
            Self::Acl => "acl",
        }
    }
}

/// Why the policy kills an instance, as `crashed: policy(<reason>)`
/// names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Over {
    /// Past `refusals_max` within the period.
    Refusals,
    /// Past `requests_per_sec`.
    Rate,
}

impl Over {
    /// The reason's name.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Refusals => "refusals",
            Self::Rate => "rate",
        }
    }
}

/// Whether `answer` refuses the instance something it may not have.
#[must_use]
pub(crate) fn refusal(answer: &Answer) -> Option<Refusal> {
    let denied = match answer {
        Answer::Called(Err(CallError::AccessDenied))
        | Answer::CastFailed(CastError::AccessDenied) => return Some(Refusal::Acl),
        Answer::Registered(Err(RegisterError::AccessDenied))
        | Answer::Kv(Err(KvError::AccessDenied))
        | Answer::KvOpened(Err(KvError::AccessDenied))
        | Answer::Groups(Err(GroupsError::AccessDenied))
        | Answer::Config(Err(ConfigError::AccessDenied))
        | Answer::ConfigAll(Err(ConfigError::AccessDenied))
        | Answer::Secret(Err(SecretError::AccessDenied))
        | Answer::Metric(Err(MetricError::AccessDenied))
        | Answer::Sql(Err(SqlError::AccessDenied))
        | Answer::Net(Err(NetError::Denied))
        | Answer::Http(Err(HttpError::Denied))
        | Answer::Blob(Err(BlobError::Denied))
        | Answer::AdminStream(Err(NetError::Denied))
        | Answer::MetricsText(Err(NetError::Denied))
        | Answer::Connected(Err(NetError::Denied)) => true,
        // Spelled out, so a new answer is classified when it is added.
        Answer::Unit
        | Answer::Pid(_)
        | Answer::MaybePid(_)
        | Answer::Ref(_)
        | Answer::Called(_)
        | Answer::CastFailed(_)
        | Answer::Registered(_)
        | Answer::Kv(_)
        | Answer::KvOpened(_)
        | Answer::Groups(_)
        | Answer::Config(_)
        | Answer::ConfigAll(_)
        | Answer::Secret(_)
        | Answer::Metric(_)
        | Answer::Sql(_)
        | Answer::Net(_)
        | Answer::Http(_)
        | Answer::Extension(_)
        | Answer::Sender(_)
        | Answer::Elsewhere(_)
        | Answer::AdminStream(_)
        | Answer::MetricsText(_)
        | Answer::Connected(_)
        | Answer::Blob(_) => false,
    };
    denied.then_some(Refusal::AccessDenied)
}

/// The limits one instance is held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    /// Refusals within `period`.
    pub(crate) refusals: u32,
    pub(crate) period: Duration,
    /// Requests per second; 0 for no limit.
    pub(crate) per_sec: u32,
}

impl From<&HostObjects> for Limits {
    fn from(h: &HostObjects) -> Self {
        Self {
            refusals: h.refusals,
            period: h.refusals_period,
            per_sec: h.requests_per_sec,
        }
    }
}

/// What has been counted.
#[derive(Debug, Default)]
pub(crate) struct Counts {
    /// When each refusal within the period came, oldest first; at most
    /// one more than the limit.
    refused: VecDeque<Instant>,
    /// The current second's start and its requests.
    second: Option<(Instant, u32)>,
    /// The policy has killed the instance: count no more.
    over: bool,
}

impl Counts {
    /// Count one answered request, `refusal` if it is one, at `now`;
    /// whether that puts the instance over a limit (once).
    pub(crate) fn count(
        &mut self,
        limits: Limits,
        refusal: Option<Refusal>,
        now: Instant,
    ) -> Option<Over> {
        if self.over {
            return None;
        }
        if refusal.is_some() {
            while self
                .refused
                .front()
                .is_some_and(|at| now.duration_since(*at) >= limits.period)
            {
                self.refused.pop_front();
            }
            self.refused.push_back(now);
            if self.refused.len() > limits.refusals as usize {
                self.over = true;
                return Some(Over::Refusals);
            }
        }
        if limits.per_sec > 0 {
            let (start, n) = match self.second {
                Some((start, n)) if now.duration_since(start) < Duration::from_secs(1) => {
                    (start, n + 1)
                }
                _ => (now, 1),
            };
            self.second = Some((start, n));
            if n > limits.per_sec {
                self.over = true;
                return Some(Over::Rate);
            }
        }
        None
    }
}

/// One instance's policy: its limits, its counts, and the handle that
/// kills it.
#[derive(Debug)]
pub(crate) struct Policy {
    me: InstanceHandle,
    stats: Arc<AppStats>,
    limits: Limits,
    counts: Mutex<Counts>,
}

impl Policy {
    pub(crate) fn new(me: InstanceHandle, stats: Arc<AppStats>, limits: &HostObjects) -> Self {
        Self {
            me,
            stats,
            limits: limits.into(),
            counts: Mutex::default(),
        }
    }

    /// The node answered one of the instance's requests with `answer`.
    pub(crate) fn answered(&self, answer: &Answer) {
        self.counted(refusal(answer));
    }

    /// The node answered one of the instance's requests with no refusal
    /// (a hot keyvalue request).
    pub(crate) fn served(&self) {
        self.counted(None);
    }

    fn counted(&self, refusal: Option<Refusal>) {
        if refusal.is_none() && self.limits.per_sec == 0 {
            return;
        }
        if let Some(r) = refusal {
            self.stats.refused(r);
        }
        let over = self
            .counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .count(self.limits, refusal, Instant::now());
        if let Some(over) = over {
            tracing::warn!(
                pid = %self.me.pid(),
                reason = over.label(),
                refusals_max = self.limits.refusals,
                period_ms = u64::try_from(self.limits.period.as_millis()).unwrap_or(u64::MAX),
                requests_per_sec = self.limits.per_sec,
                "the misbehaviour policy kills the instance"
            );
            self.stats.policy_kill(over);
            self.me
                .kill_with(ExitReason::Crashed(format!("policy({})", over.label())));
        }
    }
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;
