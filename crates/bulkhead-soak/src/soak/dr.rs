//! The disaster-recovery drill (ROADMAP DR5): the failover soak's witness
//! and pair, the witness also the DR node (`[dr]`), with the WAN between
//! site 1 (the pair) and site 2 (the DR node) given delay, jitter and loss
//! throughout, and cut now and then. The pair runs the ledger example on a
//! `cluster` bucket and the counter example as a singleton; the DR node
//! holds both.
//!
//! The run is a series of cycles, each the whole drill:
//!
//! 1. Writes flow through either pair node, under WAN cuts. In some
//!    cycles an activation is asked while site 1 still reaches the
//!    witness: it must be refused.
//! 2. Site 1 is lost while writes flow: both pair nodes killed at once.
//! 3. The DR node is activated (a second later), with the lag it reported just before. Every
//!    write acknowledged longer ago than that lag (and a margin) before
//!    the loss must be in its copy: the data lost is within the lag
//!    reported.
//! 4. Writes go to the DR node, which runs the singleton.
//! 5. Site 1 comes back; its nodes reseed from the DR node, which goes on
//!    taking writes meanwhile.
//! 6. The DR node hands back to a pair node chosen at random. Both pair
//!    nodes must then hold every write the DR node held and every write
//!    acknowledged since the activation, and the DR node pulls its copy
//!    from the pair again.
//!
//! Throughout, a watcher asks all three nodes which runs the singleton,
//! a few times a second, and the run fails at any moment two do. No key a
//! client was told was refused may appear anywhere.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::events::{EventLog, What};
use super::failover::{APP as SINGLETON, Args, Cluster, Setup, SetupApp};
use super::node::ADMIN_TIMEOUT;
use super::proxy::Mode;
use super::rng::Rng;

/// The app writing to the `cluster` bucket.
const LEDGER: &str = "ledger";

/// How long a write may take before its answer counts as unknown.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// The WAN between the sites, each way.
const WAN: Mode = Mode::Wan {
    delay_ms: 50,
    jitter_ms: 20,
    per_mille: 50,
};

/// How long after site 1's loss the activation is asked: past the WAN's
/// longest delay, so nothing site 1 sent before it died still reaches the
/// witness while it fences (which, rightly, abandons the activation; an
/// operator asks seconds later at the least).
const AFTER_LOSS: Duration = Duration::from_secs(1);

/// Slack on the lag a DR node reports: the last answer's one-way delay,
/// the poll between asks, and the status read's own age.
const LAG_SLACK: Duration = Duration::from_secs(3);

/// How long the DR node has to hold the pair's apps, shipped over the WAN.
const HOLD: Duration = Duration::from_secs(600);

/// How long the pair nodes have to reseed, and the copies to agree.
const CONVERGE: Duration = Duration::from_secs(120);

/// The DR node: node 3, the witness, or with `--apart` node 4, a DR node
/// apart from it (ROADMAP DR4b).
static DR_NODE: AtomicU32 = AtomicU32::new(3);

/// The DR node's index in the cluster's nodes and sockets.
fn dr_at() -> usize {
    usize::try_from(DR_NODE.load(Ordering::Relaxed) - 1).unwrap_or(2)
}

/// The writers' pause between writes is multiplied by this
/// (`--slow-writes`: 20).
static PAUSE_SCALE: AtomicU64 = AtomicU64::new(1);

/// How a write ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Acked,
    Refused,
    Unknown,
}

/// A write: its key, how it ended, and when the answer came.
#[derive(Debug, Clone)]
struct Written {
    key: String,
    outcome: Outcome,
    at: Instant,
}

fn outcome(reply: Option<&str>) -> Outcome {
    match reply {
        Some("ok") => Outcome::Acked,
        Some(r) if r.contains("no primary with a lease") || r.contains("not running") => {
            Outcome::Refused
        }
        _ => Outcome::Unknown,
    }
}

/// Every write so far.
#[derive(Debug, Default)]
struct Ledger {
    writes: Vec<Written>,
    next: u64,
}

impl Ledger {
    fn keys(&self, outcome: Outcome) -> BTreeSet<String> {
        self.writes
            .iter()
            .filter(|w| w.outcome == outcome)
            .map(|w| w.key.clone())
            .collect()
    }

