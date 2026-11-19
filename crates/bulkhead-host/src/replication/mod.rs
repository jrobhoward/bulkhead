//! Replicated buckets: `cluster`-scoped shared buckets kept
//! whole on both nodes of an HA pair, by the rules spike S20
//! model-checked (`spikes/s20/README.md`, R1 to R7).
//!
//! One log carries every replicated bucket's writes. The failover primary
//! leads it at its epoch: a write from any node goes to the primary, which
//! appends it, sends it to the standby, and commits it once the standby
//! has it on disk, or, once the witness has recorded the standby out of
//! date (`failover::record_partner`), alone. Each node applies committed
//! entries to its own copy (the `@cluster` namespace of its keyvalue
//! database) and tells its own watches, so reads and watches are local
//! and may trail the primary slightly. A linearizable read goes to the
//! primary, which serves it once it holds its lease and has committed an
//! entry of its own epoch (R5). Without a primary that can take it, a
//! write fails with `unavailable`.
//!
//! The replica itself is a state machine (the private `core` module, named
//! after the model's actions) over the log on disk (`log`); this module's
//! task feeds it leadership, writes, frames and ticks, and carries out
//! what it returns.

mod core;
mod dr;
mod log;
mod ship;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

pub(crate) use self::core::Refusal;
use self::core::{Config, Effect, Lead, Msg, Replica, Waiter, Write};
pub use self::dr::DrStatus;
use self::dr::{DrEffect, DrReplica, DrTuning};
pub(crate) use self::log::SPACE;
use self::ship::{DrMsg, Shipper};
use crate::error::HostError;
use crate::failover::{FailoverConfig, Leadership, RunUntil, Standing};
use crate::formats;
use crate::runtime::{Runtime, RuntimeInner};
use crate::services::keyvalue::storage::{KvStore, Op, Outcome};
use crate::transport::{self, Frame};

/// What a replicated operation returns: its outcome, or why it has none.
pub(crate) type Answer = Result<Outcome, Refusal>;

/// How long a write or read forwarded to the primary waits for its answer:
/// a little longer than the primary's own [`WRITE_TIMEOUT`], so its answer
/// comes first.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(6);

/// How long a linearizable read waits for the primary to be able to serve
/// it (a new primary's no-op to commit).
const READ_TIMEOUT: Duration = Duration::from_secs(3);

/// How long a write waits on the primary to commit (the standby and the
/// witness both out of reach) before it is answered as unknown: its entry
/// stays in the log and may still commit.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// The way into a node's replication task, kept by the keyvalue store.
#[derive(Debug, Clone)]
pub(crate) struct Handle {
    partner: u32,
    /// The DR node, and this node's side of shipping to it.
    dr: Option<(u32, Arc<Mutex<Shipper>>)>,
    tx: mpsc::UnboundedSender<Input>,
}

enum Input {
    Write {
        bucket: String,
        quota: Option<u64>,
        op: Op,
        reply: oneshot::Sender<Answer>,
    },
    Read {
        bucket: String,
        op: Op,
        reply: oneshot::Sender<Answer>,
    },
    Message(Msg),
    /// From the DR node.
    Dr(DrMsg),
    Recorded {
        epoch: u64,
        outdated: bool,
        ok: bool,
    },
}

impl Handle {
    /// Write to replicated bucket `bucket`, and wait until it commits.
    pub(crate) async fn write(&self, bucket: &str, quota: Option<u64>, op: Op) -> Answer {
        let (reply, answer) = oneshot::channel();
        let input = Input::Write {
            bucket: bucket.to_string(),
            quota,
            op,
            reply,
        };
        if self.tx.send(input).is_err() {
            return Err(Refusal::Unavailable);
        }
        answer.await.unwrap_or(Err(Refusal::Unknown))
    }

    /// A linearizable read of replicated bucket `bucket`, served by the
    /// primary.
    pub(crate) async fn read(&self, bucket: &str, op: Op) -> Answer {
        let (reply, answer) = oneshot::channel();
        let input = Input::Read {
            bucket: bucket.to_string(),
            op,
            reply,
        };
        if self.tx.send(input).is_err() {
            return Err(Refusal::Unavailable);
        }
        answer.await.unwrap_or(Err(Refusal::Unavailable))
    }
}