    /// Keys acknowledged from `since` and before `before`.
    fn acked_between(&self, since: Instant, before: Instant) -> BTreeSet<String> {
        self.writes
            .iter()
            .filter(|w| w.outcome == Outcome::Acked && w.at >= since && w.at < before)
            .map(|w| w.key.clone())
            .collect()
    }

    /// Keys acknowledged at or after `since`.
    fn acked_since(&self, since: Instant) -> BTreeSet<String> {
        self.writes
            .iter()
            .filter(|w| w.outcome == Outcome::Acked && w.at >= since)
            .map(|w| w.key.clone())
            .collect()
    }
}

/// Keys a copy holds that a client was told were refused.
fn phantoms(ledger: &Ledger, keys: &BTreeSet<String>) -> Vec<String> {
    let refused = ledger.keys(Outcome::Refused);
    keys.intersection(&refused).take(5).cloned().collect()
}

/// Run, and return whether the run passed.
pub(crate) async fn run(
    args: &Args,
    writes_only: bool,
    slow_writes: bool,
    apart: bool,
) -> anyhow::Result<bool> {
    PAUSE_SCALE.store(if slow_writes { 20 } else { 1 }, Ordering::Relaxed);
    DR_NODE.store(if apart { 4 } else { 3 }, Ordering::Relaxed);
    let events = Arc::new(EventLog::create(&args.dir.join("events.jsonl"))?);
    let bucket = "[buckets.ledger]\ndurability = \"disk\"\nscope = \"cluster\"\n".to_string();
    let counter = std::fs::read_to_string(args.root.join("examples/counter/bulkhead.toml"))?
        + "\n[appserver.placement]\nmode = \"singleton\"\n";
    let setup = Setup {
        apps: vec![
            SetupApp {
                name: LEDGER,
                manifest: super::node::native_manifest(&std::fs::read_to_string(
                    args.root.join("examples/ledger/bulkhead.toml"),
                )?)?,
                program: args.root.join("target/release/example-ledger"),
            },
            SetupApp {
                name: SINGLETON,
                manifest: super::node::native_manifest(&counter)?,
                program: args.root.join("target/release/example-counter"),
            },
        ],
        pair_config: [bucket.clone(), bucket.clone()],
        failover_config: format!("dr = {}\n", dr_at() + 1),
        policy: Some(
            "[apps.ledger]\ngrant = [\"bulkhead:keyvalue\"]\nbuckets = [\"ledger\"]\n".into(),
        ),
        witness_config: if apart {
            String::new()
        } else {
            format!("[dr]\npair = [1, 2]\npoll_ms = 200\n{bucket}")
        },
        witness_table: if apart {
            "dr = 4\n".into()
        } else {
            String::new()
        },
        apart_config: apart
            .then(|| format!("[dr]\npair = [1, 2]\npoll_ms = 200\nwitness = 3\n{bucket}")),
    };
    let deadline = Instant::now() + Duration::from_secs_f64(args.minutes * 60.0);
    let mut rng = Rng::new(args.seed);
    let mut ledger = Ledger::default();
    let mut found = Vec::new();
    let mut cycles = 0;
    let mut lost_total = 0;
    let shipped: Option<(bool, Duration)>;
    let watch = Watch::default();
    {
        let mut cluster = Cluster::start(args, &setup, events.clone()).await?;
        set_wan(&cluster, WAN).await;
        let sockets: Vec<PathBuf> = cluster
            .nodes
            .iter()
            .map(|n| n.socket().to_path_buf())
            .collect();
        let watcher = tokio::spawn(watch.clone().run(sockets.clone(), events.clone()));
        let sampler = tokio::spawn(sample(cluster.nodes[dr_at()].client(), events.clone()));
        tokio::time::sleep(args.timings.settle()).await;
        // The DR node must hold the pair's apps before a drill: shipped
        // over the WAN, a chunk at a time (DR3).
        let shipping = Instant::now();
        let held = holds_apps(&cluster.nodes[dr_at()].client()).await;
        note(
            &events,
            format!(
                "the DR node holds the pair's apps: {held}, after {:?}",
                shipping.elapsed()
            ),
        );
        shipped = Some((held, shipping.elapsed()));
        if !held {
            found.push(format!(
                "the DR node did not hold the pair's apps within {HOLD:?} over the WAN"
            ));
        }
        if writes_only && found.is_empty() {
            found.extend(writes_alone(&cluster, &sockets, &mut rng, &mut ledger, deadline).await);
        }
        while !writes_only && Instant::now() < deadline && found.is_empty() {
            cycles += 1;
            match cycle(args, &mut cluster, &sockets, &mut rng, &mut ledger, &events).await {
                Ok((lost, wrong)) => {
                    lost_total += lost;
                    found.extend(wrong);
                }
                Err(e) => found.push(format!("cycle {cycles}: {e:#}")),
            }
            found.extend(watch.take());
        }
        watch.stop();
        sampler.abort();
        let _ = watcher.await;
        found.extend(watch.take());
        // Dropping the cluster kills every node, recorded.
    }
    let acked = ledger.keys(Outcome::Acked).len();
    let mut text = format!(
        "{cycles} cycles, {} writes: {acked} acknowledged, {} unknown, {} refused; {lost_total} \
         acknowledged writes lost at activations, each within the lag reported\n",
        ledger.next,
        ledger.keys(Outcome::Unknown).len(),
        ledger.keys(Outcome::Refused).len(),
    );
    if let Some((held, took)) = shipped {
        let _ = writeln!(
            text,
            "the pair's apps shipped to the DR node over the WAN in {took:.1?}{}",
            if held { "" } else { " (not done)" }
        );
    }
    if acked == 0 {
        found.push("no write was acknowledged".into());
    }
    for p in &found {
        let _ = writeln!(text, "PROBLEM: {p}");
    }
    std::fs::write(args.dir.join("result.txt"), &text)?;
    print!("{text}");
    Ok(found.is_empty())
}