/// A frame of replication from node `from`.
pub(crate) fn deliver(rt: &RuntimeInner, from: u32, body: &[u8]) {
    let Some(handle) = rt.services.keyvalue().replication() else {
        return;
    };
    if from != handle.partner {
        tracing::warn!(
            from,
            "replication frame from a node that is not the partner; dropped"
        );
        return;
    }
    match transport::wire::from_cbor::<Msg>(body) {
        Ok(msg) => {
            let _ = handle.tx.send(Input::Message(msg));
        }
        Err(e) => tracing::warn!(from, error = %e, "malformed replication frame; dropped"),
    }
}

/// A frame of shipping to or from a DR node, from node `from`: to the DR
/// task from a pair node, or to the replication task from the configured
/// DR node.
pub(crate) fn deliver_dr(rt: &RuntimeInner, from: u32, body: &[u8]) {
    let kv = rt.services.keyvalue();
    let to_dr = kv.dr().filter(|d| d.pair.contains(&from));
    let to_pair = kv
        .replication()
        .filter(|h| h.dr.as_ref().is_some_and(|(n, _)| *n == from));
    if to_dr.is_none() && to_pair.is_none() {
        tracing::warn!(
            from,
            "DR frame from a node that is neither the DR node nor of its pair; dropped"
        );
        return;
    }
    match transport::wire::from_cbor::<DrMsg>(body) {
        Ok(DrMsg::Deploys { body }) => match kv.dr_deploys.get() {
            Some(tx) => {
                if tx.try_send((from, body)).is_err() {
                    tracing::warn!(from, "DR deployment message not taken in time; dropped");
                }
            }
            None => tracing::debug!(
                from,
                "DR deployment message before the node listens; dropped"
            ),
        },
        Ok(msg) => {
            if let Some(d) = to_dr {
                let _ = d.tx.send((from, msg));
            } else if let Some(h) = to_pair {
                let _ = h.tx.send(Input::Dr(msg));
            }
        }
        Err(e) => tracing::warn!(from, error = %e, "malformed DR frame; dropped"),
    }
}

/// Decode a DR frame's body as `deliver_dr` does and, if it decodes,
/// check it encodes back to the same message: for the fuzz targets in
/// `fuzz/` only.
#[cfg(fuzzing)]
#[doc(hidden)]
pub fn dr_body_for_fuzzing(body: &[u8]) {
    if let Ok(msg) = transport::wire::from_cbor::<DrMsg>(body)
        && let Ok(again) = transport::wire::to_cbor(&msg)
    {
        assert_eq!(transport::wire::from_cbor::<DrMsg>(&again).ok(), Some(msg));
    }
}

/// Where the replicated log stands on this node (`bulkhead status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReplicationStatus {
    /// This node's failover standing.
    pub standing: Standing,
    /// Its failover epoch.
    pub epoch: u64,
    /// The last entry applied to this node's copy.
    pub commit: u64,
    /// The last entry in its log.
    pub last: u64,
    /// The first entry still in its log, less one (compacted up to here).
    pub base: u64,
    /// The encoded size of the entries in its log.
    pub log_bytes: u64,
    /// On the primary, the last entry the standby acknowledged.
    pub partner_matched: Option<u64>,
    /// On the primary, whether it commits without its partner (the witness
    /// has the partner recorded out of date).
    pub alone: bool,
    /// Whether this node's copy holds every committed write: on the
    /// primary holding its lease, once it committed an entry of its own
    /// epoch (R5). A singleton with replicated buckets starts only then.
    pub ready: bool,
}

impl Runtime {
    /// Where this node's replicated log stands, if it has replicated
    /// buckets and failover runs.
    #[must_use]
    pub fn replication_status(&self) -> Option<ReplicationStatus> {
        self.inner()
            .services
            .keyvalue()
            .replication_status()
            .map(|s| *s.borrow())
    }

    /// [`Runtime::replication_status`] as it changes, if replication runs.
    #[must_use]
    pub fn replication_updates(&self) -> Option<watch::Receiver<ReplicationStatus>> {
        self.inner().services.keyvalue().replication_status()
    }

    /// On a pair node with a DR node: how long since the DR node last held
    /// every entry this node had committed (since this node started, if
    /// never).
    #[must_use]
    pub fn dr_lag(&self) -> Option<Duration> {
        let kv = self.inner().services.keyvalue();
        let (_, shipper) = kv.replication()?.dr.as_ref()?;
        let shipper = shipper.lock().unwrap_or_else(PoisonError::into_inner);
        Some(shipper.lag(Instant::now()))
    }

    /// On the DR node: where its copy of the pair's log stands.
    #[must_use]
    pub fn dr_status(&self) -> Option<DrStatus> {
        let kv = self.inner().services.keyvalue();
        let (status, since) = *kv.dr()?.status.borrow();
        Some(DrStatus {
            lag: Instant::now().duration_since(since),
            ..status
        })
    }

    /// The channel for the node's own DR messages about deployments
    /// (ROADMAP DR3), carried inside DR frames: the messages from the
    /// pair's nodes (on the DR node) or from the DR node (on a pair node),
    /// each with its sender, and the way to send them. Once only; `None`
    /// after the first call. Messages arriving before it are dropped.
    #[must_use]
    pub fn dr_deploys(&self) -> Option<(DrDeploySender, DrDeployInbox)> {
        let (tx, rx) = mpsc::channel(64);
        self.inner().services.keyvalue().dr_deploys.set(tx).ok()?;
        Some((
            DrDeploySender {
                rt: Arc::downgrade(self.inner()),
            },
            rx,
        ))
    }

    /// Keep a copy of HA pair `cfg.pair`'s replicated log on this node, the
    /// disaster-recovery node (ROADMAP Phase 8): committed entries pulled
    /// from either pair node, into this node's `cluster` buckets. Needs a
    /// data directory; once only.
    ///
    /// # Errors
    ///
    /// Without a data directory, if the log cannot be opened, or if it is
    /// already started.
    pub fn start_dr(&self, cfg: DrConfig) -> Result<(), HostError> {
        start_dr(self.inner(), &cfg)
    }
}

/// The node's DR deployment messages, each with the node it came from
/// ([`Runtime::dr_deploys`]).
pub type DrDeployInbox = mpsc::Receiver<(u32, Vec<u8>)>;

/// Sends the node's DR deployment messages ([`Runtime::dr_deploys`]).
#[derive(Debug, Clone)]
pub struct DrDeploySender {
    rt: Weak<RuntimeInner>,
}

impl DrDeploySender {
    /// Send `body` to node `to` in a DR frame: to a pair node from the DR
    /// node, or to the DR node from a pair node. Whether it was sent; lost
    /// if `to` is out of reach now, or is not one of those nodes.
    pub fn send(&self, to: u32, body: Vec<u8>) -> bool {
        let Some(rt) = self.rt.upgrade() else {
            return false;
        };
        let kv = rt.services.keyvalue();
        let peer = kv.dr().is_some_and(|d| d.pair.contains(&to))
            || kv
                .replication()
                .is_some_and(|h| h.dr.as_ref().is_some_and(|(n, _)| *n == to));
        if !peer {
            tracing::warn!(
                to,
                "DR deployment message to a node that is no DR peer; not sent"
            );
            return false;
        }
        match transport::wire::to_cbor(&DrMsg::Deploys { body }) {
            Ok(body) => transport::send(&rt, to, Frame::Dr { body }),
            Err(e) => {
                tracing::error!(error = %e, "DR frame not encoded");
                false
            }
        }
    }
}

/// A disaster-recovery node's configuration ([`Runtime::start_dr`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct DrConfig {
    /// The HA pair's nodes.
    pub pair: [u32; 2],
    /// Between rounds of asks once caught up.
    pub poll: Duration,
    /// About the most bytes of entries asked for at once.
    pub batch_bytes: u64,
    /// A warning is logged while the copy trails the pair by longer.
    pub rpo_target: Duration,
    /// The most bytes of entries its log keeps, and the age of the oldest,
    /// as on the pair ([`FailoverConfig::replication_log_bytes`]).
    pub log_bytes: u64,
    /// See [`DrConfig::log_bytes`].
    pub log_age: Duration,
}

impl DrConfig {
    /// For pair `pair`: a round a second, batches of about 1 MiB, a
    /// 5-minute target, and the pair's default log bounds.
    #[must_use]
    pub fn new(pair: [u32; 2]) -> Self {
        Self {
            pair,
            poll: Duration::from_secs(1),
            batch_bytes: 1 << 20,
            rpo_target: Duration::from_secs(300),
            log_bytes: 64 << 20,
            log_age: Duration::from_secs(3600),
        }
    }
}