fn note(events: &EventLog, text: String) {
    events.record(0, What::Note { text });
}

/// Note the DR copy's size once a minute (the DR node's memory is sampled
/// beside it, DR3b).
async fn sample(dr: bulkhead::api::Client, events: Arc<EventLog>) {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        if let Ok(Ok(d)) = tokio::time::timeout(ADMIN_TIMEOUT, dr.dr()).await {
            note(
                &events,
                format!(
                    "dr copy: commit {}, base {}, log_bytes {}, snapshotting {}, active {:?}",
                    d.commit, d.base, d.log_bytes, d.snapshotting, d.active_epoch
                ),
            );
        }
    }
}

/// `--writes-only`: writes through site 1 until `deadline`, then the DR
/// copy must catch up with the primary's commit index.
async fn writes_alone(
    cluster: &Cluster,
    sockets: &[PathBuf],
    rng: &mut Rng,
    ledger: &mut Ledger,
    deadline: Instant,
) -> Vec<String> {
    let writer = Writer::start(&sockets[..2], rng, ledger.next);
    tokio::time::sleep_until(deadline.into()).await;
    ledger.absorb(writer.stop().await);
    let mut committed = 0;
    for n in &cluster.nodes[..2] {
        if let Ok(Ok(f)) = tokio::time::timeout(ADMIN_TIMEOUT, n.client().failover()).await
            && let Some(r) = f.replication
        {
            committed = committed.max(r.commit);
        }
    }
    let dr = cluster.nodes[dr_at()].client();
    let caught_up = until(CONVERGE, || async {
        matches!(
            tokio::time::timeout(ADMIN_TIMEOUT, dr.dr()).await,
            Ok(Ok(d)) if d.commit >= committed
        )
    })
    .await;
    if caught_up {
        Vec::new()
    } else {
        vec![format!(
            "the DR copy did not reach the pair's commit index {committed} within {CONVERGE:?}"
        )]
    }
}