/// The way into the DR node's task, kept by the keyvalue store.
#[derive(Debug, Clone)]
pub(crate) struct DrHandle {
    pair: [u32; 2],
    tx: mpsc::UnboundedSender<(u32, DrMsg)>,
    /// Its status, its lag left out, and since when it trails.
    status: watch::Receiver<(DrStatus, Instant)>,
}

fn start_dr(rt: &Arc<RuntimeInner>, cfg: &DrConfig) -> Result<(), HostError> {
    let kv = rt.services.keyvalue().clone();
    let disk = kv
        .disk()
        .ok_or_else(|| HostError::Engine("a DR node needs a data directory".into()))?;
    let log = log::Log::open(disk).map_err(HostError::Storage)?;
    let tuning = DrTuning {
        pair: cfg.pair,
        poll: cfg.poll,
        batch_bytes: cfg.batch_bytes.min(ship::MAX_BATCH_BYTES),
        rpo_target: cfg.rpo_target,
        log_bytes: cfg.log_bytes,
        log_age: cfg.log_age,
    };
    let now = Instant::now();
    let replica = DrReplica::new(tuning, log, now).map_err(HostError::Storage)?;
    let (tx, rx) = mpsc::unbounded_channel();
    let (status, status_rx) = watch::channel((replica.status(now), replica.behind_since()));
    kv.set_dr(DrHandle {
        pair: cfg.pair,
        tx,
        status: status_rx,
    })
    .map_err(|()| HostError::Engine("the DR copy is already started".into()))?;
    let task = DrTask {
        rt: Arc::downgrade(rt),
        kv,
        replica,
        status,
    };
    // Ticks often enough for rounds a poll apart and for timeouts.
    let tick = (cfg.poll / 2).clamp(Duration::from_millis(50), Duration::from_secs(1));
    tokio::spawn(task.run(rx, tick));
    Ok(())
}

/// The DR node's task: the copy fed ticks and the pair's answers.
struct DrTask {
    rt: Weak<RuntimeInner>,
    kv: Arc<KvStore>,
    replica: DrReplica,
    status: watch::Sender<(DrStatus, Instant)>,
}

impl DrTask {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<(u32, DrMsg)>, tick: Duration) {
        let mut ticker = tokio::time::interval(tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let mut inputs = Vec::new();
            let ticked = tokio::select! {
                input = rx.recv() => match input {
                    Some(i) => {
                        inputs.push(i);
                        false
                    }
                    None => return,
                },
                _ = ticker.tick() => true,
            };
            while let Ok(i) = rx.try_recv() {
                inputs.push(i);
            }
            let Some(rt) = self.rt.upgrade() else {
                return;
            };
            let effects = blocking(|| {
                let now = Instant::now();
                let mut out = Vec::new();
                for (from, msg) in inputs {
                    absorb_dr(self.replica.answer(from, msg, now), &mut out);
                }
                if ticked {
                    absorb_dr(self.replica.tick(now), &mut out);
                }
                out
            });
            for effect in effects {
                match effect {
                    DrEffect::Send { to, msg } => match transport::wire::to_cbor(&msg) {
                        // Lost if it cannot be sent now; asked again.
                        Ok(body) => {
                            transport::send(&rt, to, Frame::Dr { body });
                        }
                        Err(e) => tracing::error!(error = %e, "DR frame not encoded"),
                    },
                    DrEffect::Notify {
                        bucket,
                        changed,
                        revision,
                    } => {
                        if bucket == formats::BUCKET
                            && changed.iter().any(|k| k == formats::KEY)
                            && let Some(disk) = self.kv.disk()
                            && rt.formats_applied(&disk).is_some()
                        {
                            // The pair's version is newer than this build:
                            // stop, so it holds nothing it may not read.
                            self.replica.stop();
                        }
                        let key = (SPACE.to_string(), bucket);
                        self.kv.watches.notify(&key, &changed, revision);
                    }
                }
            }
            let now = Instant::now();
            self.status
                .send_replace((self.replica.status(now), self.replica.behind_since()));
        }
    }
}

fn absorb_dr(result: Result<Vec<DrEffect>, String>, out: &mut Vec<DrEffect>) {
    match result {
        Ok(effects) => out.extend(effects),
        Err(e) => tracing::error!(error = %e, "replication: the DR copy's storage failed"),
    }
}

/// Start replication for this node's replicated buckets, following
/// `leadership`, with the pair and the log's bounds from `cfg`. Called when
/// the node has replicated shared buckets or `cfg.replication` is set.
pub(crate) fn start(
    rt: &Arc<RuntimeInner>,
    cfg: &FailoverConfig,
    leadership: watch::Receiver<Leadership>,
) -> Result<(), HostError> {
    let partner = cfg.partner;
    let kv = rt.services.keyvalue().clone();
    let disk = kv
        .disk()
        .ok_or_else(|| HostError::Engine("replicated buckets need a data directory".into()))?;
    let log = log::Log::open(disk).map_err(HostError::Storage)?;
    let config = Config {
        log_bytes: cfg.replication_log_bytes,
        log_age: cfg.replication_log_age,
        ..Config::default()
    };
    let replica = Replica::new(config, log);
    let (tx, rx) = mpsc::unbounded_channel();
    let first = leadership.borrow().clone();
    let (status, status_rx) = watch::channel(status_of(&replica, &first));
    let dr = cfg
        .dr
        .map(|node| (node, Arc::new(Mutex::new(Shipper::new(Instant::now())))));
    kv.set_replication(
        Handle {
            partner,
            dr: dr.clone(),
            tx: tx.clone(),
        },
        status_rx,
    )
    .map_err(|()| HostError::Engine("replication is already started".into()))?;
    let task = Task {
        rt: Arc::downgrade(rt),
        kv,
        replica,
        partner,
        tx,
        lead: first,
        run_until: rt.failover.run_until(),
        waiters: HashMap::new(),
        next_waiter: 0,
        forwarded: HashMap::new(),
        next_forward: 0,
        reads: Vec::new(),
        status,
        dr,
        dr_out: Vec::new(),
    };
    tokio::spawn(task.run(rx, leadership, rt.liveness.interval()));
    Ok(())
}

fn status_of(replica: &Replica, lead: &Leadership) -> ReplicationStatus {
    ReplicationStatus {
        standing: lead.standing,
        epoch: lead.epoch,
        commit: replica.log().commit(),
        last: replica.log().last(),
        base: replica.log().base(),
        log_bytes: replica.log().bytes(),
        partner_matched: replica.matched(),
        alone: replica.alone(),
        ready: replica.read_ready(),
    }
}

/// Who waits for a write's answer.
enum Waiting {
    Local(oneshot::Sender<Answer>),
    /// The partner, for the forwarded operation `id`.
    Remote(u64),
}

/// A forwarded operation waiting for the primary's answer.
struct Forwarded {
    reply: oneshot::Sender<Answer>,
    sent: Instant,
    /// A write: once sent, it may have committed.
    write: bool,
}

/// A linearizable read waiting for the primary to be able to serve it.
struct PendingRead {
    bucket: String,
    op: Op,
    reply: Waiting,
    since: Instant,
}

struct Task {
    rt: Weak<RuntimeInner>,
    kv: Arc<KvStore>,
    replica: Replica,
    partner: u32,
    tx: mpsc::UnboundedSender<Input>,
    lead: Leadership,
    run_until: watch::Receiver<RunUntil>,
    /// Who waits for each write, and since when.
    waiters: HashMap<Waiter, (Waiting, Instant)>,
    next_waiter: Waiter,
    forwarded: HashMap<u64, Forwarded>,
    next_forward: u64,
    reads: Vec<PendingRead>,
    status: watch::Sender<ReplicationStatus>,
    /// The DR node and this node's side of shipping to it.
    dr: Option<(u32, Arc<Mutex<Shipper>>)>,
    /// Answers to the DR node, sent after a step.
    dr_out: Vec<DrMsg>,
}

/// What woke the task.
enum Wake {
    Input(Input),
    Leadership,
    Tick,
}

/// Run `f`, which blocks on disk: off the worker on the multi-threaded
/// runtime, inline on a single-threaded one (tests on a paused clock,
/// whose time must not move while disk work runs).
fn blocking<R>(f: impl FnOnce() -> R) -> R {
    match tokio::runtime::Handle::current().runtime_flavor() {
        tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
        _ => f(),
    }
}