/// One drill: the acknowledged writes lost at the activation, and what
/// was found wrong.
async fn cycle(
    args: &Args,
    cluster: &mut Cluster,
    sockets: &[PathBuf],
    rng: &mut Rng,
    ledger: &mut Ledger,
    events: &EventLog,
) -> anyhow::Result<(usize, Vec<String>)> {
    let mut wrong = Vec::new();
    // 1. Writes through site 1, under WAN cuts. Writes lost at an earlier
    // activation stay lost, so the checks below count this cycle's alone.
    let began = Instant::now();
    let writer = Writer::start(&sockets[..2], rng, ledger.next);
    site_1_runs(cluster, rng, events, &mut wrong).await;
    // 2. Site 1 lost while writes flow; 3. activation, which is refused
    // while the DR node does not hold the primary's apps (DR3b): wait for
    // them, as an operator would.
    let dr = cluster.nodes[dr_at()].client();
    if !holds_apps(&dr).await {
        wrong.push(format!(
            "the DR node did not hold the pair's apps within {HOLD:?} before site 1's loss"
        ));
    }
    let lag = dr.dr().await.map(|d| Duration::from_millis(d.lag_ms))?;
    let lost_at = Instant::now();
    cluster.node(1).kill();
    cluster.node(2).kill();
    note(
        events,
        format!("site 1 lost; the DR node reported a lag of {lag:?}"),
    );
    ledger.absorb(writer.stop().await);
    tokio::time::sleep(AFTER_LOSS).await;
    let done = tokio::time::timeout(Duration::from_secs(30), dr.dr_activate(true, false))
        .await
        .map_err(|_| anyhow::anyhow!("activation did not answer in 30 s"))??;
    let activated = Instant::now();
    note(events, done.message);
    let copy = keys_when(sockets[dr_at()].clone(), |_| true).await;
    let window = Window {
        began,
        lost_at,
        lag,
        activated,
    };
    let lost = check_copy(ledger, &window, &copy, &mut wrong);
    // 4. Writes at site 2; 5. site 1 back, reseeding meanwhile.
    let writer = Writer::start(&sockets[dr_at()..=dr_at()], rng, ledger.next);
    tokio::time::sleep(Duration::from_millis(rng.range(5_000, 15_000))).await;
    cluster.node(1).spawn().await?;
    cluster.node(2).spawn().await?;
    let reseeded = until(CONVERGE, || all_reseeded(&sockets[..2])).await;
    ledger.absorb(writer.stop().await);
    if !reseeded {
        wrong.push(format!("site 1 did not reseed within {CONVERGE:?}"));
        return Ok((lost, wrong));
    }
    // 6. Failback.
    let to = u32::try_from(rng.range(1, 3)).unwrap_or(1);
    fail_back(sockets, to, ledger, activated, events, &mut wrong).await?;
    tokio::time::sleep(args.timings.settle()).await;
    note(
        events,
        format!(
            "cycle done: {lost} acknowledged writes lost at the activation (lag {lag:?}); {}",
            if wrong.is_empty() {
                "ok".into()
            } else {
                wrong.join("; ")
            }
        ),
    );
    Ok((lost, wrong))
}

/// When this cycle's writes began, when site 1 was lost and the lag the DR
/// node reported then, and when it was activated.
struct Window {
    began: Instant,
    lost_at: Instant,
    lag: Duration,
    activated: Instant,
}

/// Site 1 runs for a while under WAN cuts; in some cycles an activation is
/// asked meanwhile, which must be refused.
/// Whether the DR node holds both of the pair's apps, ready, with nothing
/// being fetched, within [`HOLD`].
async fn holds_apps(dr: &bulkhead::api::Client) -> bool {
    until(HOLD, || async {
        matches!(
            tokio::time::timeout(ADMIN_TIMEOUT, dr.dr()).await,
            Ok(Ok(d)) if d.deploys.as_ref().is_some_and(|h| h.apps == 2 && h.fetching.is_none() && h.not_ready.is_empty())
        )
    })
    .await
}

async fn site_1_runs(cluster: &Cluster, rng: &mut Rng, events: &EventLog, wrong: &mut Vec<String>) {
    let end = Instant::now() + Duration::from_millis(rng.range(20_000, 45_000));
    let mut ask_refusal = rng.chance(300);
    while Instant::now() < end {
        if ask_refusal {
            ask_refusal = false;
            // Fencing is what is tried here, whatever apps are held.
            match cluster.nodes[dr_at()]
                .client()
                .dr_activate(true, true)
                .await
            {
                Err(e) if e.to_string().contains("site 1 is not down") => {
                    note(
                        events,
                        "an activation with site 1 reachable: refused".into(),
                    );
                }
                other => wrong.push(format!(
                    "an activation with site 1 reaching the witness was not refused: {other:?}"
                )),
            }
        } else if rng.chance(200) {
            let cut = Duration::from_millis(rng.range(1_000, 8_000));
            set_wan(cluster, Mode::Cut).await;
            tokio::time::sleep(cut).await;
            set_wan(cluster, WAN).await;
        }
        tokio::time::sleep(Duration::from_millis(rng.range(500, 2_000))).await;
    }
}

/// Both directions between each pair node and the DR node.
/// The WAN between the sites: site 1's nodes and the witness, and with
/// `--apart` site 1's nodes and the DR node, and the DR node and the
/// witness (as if at a third site).
async fn set_wan(cluster: &Cluster, mode: Mode) {
    let mut links = vec![(1, 3), (2, 3)];
    if DR_NODE.load(Ordering::Relaxed) == 4 {
        links.extend([(1, 4), (2, 4), (3, 4)]);
    }
    for (a, b) in links {
        cluster.set_link(a, b, mode).await;
        cluster.set_link(b, a, mode).await;
    }
}

/// Check the DR copy just after the activation: every write of this cycle
/// acknowledged longer ago than the lag reported (and [`LAG_SLACK`])
/// before the loss is in it, and no refused one. The acknowledged writes
/// it lacks: lost.
fn check_copy(
    ledger: &Ledger,
    w: &Window,
    copy: &BTreeSet<String>,
    wrong: &mut Vec<String>,
) -> usize {
    let must = ledger.acked_between(
        w.began,
        w.lost_at.checked_sub(w.lag + LAG_SLACK).unwrap_or(w.began),
    );
    let missing: Vec<_> = must.difference(copy).take(5).cloned().collect();
    if !missing.is_empty() {
        wrong.push(format!(
            "writes acknowledged before the lag reported ({:?}) are not in the DR copy: \
             {missing:?}",
            w.lag
        ));
    }
    wrong.extend(
        phantoms(ledger, copy)
            .into_iter()
            .map(|k| format!("the DR copy holds a write refused to the client: {k}")),
    );
    ledger
        .acked_between(w.began, w.activated)
        .difference(copy)
        .count()
}

/// Whether both pair nodes are stale and caught up with the DR node.
async fn all_reseeded(sockets: &[PathBuf]) -> bool {
    for socket in sockets {
        let f = tokio::time::timeout(ADMIN_TIMEOUT, bulkhead::api::Client::new(socket).failover())
            .await;
        let ready = matches!(
            f,
            Ok(Ok(f)) if f.replication.as_ref().is_some_and(|r| r.stale == u32::try_from(dr_at() + 1).ok() && r.reseeded)
        );
        if !ready {
            return false;
        }
    }
    true
}

/// Hand back to pair node `to`, then check: both pair nodes hold every
/// write the DR node held and every write acknowledged since the
/// activation, and no refused one; `to` leads; the DR node pulls again.
async fn fail_back(
    sockets: &[PathBuf],
    to: u32,
    ledger: &Ledger,
    activated: Instant,
    events: &EventLog,
    wrong: &mut Vec<String>,
) -> anyhow::Result<()> {
    let dr = bulkhead::api::Client::new(&sockets[dr_at()]);
    let held = keys_when(sockets[dr_at()].clone(), |_| true).await;
    let mut handed = dr.dr_failback(to, 60).await;
    if matches!(&handed, Err(e) if e.to_string().contains("has not reseeded")) {
        tokio::time::sleep(Duration::from_secs(5)).await;
        handed = dr.dr_failback(to, 60).await;
    }
    note(events, format!("failback to node {to}: {handed:?}"));
    handed?;
    let expected: BTreeSet<String> = held
        .union(&ledger.acked_since(activated))
        .cloned()
        .collect();
    for (n, socket) in (1..=2).zip(&sockets[..2]) {
        let want = expected.clone();
        let copy = keys_when(socket.clone(), move |k| k.is_superset(&want)).await;
        let missing: Vec<_> = expected.difference(&copy).take(5).cloned().collect();
        if !missing.is_empty() {
            wrong.push(format!("after failback node {n} lacks {missing:?}"));
        }
        wrong.extend(
            phantoms(ledger, &copy)
                .into_iter()
                .map(|k| format!("node {n} holds a write refused to the client: {k}")),
        );
    }
    let leader = bulkhead::api::Client::new(&sockets[usize::try_from(to - 1).unwrap_or(0)]);
    let leads = until(CONVERGE, || async {
        let f = tokio::time::timeout(ADMIN_TIMEOUT, leader.failover()).await;
        matches!(f, Ok(Ok(f)) if f.role == "primary" && f.may_run)
    })
    .await;
    if !leads {
        wrong.push(format!("node {to} did not lead after the failback"));
    }
    let following = until(CONVERGE, || async {
        matches!(
            tokio::time::timeout(ADMIN_TIMEOUT, dr.dr()).await,
            Ok(Ok(d)) if d.active_epoch.is_none() && !d.stopped
        )
    })
    .await;
    if !following {
        wrong.push("the DR node did not go back to pulling its copy".into());
    }
    Ok(())
}