impl Task {
    async fn run(
        mut self,
        mut rx: mpsc::UnboundedReceiver<Input>,
        mut leadership: watch::Receiver<Leadership>,
        interval: Duration,
    ) {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The leadership at the start counts as a change.
        let mut wake = Some(Wake::Leadership);
        loop {
            let first = match wake.take() {
                Some(w) => w,
                None => tokio::select! {
                    biased;
                    input = rx.recv() => match input {
                        Some(i) => Wake::Input(i),
                        None => return,
                    },
                    changed = leadership.changed() => match changed {
                        Ok(()) => Wake::Leadership,
                        Err(_) => return,
                    },
                    _ = ticker.tick() => Wake::Tick,
                },
            };
            let mut inputs = Vec::new();
            let mut lead_changed = false;
            let mut tick = false;
            match first {
                Wake::Input(i) => inputs.push(i),
                Wake::Leadership => lead_changed = true,
                Wake::Tick => tick = true,
            }
            while let Ok(i) = rx.try_recv() {
                inputs.push(i);
            }
            if lead_changed || leadership.has_changed().unwrap_or(false) {
                self.lead = leadership.borrow_and_update().clone();
            }
            let Some(rt) = self.rt.upgrade() else {
                return;
            };
            let partner_heard = rt.liveness.beats_from(self.partner).iter().any(|b| b.live);
            let effects = blocking(|| self.step(&rt, inputs, tick, partner_heard));
            self.send_dr(&rt);
            if self.carry_out(&rt, effects).is_some() {
                // A pair's version newer than this build: stop, so this
                // node neither acknowledges nor serves what it may not
                // read. The partner commits without it (R13).
                let status = ReplicationStatus {
                    ready: false,
                    ..status_of(&self.replica, &self.lead)
                };
                self.status.send_replace(status);
                return;
            }
            let status = status_of(&self.replica, &self.lead);
            self.status
                .send_if_modified(|s| std::mem::replace(s, status) != status);
        }
    }

    /// Handle one batch of inputs (and a tick), in order; writes in a row
    /// go to the log as one transaction.
    fn step(
        &mut self,
        rt: &RuntimeInner,
        inputs: Vec<Input>,
        tick: bool,
        partner_heard: bool,
    ) -> Vec<Effect> {
        let now = Instant::now();
        let mut out = Vec::new();
        let lead = Lead {
            standing: self.lead.standing,
            epoch: self.lead.epoch,
            may_run: self.lead.may_run,
            partner_outdated: self.lead.partner_outdated,
        };
        absorb(self.replica.leadership(lead, now), &mut out);
        let mut writes = Vec::new();
        for input in inputs {
            if !matches!(
                input,
                Input::Write { .. } | Input::Message(Msg::Write { .. })
            ) {
                self.flush(&mut writes, now, &mut out);
            }
            match input {
                Input::Write {
                    bucket,
                    quota,
                    op,
                    reply,
                } => self.write(rt, bucket, quota, op, Waiting::Local(reply), &mut writes),
                Input::Read { bucket, op, reply } => {
                    self.read(rt, bucket, op, Waiting::Local(reply), now);
                }
                Input::Recorded {
                    epoch,
                    outdated,
                    ok,
                } => {
                    let r = self.replica.recorded(epoch, outdated, ok, now);
                    absorb(r, &mut out);
                }
                Input::Message(Msg::Write {
                    id,
                    bucket,
                    quota,
                    op,
                }) => {
                    if self.replica.is_primary() {
                        let waiter = self.waiter(Waiting::Remote(id));
                        writes.push(Write {
                            waiter,
                            bucket,
                            quota,
                            op,
                        });
                    } else {
                        out.push(done(id, Err(Refusal::Unavailable)));
                    }
                }
                Input::Message(Msg::Read { id, bucket, op }) => {
                    if self.replica.is_primary() {
                        self.read(rt, bucket, op, Waiting::Remote(id), now);
                    } else {
                        out.push(done(id, Err(Refusal::Unavailable)));
                    }
                }
                Input::Dr(msg) => self.answer_dr(msg, now),
                Input::Message(Msg::Done { id, result }) => {
                    if let Some(f) = self.forwarded.remove(&id) {
                        let _ = f.reply.send(result);
                    }
                }
                Input::Message(msg) => {
                    let r = self.replica.message(msg, now);
                    absorb(r, &mut out);
                }
            }
        }
        self.flush(&mut writes, now, &mut out);
        if tick {
            let r = self.replica.tick(now, partner_heard);
            absorb(r, &mut out);
            self.expire(rt, now);
            if let Some((_, shipper)) = &self.dr {
                shipper
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .tick(now);
            }
        }
        self.serve_reads(now, &mut out);
        out
    }

    /// A message from the DR node, answered from this node's log.
    fn answer_dr(&mut self, msg: DrMsg, now: Instant) {
        let Some((_, shipper)) = &self.dr else {
            return;
        };
        let standby = self.replica.is_standby();
        let answer = shipper
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .answer(self.replica.log(), standby, msg, now);
        match answer {
            Ok(Some(reply)) => self.dr_out.push(reply),
            Ok(None) => {}
            Err(e) => tracing::error!(error = %e, "replication: a DR answer failed"),
        }
    }

    /// Send the answers to the DR node; lost if they cannot go now, since
    /// it asks again.
    fn send_dr(&mut self, rt: &RuntimeInner) {
        let Some((node, _)) = &self.dr else {
            return;
        };
        for msg in std::mem::take(&mut self.dr_out) {
            match transport::wire::to_cbor(&msg) {
                Ok(body) => {
                    transport::send(rt, *node, Frame::Dr { body });
                }
                Err(e) => tracing::error!(error = %e, "DR frame not encoded"),
            }
        }
    }

    fn waiter(&mut self, waiting: Waiting) -> Waiter {
        let id = self.next_waiter;
        self.next_waiter += 1;
        self.waiters.insert(id, (waiting, Instant::now()));
        id
    }

    /// A client's write: queued for the log on the primary, forwarded from
    /// the standby, refused elsewhere.
    fn write(
        &mut self,
        rt: &RuntimeInner,
        bucket: String,
        quota: Option<u64>,
        op: Op,
        reply: Waiting,
        writes: &mut Vec<Write>,
    ) {
        if self.replica.is_primary() {
            let waiter = self.waiter(reply);
            writes.push(Write {
                waiter,
                bucket,
                quota,
                op,
            });
            return;
        }
        let Waiting::Local(reply) = reply else {
            return;
        };
        if !self.replica.is_standby() {
            let _ = reply.send(Err(Refusal::Unavailable));
            return;
        }
        let id = self.next_forward;
        self.next_forward += 1;
        let msg = Msg::Write {
            id,
            bucket,
            quota,
            op,
        };
        if self.send(rt, &msg) {
            self.forwarded.insert(
                id,
                Forwarded {
                    reply,
                    sent: Instant::now(),
                    write: true,
                },
            );
        } else {
            let _ = reply.send(Err(Refusal::Unavailable));
        }
    }

    fn flush(&mut self, writes: &mut Vec<Write>, now: Instant, out: &mut Vec<Effect>) {
        if writes.is_empty() {
            return;
        }
        let r = self.replica.write(std::mem::take(writes), now);
        absorb(r, out);
    }

    /// A linearizable read: waits on the primary until it can be served,
    /// forwarded from the standby, refused elsewhere.
    fn read(&mut self, rt: &RuntimeInner, bucket: String, op: Op, reply: Waiting, now: Instant) {
        if self.replica.is_primary() {
            self.reads.push(PendingRead {
                bucket,
                op,
                reply,
                since: now,
            });
            return;
        }
        let Waiting::Local(reply) = reply else {
            return;
        };
        if !self.replica.is_standby() {
            let _ = reply.send(Err(Refusal::Unavailable));
            return;
        }
        let id = self.next_forward;
        self.next_forward += 1;
        if self.send(rt, &Msg::Read { id, bucket, op }) {
            self.forwarded.insert(
                id,
                Forwarded {
                    reply,
                    sent: now,
                    write: false,
                },
            );
        } else {
            let _ = reply.send(Err(Refusal::Unavailable));
        }
    }

    /// Serve the reads the primary can serve now (R5, with the lease as
    /// the instances see it); refuse them all if this node is no longer
    /// primary, and each that waited too long.
    fn serve_reads(&mut self, now: Instant, out: &mut Vec<Effect>) {
        if self.reads.is_empty() {
            return;
        }
        let primary = self.replica.is_primary();
        let ready = self.replica.read_ready() && self.run_until.borrow().allows(now);
        let mut waiting = Vec::new();
        for read in std::mem::take(&mut self.reads) {
            if !ready && primary && now.duration_since(read.since) < READ_TIMEOUT {
                waiting.push(read);
                continue;
            }
            let result = if ready {
                let key = (SPACE.to_string(), read.bucket);
                self.replica
                    .log()
                    .disk()
                    .run(&key, read.op, None)
                    .map(|a| a.outcome)
                    .map_err(Refusal::Failed)
            } else {
                Err(Refusal::Unavailable)
            };
            match read.reply {
                Waiting::Local(reply) => {
                    let _ = reply.send(result);
                }
                Waiting::Remote(id) => out.push(done(id, result)),
            }
        }
        self.reads = waiting;
    }