impl Ledger {
    fn absorb(&mut self, written: Vec<Written>) {
        self.next += written.len() as u64;
        self.writes.extend(written);
    }
}

/// Wait until `done` or `limit`; whether it was done.
async fn until<F, Fut>(limit: Duration, mut done: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if done().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

/// The ledger's keys on the node at `socket`, once `done` holds of them,
/// or as they are after [`CONVERGE`].
async fn keys_when(socket: PathBuf, done: impl Fn(&BTreeSet<String>) -> bool) -> BTreeSet<String> {
    let end = Instant::now() + CONVERGE;
    let mut keys = BTreeSet::new();
    loop {
        let client = bulkhead::api::Client::new(&socket);
        if let Ok(Ok((_, body))) = tokio::time::timeout(
            ADMIN_TIMEOUT,
            client.call(LEDGER, "text/plain", b"keys".to_vec(), 5_000),
        )
        .await
        {
            let text = String::from_utf8_lossy(&body);
            if !text.starts_with("err") {
                keys = text
                    .split(',')
                    .filter(|k| k.starts_with('w'))
                    .map(str::to_string)
                    .collect();
                if done(&keys) {
                    return keys;
                }
            }
        }
        if Instant::now() >= end {
            return keys;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Writes new keys through the nodes given, at random, until stopped.
struct Writer {
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<Vec<Written>>,
}

impl Writer {
    fn start(sockets: &[PathBuf], rng: &mut Rng, first: u64) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(write(
            sockets.to_vec(),
            Rng::new(rng.next_u64()),
            first,
            stop.clone(),
        ));
        Self { stop, task }
    }

    async fn stop(self) -> Vec<Written> {
        self.stop.store(true, Ordering::Relaxed);
        self.task.await.unwrap_or_default()
    }
}

async fn write(
    sockets: Vec<PathBuf>,
    mut rng: Rng,
    first: u64,
    stop: Arc<AtomicBool>,
) -> Vec<Written> {
    let mut out = Vec::new();
    let mut seq = first;
    while !stop.load(Ordering::Relaxed) {
        let n = sockets.len() as u64;
        let socket = &sockets[usize::try_from(rng.range(0, n)).unwrap_or(0)];
        let key = format!("w{seq:07}");
        seq += 1;
        let client = bulkhead::api::Client::new(socket);
        let call = client.call(
            LEDGER,
            "text/plain",
            format!("put {key} {seq}").into_bytes(),
            u64::try_from(WRITE_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
        );
        let reply = match tokio::time::timeout(WRITE_TIMEOUT + ADMIN_TIMEOUT, call).await {
            Ok(Ok((_, body))) => Some(String::from_utf8_lossy(&body).into_owned()),
            _ => None,
        };
        out.push(Written {
            key,
            outcome: outcome(reply.as_deref()),
            at: Instant::now(),
        });
        let scale = PAUSE_SCALE.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(rng.range(20, 200) * scale)).await;
    }
    out
}

/// Which nodes run the singleton, asked of all three a few times a
/// second; every moment two did is a problem.
#[derive(Debug, Clone, Default)]
struct Watch {
    stop: Arc<AtomicBool>,
    found: Arc<Mutex<Vec<String>>>,
}

impl Watch {
    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.found.lock().unwrap_or_else(PoisonError::into_inner))
    }

    async fn run(self, sockets: Vec<PathBuf>, events: Arc<EventLog>) {
        while !self.stop.load(Ordering::Relaxed) {
            let mut running = Vec::new();
            for (n, socket) in (1..).zip(&sockets) {
                let status = tokio::time::timeout(
                    Duration::from_millis(500),
                    bulkhead::api::Client::new(socket).status(SINGLETON),
                )
                .await;
                if matches!(status, Ok(Ok(d)) if d.summary.status == "running") {
                    running.push(n);
                }
            }
            if running.len() > 1 {
                let text = format!("the singleton runs on nodes {running:?} at once");
                note(&events, text.clone());
                self.found
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(text);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

#[cfg(test)]
#[path = "dr_tests.rs"]
mod dr_tests;