    /// Forwarded operations unanswered too long: a write may have
    /// committed.
    fn expire(&mut self, rt: &RuntimeInner, now: Instant) {
        let late: Vec<Waiter> = self
            .waiters
            .iter()
            .filter(|(_, (_, since))| now.duration_since(*since) >= WRITE_TIMEOUT)
            .map(|(id, _)| *id)
            .collect();
        for id in late {
            match self.waiters.remove(&id) {
                Some((Waiting::Local(reply), _)) => {
                    let _ = reply.send(Err(Refusal::Unknown));
                }
                Some((Waiting::Remote(fwd), _)) => {
                    self.send(rt, &done_msg(fwd, Err(Refusal::Unknown)));
                }
                None => {}
            }
        }
        let expired: Vec<u64> = self
            .forwarded
            .iter()
            .filter(|(_, f)| now.duration_since(f.sent) >= FORWARD_TIMEOUT)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(f) = self.forwarded.remove(&id) {
                let refusal = if f.write {
                    Refusal::Unknown
                } else {
                    Refusal::Unavailable
                };
                let _ = f.reply.send(Err(refusal));
            }
        }
    }

    fn send(&self, rt: &RuntimeInner, msg: &Msg) -> bool {
        match transport::wire::to_cbor(msg) {
            Ok(body) => transport::send(rt, self.partner, Frame::Replication { body }),
            Err(e) => {
                tracing::error!(error = %e, "replication frame not encoded");
                false
            }
        }
    }

    /// Carry out `effects`; `Some(version)` if one applied a pair's version
    /// newer than this build ([`crate::formats`]), after which replication
    /// stops.
    fn carry_out(&mut self, rt: &Arc<RuntimeInner>, effects: Vec<Effect>) -> Option<u16> {
        let mut outrun = None;
        for effect in effects {
            match effect {
                Effect::Send(msg) => {
                    // Lost if it cannot be sent now; the replica resends.
                    self.send(rt, &msg);
                }
                Effect::Record { epoch, outdated } => {
                    let rt = rt.clone();
                    let tx = self.tx.clone();
                    tokio::spawn(async move {
                        let ok = crate::failover::record_partner(&rt, epoch, outdated)
                            .await
                            .is_ok();
                        let _ = tx.send(Input::Recorded {
                            epoch,
                            outdated,
                            ok,
                        });
                    });
                }
                Effect::Reply { waiter, result } => match self.waiters.remove(&waiter) {
                    Some((Waiting::Local(reply), _)) => {
                        let _ = reply.send(result);
                    }
                    Some((Waiting::Remote(id), _)) => {
                        self.send(rt, &done_msg(id, result));
                    }
                    // Answered as unknown already.
                    None => {}
                },
                Effect::Notify {
                    bucket,
                    changed,
                    revision,
                } => {
                    // A primary applies only what it wrote itself, never
                    // newer than its build; a standby applies its partner's.
                    if bucket == formats::BUCKET
                        && !self.replica.is_primary()
                        && changed.iter().any(|k| k == formats::KEY)
                        && let Some(disk) = self.kv.disk()
                    {
                        outrun = outrun.or(rt.formats_applied(&disk));
                    }
                    let key = (SPACE.to_string(), bucket);
                    self.kv.watches.notify(&key, &changed, revision);
                }
            }
        }
        outrun
    }
}

fn absorb(result: Result<Vec<Effect>, String>, out: &mut Vec<Effect>) {
    match result {
        Ok(effects) => out.extend(effects),
        Err(e) => tracing::error!(error = %e, "replication: storage failed"),
    }
}

fn done_msg(id: u64, result: Answer) -> Msg {
    Msg::Done { id, result }
}

fn done(id: u64, result: Answer) -> Effect {
    Effect::Send(done_msg(id, result))
}
