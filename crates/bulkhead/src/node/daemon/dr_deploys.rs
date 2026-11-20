//! Shipping deployments to a disaster-recovery node (ROADMAP Phase 8,
//! DR3): the pair's bundles and deploy state, so that activating the DR
//! node (DR4) needs no redeploy.
//!
//! The DR node pulls, as it pulls the replicated log (DR2), through the
//! same DR frames (`bulkhead_host::Runtime::dr_deploys`). Every ten polls
//! of the log (`[dr] poll_ms`) it asks both pair nodes for their deploy
//! state, naming
//! the digest of the one it holds; each answers `Same` or with its state,
//! and says whether it is the primary. The DR node follows the primary's
//! alone (a standby may be part way through `release upgrade --pair`). It
//! fetches the bundles it lacks, a chunk at a time, by SHA-256, then
//! under the change lock keeps them, takes the primary's state as its own
//! and builds each app as a deploy would, without starting it: the bundle
//! is verified with this node's trusted keys and its grant computed from
//! this node's policy (decided with the user 2026-11-19: policy, config,
//! secrets and keys stay at site 2). An app that does not build here is
//! reported not ready; one whose grant differs from the primary's,
//! reported too. The apps start only when the node is activated.
//!
//! A pair node keeps nothing for the DR node but a cache of its bundles'
//! digests.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, SystemTime};

use bulkhead_host::DrDeploySender;
use bulkhead_host::failover::Standing;
use bulkhead_host::transport::wire::{from_cbor, to_cbor};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{DeployInfo, Node};
use crate::node::error::{NodeError, internal};
use crate::node::store::{AppState, State};

/// The DR node asks for the pair's deploy state once every this many of
/// its polls for the log (`[dr] poll_ms`): every 10 s by default.
const POLLS_PER_ASK: u32 = 10;

/// The DR node asks for the pair's deploy state at most this often.
const MIN_ASK: Duration = Duration::from_secs(1);

/// The most bytes of a bundle in one answer.
const CHUNK: u64 = 1 << 20;

/// How long the DR node waits for a chunk before asking again.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(10);

/// Unanswered asks for a chunk before the fetch is dropped until the next
/// round.
const CHUNK_MISSES: u32 = 3;

/// Where the DR node keeps the state it holds, with the primary's grants.
pub(super) const SHIPPED_FILE: &str = "dr-shipped.json";

/// The node's DR messages about deployments, as CBOR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum DeployMsg {
    /// From the DR node: the sender's deploy state, unless its digest is
    /// `digest`.
    Ask { digest: Option<String> },
    /// The deploy state's digest is the one asked with.
    Same { primary: bool, epoch: u64 },
    /// The sender's deploy state; `primary` if it is the pair's primary,
    /// at failover epoch `epoch`.
    State {
        primary: bool,
        epoch: u64,
        digest: String,
        shipped: Shipped,
    },
    /// From the DR node: the bytes from `offset` of `app`'s `version`,
    /// whose SHA-256 is `sha`.
    AskBundle {
        app: String,
        version: String,
        sha: String,
        offset: u64,
    },
    /// Bytes of a bundle from `offset`, of `total`.
    Chunk {
        app: String,
        version: String,
        sha: String,
        offset: u64,
        total: u64,
        #[serde(with = "bulkhead_host::transport::wire::bytes")]
        bytes: Vec<u8>,
    },
    /// That bundle is no longer here (removed or replaced meanwhile).
    NoBundle {
        app: String,
        version: String,
        sha: String,
    },
}

/// A pair node's deploy state as shipped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Shipped {
    /// Apps in deploy order.
    pub(super) order: Vec<String>,
    pub(super) apps: BTreeMap<String, ShippedApp>,
}

/// One app as shipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ShippedApp {
    pub(super) state: AppState,
    /// Its bundles by version: current, previous, a blue-green new side.
    pub(super) bundles: BTreeMap<String, BundleRef>,
    /// What the pair node's policy granted its current version.
    pub(super) granted: Vec<String>,
}

/// A bundle's SHA-256 (lowercase hex) and size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct BundleRef {
    pub(super) sha: String,
    pub(super) size: u64,
}

/// A bundle the DR node lacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Need {
    pub(super) app: String,
    pub(super) version: String,
    pub(super) bundle: BundleRef,
}

/// Where the DR node's held deployments stand (`bulkhead dr`, metrics).
#[derive(Debug, Clone, Default)]
pub(super) struct HeldStatus {
    /// The pair node whose state it holds.
    pub(super) source: Option<u32>,
    /// When a primary last confirmed it, or it took a new one.
    pub(super) checked: Option<Instant>,
    /// Apps held.
    pub(super) apps: usize,
    /// The bundle being fetched, and how far.
    pub(super) fetching: Option<String>,
    /// Apps that do not build here, with why.
    pub(super) not_ready: BTreeMap<String, String>,
    /// Apps whose grant here differs from the primary's, with how.
    pub(super) grants_differ: BTreeMap<String, String>,
    /// Why the last state shipped was refused.
    pub(super) refused: Option<String>,
}

/// Shared between the node's status and its DR task.
pub(super) type Held = Arc<Mutex<HeldStatus>>;

/// The shipped form of a deploy state: each app's bundles from
/// `bundle` (`None` for a version whose bundle is gone), its grant from
/// `info`.
pub(super) fn shipped(
    state: &State,
    info: &HashMap<String, DeployInfo>,
    mut bundle: impl FnMut(&str, &str) -> Option<BundleRef>,
) -> Shipped {
    let mut apps = BTreeMap::new();
    for (name, app) in &state.apps {
        let mut bundles = BTreeMap::new();
        for version in versions(app) {
            if let Some(b) = bundle(name, &version) {
                bundles.insert(version, b);
            }
        }
        let granted = info
            .get(name)
            .map(|i| i.granted.clone())
            .unwrap_or_default();
        apps.insert(
            name.clone(),
            ShippedApp {
                state: app.clone(),
                bundles,
                granted,
            },
        );
    }
    Shipped {
        order: state.order.clone(),
        apps,
    }
}

/// The versions of an app whose bundles are kept.
fn versions(app: &AppState) -> Vec<String> {
    let mut v = vec![app.current.clone()];
    v.extend(app.previous.clone());
    v.extend(app.new_side.as_ref().map(|n| n.version.clone()));
    v.dedup();
    v
}

/// SHA-256 of the CBOR of `shipped`, as hex.
pub(super) fn digest(shipped: &Shipped) -> String {
    let bytes = to_cbor(shipped).unwrap_or_default();
    hex(ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn is_sha(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A name and version that may form a path in the store.
fn valid_bundle_name(app: &str, version: &str) -> bool {
    bulkhead_bundle::manifest::is_valid_name(app) && semver::Version::parse(version).is_ok()
}

/// Check a state a pair node shipped before any of it forms a path: each
/// app's name and versions valid, its order the apps exactly, each
/// version it keeps shipped, digests well formed, bundles at most `max`
/// bytes.
pub(super) fn check(shipped: &Shipped, max: u64) -> Result<(), String> {
    let ordered: BTreeSet<&String> = shipped.order.iter().collect();
    if ordered.len() != shipped.order.len()
        || ordered != shipped.apps.keys().collect::<BTreeSet<_>>()
    {
        return Err("its deploy order does not list its apps once each".into());
    }
    for (name, app) in &shipped.apps {
        for version in versions(&app.state).iter().chain(app.bundles.keys()) {
            if !valid_bundle_name(name, version) {
                return Err(format!(
                    "'{name}' '{version}' is not an app name and version"
                ));
            }
        }
        if !app.bundles.contains_key(&app.state.current) {
            return Err(format!(
                "{name}: no bundle for its version {}",
                app.state.current
            ));
        }
        for (version, b) in &app.bundles {
            if !is_sha(&b.sha) {
                return Err(format!("{name} {version}: a malformed digest"));
            }
            if b.size > max {
                return Err(format!(
                    "{name} {version}: {} bytes, over the {max} a deploy may be",
                    b.size
                ));
            }
        }
    }
    Ok(())
}

/// The bundles of `shipped` not held: those whose digest `have` does not
/// report the same.
pub(super) fn needs(
    shipped: &Shipped,
    mut have: impl FnMut(&str, &str) -> Option<String>,
) -> Vec<Need> {
    let mut out = Vec::new();
    for name in &shipped.order {
        let Some(app) = shipped.apps.get(name) else {
            continue;
        };
        for (version, b) in &app.bundles {
            if have(name, version).as_deref() != Some(b.sha.as_str()) {
                out.push(Need {
                    app: name.clone(),
                    version: version.clone(),
                    bundle: b.clone(),
                });
            }
        }
    }
    out
}

/// What holding `shipped` in place of `held` removes: apps gone, and
/// versions gone of apps kept.
pub(super) fn gone(held: &State, shipped: &Shipped) -> (Vec<String>, Vec<(String, String)>) {
    let mut apps = Vec::new();
    let mut kept = Vec::new();
    for (name, app) in &held.apps {
        match shipped.apps.get(name) {
            None => apps.push(name.clone()),
            Some(s) => kept.extend(
                versions(app)
                    .into_iter()
                    .filter(|v| !s.bundles.contains_key(v))
                    .map(|v| (name.clone(), v)),
            ),
        }
    }
    (apps, kept)
}

/// How this node's grant `here` differs from the primary's `primary`, if
/// it does.
pub(super) fn grants_differ(primary: &[String], here: &[String]) -> Option<String> {
    let p: BTreeSet<&String> = primary.iter().collect();
    let h: BTreeSet<&String> = here.iter().collect();
    let lacks: Vec<&str> = p.difference(&h).map(|s| s.as_str()).collect();
    let more: Vec<&str> = h.difference(&p).map(|s| s.as_str()).collect();
    match (lacks.is_empty(), more.is_empty()) {
        (true, true) => None,
        (false, true) => Some(format!("not granted here: {}", lacks.join(", "))),
        (true, false) => Some(format!("granted here only: {}", more.join(", "))),
        (false, false) => Some(format!(
            "not granted here: {}; granted here only: {}",
            lacks.join(", "),
            more.join(", ")
        )),
    }
}

/// A bundle's length, modification time and digest, by path.
type DigestCache = HashMap<PathBuf, (u64, Option<SystemTime>, String)>;

/// Bundles' digests, cached by path, length and modification time.
#[derive(Debug, Clone, Default)]
struct Digests(Arc<Mutex<DigestCache>>);

impl Digests {
    /// `path`'s digest and size; blocking.
    fn of(&self, path: &Path) -> std::io::Result<BundleRef> {
        let meta = std::fs::metadata(path)?;
        let (len, mtime) = (meta.len(), meta.modified().ok());
        {
            let cache = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some((l, m, sha)) = cache.get(path)
                && *l == len
                && *m == mtime
            {
                return Ok(BundleRef {
                    sha: sha.clone(),
                    size: len,
                });
            }
        }
        let mut file = std::fs::File::open(path)?;
        let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
        let mut buf = vec![0; 64 << 10];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            ctx.update(&buf[..n]);
        }
        let sha = hex(ctx.finish().as_ref());
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_path_buf(), (len, mtime, sha.clone()));
        Ok(BundleRef { sha, size: len })
    }
}

/// Up to [`CHUNK`] bytes of `path` from `offset`, and its length; blocking.
fn read_chunk(path: &Path, offset: u64) -> std::io::Result<(Vec<u8>, u64)> {
    let mut file = std::fs::File::open(path)?;
    let total = file.metadata()?.len();
    file.seek(std::io::SeekFrom::Start(offset.min(total)))?;
    let mut bytes = Vec::new();
    file.take(CHUNK).read_to_end(&mut bytes)?;
    Ok((bytes, total))
}

/// The pair node's answer to the DR node: whether it is primary, at which
/// epoch.
fn standing(node: &Node) -> (bool, u64) {
    node.rt
        .replication_status()
        .map_or((false, 0), |s| (s.standing == Standing::Primary, s.epoch))
}

impl Node {
    /// The pair's deployments held here, for `bulkhead dr`.
    pub(super) fn held_info(&self) -> crate::api::DrDeploysInfo {
        let h = self
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        crate::api::DrDeploysInfo {
            source: h.source,
            apps: u32::try_from(self.st().desired.apps.len()).unwrap_or(u32::MAX),
            checked_ms: h
                .checked
                .map(|at| u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX)),
            fetching: h.fetching,
            not_ready: h.not_ready,
            grants_differ: h.grants_differ,
            refused: h.refused,
        }
    }

    /// Refuse a change of deployments on a DR node: it holds the primary's.
    pub(super) fn refuse_dr_copy(&self) -> Result<(), NodeError> {
        // Once activated (DR4), its deployments are its own until failback.
        if self.config.dr.is_some() && self.rt.dr_active().is_none() {
            return Err(NodeError::Refused(
                "this node keeps the pair's deployments as its DR copy; deploy to the pair".into(),
            ));
        }
        Ok(())
    }

    /// Ship deployments to the DR node, on a pair node with `failover.dr`,
    /// or hold the pair's, on the DR node, until the node stops.
    pub fn start_dr_deploys(self: &Arc<Self>) {
        let pair_side = self.config.failover.as_ref().and_then(|f| f.dr).is_some();
        let pair = self.config.dr.as_ref().map(|d| {
            let poll = Duration::from_millis(d.poll_ms) * POLLS_PER_ASK;
            (d.pair, poll.max(MIN_ASK))
        });
        if !pair_side && pair.is_none() {
            return;
        }
        let Some((tx, rx)) = self.rt.dr_deploys() else {
            return;
        };
        let (node, stop) = (Arc::downgrade(self), self.stop.subscribe());
        match pair {
            Some((pair, every)) => {
                tokio::spawn(pull(node, pair, every, tx, rx, stop));
            }
            None => {
                tokio::spawn(serve(node, tx, rx, stop));
            }
        }
    }

    /// This node's deploy state as shipped, its bundles' digests from
    /// `digests`.
    async fn shipped_state(&self, digests: &Digests) -> Shipped {
        let (state, info) = {
            let st = self.st();
            (st.desired.clone(), st.info.clone())
        };
        let paths: Vec<(String, String, PathBuf)> = state
            .apps
            .iter()
            .flat_map(|(name, app)| {
                versions(app).into_iter().map(|v| {
                    let path = self.store.bundle_path(name, &v);
                    (name.clone(), v, path)
                })
            })
            .collect();
        let digests = digests.clone();
        let refs = tokio::task::spawn_blocking(move || {
            paths
                .into_iter()
                .filter_map(|(name, v, path)| match digests.of(&path) {
                    Ok(b) => Some(((name, v), b)),
                    Err(e) => {
                        tracing::warn!(app = %name, version = %v, error = %e, "bundle not shipped to the DR node");
                        None
                    }
                })
                .collect::<HashMap<_, _>>()
        })
        .await
        .unwrap_or_default();
        shipped(&state, &info, |name, v| {
            refs.get(&(name.to_string(), v.to_string())).cloned()
        })
    }

    /// Hold `shipped` from pair node `source`: keep its fetched bundles,
    /// take its state as this node's, drop what it no longer lists, and
    /// check each app builds here. Under the change lock.
    async fn hold(
        &self,
        source: u32,
        shipped: &Shipped,
        fetched: Vec<(Need, PathBuf)>,
        held: &Held,
    ) -> Result<(), NodeError> {
        let _change = self.change.lock().await;
        for (need, path) in fetched {
            self.store.keep_upload(&path, &need.app, &need.version)?;
        }
        // Checked before it is taken, so what `bulkhead dr` shows of it is
        // never older than the state listed.
        self.check_held(shipped, held).await;
        let old = self.st().desired.clone();
        let (apps_gone, versions_gone) = gone(&old, shipped);
        let state = State {
            order: shipped.order.clone(),
            apps: shipped
                .apps
                .iter()
                .map(|(name, a)| (name.clone(), a.state.clone()))
                .collect(),
        };
        self.st().desired = state;
        self.save_desired()?;
        let json = serde_json::to_string_pretty(shipped).map_err(internal)?;
        self.store.write_file(SHIPPED_FILE, &json)?;
        for app in &apps_gone {
            self.store.remove_app(app);
            let mut st = self.st();
            st.info.remove(app);
            st.previous_info.remove(app);
            st.new_info.remove(app);
        }
        for (app, version) in &versions_gone {
            self.store.remove_version(app, version);
        }
        let desired = self.st().desired.clone();
        self.store.prune_compiled(&desired);
        // Bundles fetched for a state the primary moved on from.
        self.store.remove_tmp_prefixed("dr-");
        let mut h = held.lock().unwrap_or_else(PoisonError::into_inner);
        h.source = Some(source);
        h.checked = Some(Instant::now());
        h.refused = None;
        self.audit.record(
            "dr_deploys_held",
            json!({ "source": source, "apps": shipped.order, "not_ready": h.not_ready, "grants_differ": h.grants_differ }),
        );
        tracing::info!(
            source,
            apps = shipped.order.len(),
            "holding the pair's deployments"
        );
        Ok(())
    }

    /// Build each held app's current and new-side versions as a deploy
    /// would, without starting them, and compare its grant with the
    /// primary's.
    async fn check_held(&self, shipped: &Shipped, held: &Held) {
        let mut not_ready = BTreeMap::new();
        let mut differ = BTreeMap::new();
        for name in &shipped.order {
            let Some(app) = shipped.apps.get(name) else {
                continue;
            };
            let mut check = vec![app.state.current.clone()];
            check.extend(app.state.new_side.as_ref().map(|n| n.version.clone()));
            for version in check {
                match self.build(&self.store.bundle_path(name, &version)).await {
                    Ok(b) if b.name() != *name || b.version() != version => {
                        not_ready.entry(name.clone()).or_insert(format!(
                            "the bundle for {version} is {} {}",
                            b.name(),
                            b.version()
                        ));
                    }
                    Ok(b) => {
                        if version == app.state.current {
                            if let Some(d) = grants_differ(&app.granted, &b.info.granted) {
                                differ.insert(name.clone(), d);
                            }
                            self.st().info.insert(name.clone(), b.info);
                        }
                    }
                    Err(e) => {
                        not_ready
                            .entry(name.clone())
                            .or_insert(format!("{version}: {e}"));
                    }
                }
            }
        }
        for (app, why) in &not_ready {
            tracing::warn!(%app, reason = %why, "a held app of the pair's would not start here");
        }
        for (app, how) in &differ {
            tracing::warn!(%app, difference = %how, "a held app's grant here differs from the primary's");
        }
        let mut h = held.lock().unwrap_or_else(PoisonError::into_inner);
        h.apps = shipped.apps.len();
        h.not_ready = not_ready;
        h.grants_differ = differ;
    }
}

/// A pair node's side: answer the DR node's asks.
async fn serve(
    node: Weak<Node>,
    tx: DrDeploySender,
    mut rx: mpsc::Receiver<(u32, Vec<u8>)>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let digests = Digests::default();
    loop {
        let (from, body) = tokio::select! {
            _ = stop.wait_for(|s| *s) => return,
            got = rx.recv() => match got {
                Some(m) => m,
                None => return,
            },
        };
        let Some(node) = node.upgrade() else { return };
        let answer = match from_cbor::<DeployMsg>(&body) {
            Ok(DeployMsg::Ask { digest: known }) => {
                let shipped = node.shipped_state(&digests).await;
                let (primary, epoch) = standing(&node);
                let d = digest(&shipped);
                if known.as_deref() == Some(d.as_str()) {
                    DeployMsg::Same { primary, epoch }
                } else {
                    DeployMsg::State {
                        primary,
                        epoch,
                        digest: d,
                        shipped,
                    }
                }
            }
            Ok(DeployMsg::AskBundle {
                app,
                version,
                sha,
                offset,
            }) => {
                if !valid_bundle_name(&app, &version) {
                    tracing::warn!(
                        from,
                        "DR node asked for a bundle by a malformed name; ignored"
                    );
                    continue;
                }
                let path = node.store.bundle_path(&app, &version);
                let (d, want) = (digests.clone(), sha.clone());
                let read = tokio::task::spawn_blocking(move || {
                    if d.of(&path).ok().map(|b| b.sha) != Some(want) {
                        return None;
                    }
                    read_chunk(&path, offset).ok()
                })
                .await
                .ok()
                .flatten();
                match read {
                    Some((bytes, total)) => DeployMsg::Chunk {
                        app,
                        version,
                        sha,
                        offset,
                        total,
                        bytes,
                    },
                    None => DeployMsg::NoBundle { app, version, sha },
                }
            }
            Ok(_) => continue,
            Err(e) => {
                tracing::warn!(from, error = %e, "malformed DR deployment message; dropped");
                continue;
            }
        };
        if let Ok(body) = to_cbor(&answer) {
            // Lost if the DR node is out of reach now; it asks again.
            tx.send(from, body);
        }
    }
}

/// A bundle being fetched.
#[derive(Debug)]
struct Part {
    need: Need,
    path: PathBuf,
    offset: u64,
    asked: Instant,
    misses: u32,
}

/// A deploy state being fetched from pair node `from`.
#[derive(Debug)]
struct Fetch {
    from: u32,
    digest: String,
    shipped: Shipped,
    needs: VecDeque<Need>,
    done: Vec<(Need, PathBuf)>,
    part: Option<Part>,
}

/// The DR node's side: ask, fetch, hold.
struct Puller {
    node: Weak<Node>,
    pair: [u32; 2],
    /// Between asks for the deploy state.
    every: Duration,
    tx: DrDeploySender,
    held: Held,
    digests: Digests,
    /// The digest of the state held.
    holding: Option<String>,
    /// Pair nodes that answered as primary, at which epoch, when.
    claims: HashMap<u32, (u64, Instant)>,
    fetch: Option<Fetch>,
}

async fn pull(
    node: Weak<Node>,
    pair: [u32; 2],
    every: Duration,
    tx: DrDeploySender,
    mut rx: mpsc::Receiver<(u32, Vec<u8>)>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let Some(n) = node.upgrade() else { return };
    let held = n.held.clone();
    let shipped: Option<Shipped> = n
        .store
        .read_file(SHIPPED_FILE)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str(&text).ok());
    if let Some(s) = &shipped {
        let _change = n.change.lock().await;
        n.check_held(s, &held).await;
    }
    drop(n);
    let mut puller = Puller {
        node,
        pair,
        every,
        tx,
        held,
        digests: Digests::default(),
        holding: shipped.as_ref().map(digest),
        claims: HashMap::new(),
        fetch: None,
    };
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut next_round = Instant::now();
    loop {
        // Handled outside the select, so no borrow of `stop` is held
        // across an await.
        let got = tokio::select! {
            _ = stop.wait_for(|s| *s) => return,
            got = rx.recv() => match got {
                Some(m) => Some(m),
                None => return,
            },
            _ = ticker.tick() => None,
        };
        // Activated (DR4): the pair is fenced, and its deployments no
        // longer replace this node's.
        if puller
            .node
            .upgrade()
            .is_none_or(|n| n.rt.dr_active().is_some())
        {
            continue;
        }
        match got {
            Some((from, body)) => match from_cbor::<DeployMsg>(&body) {
                Ok(msg) => puller.answer(from, msg).await,
                Err(e) => {
                    tracing::warn!(from, error = %e, "malformed DR deployment message; dropped");
                }
            },
            None => {
                let now = Instant::now();
                if now >= next_round {
                    next_round = now + puller.every;
                    puller.round();
                }
                puller.timeouts(now);
            }
        }
    }
}

/// Change the held status.
fn note(held: &Held, f: impl FnOnce(&mut HeldStatus)) {
    f(&mut held.lock().unwrap_or_else(PoisonError::into_inner));
}

impl Puller {
    fn send(&self, to: u32, msg: &DeployMsg) {
        if let Ok(body) = to_cbor(msg) {
            self.tx.send(to, body);
        }
    }

    fn round(&self) {
        let ask = DeployMsg::Ask {
            digest: self.holding.clone(),
        };
        for n in self.pair {
            self.send(n, &ask);
        }
    }

    /// Whether pair node `from`, answering as primary at `epoch`, is the
    /// one to follow: no other answered as primary at a higher epoch
    /// lately (within two and a half rounds).
    fn follow(&mut self, from: u32, epoch: u64) -> bool {
        let now = Instant::now();
        let kept = self.every * 5 / 2;
        self.claims.insert(from, (epoch, now));
        !self
            .claims
            .iter()
            .any(|(n, (e, at))| *n != from && *e > epoch && now.duration_since(*at) < kept)
    }

    async fn answer(&mut self, from: u32, msg: DeployMsg) {
        match msg {
            DeployMsg::Same { primary, epoch } => {
                if primary && self.follow(from, epoch) {
                    note(&self.held, |h| {
                        h.source = Some(from);
                        h.checked = Some(Instant::now());
                    });
                }
            }
            DeployMsg::State {
                primary,
                epoch,
                digest: d,
                shipped,
            } => {
                if !primary || !self.follow(from, epoch) {
                    return;
                }
                if self.fetch.as_ref().is_some_and(|f| f.digest == d) {
                    return;
                }
                if digest(&shipped) != d {
                    tracing::warn!(
                        from,
                        "a pair node's deploy state does not match its digest; ignored"
                    );
                    return;
                }
                if let Err(why) = check(&shipped, crate::node::server::MAX_BODY as u64) {
                    tracing::error!(from, reason = %why, "the primary's deploy state refused");
                    note(&self.held, |h| h.refused = Some(why));
                    return;
                }
                self.start_fetch(from, d, shipped).await;
            }
            DeployMsg::Chunk {
                app,
                version,
                sha,
                offset,
                total,
                bytes,
            } => {
                self.chunk(from, (&app, &version, &sha), offset, total, bytes)
                    .await
            }
            DeployMsg::NoBundle { .. } => {
                if self.fetch.as_ref().is_some_and(|f| f.from == from) {
                    // Its state changed meanwhile: the next round says how.
                    self.drop_fetch("the bundle is gone from the primary");
                }
            }
            DeployMsg::Ask { .. } | DeployMsg::AskBundle { .. } => {}
        }
    }

    async fn start_fetch(&mut self, from: u32, digest: String, shipped: Shipped) {
        let Some(node) = self.node.upgrade() else {
            return;
        };
        // Bundles held already, and ones fetched for an earlier state.
        let paths: Vec<(String, String, PathBuf, PathBuf)> = shipped
            .apps
            .iter()
            .flat_map(|(name, a)| {
                a.bundles.iter().map(|(v, b)| {
                    (
                        name.clone(),
                        v.clone(),
                        node.store.bundle_path(name, v),
                        node.store.tmp_path(&format!("dr-{}.rbp", b.sha)),
                    )
                })
            })
            .collect();
        let digests = self.digests.clone();
        let have = tokio::task::spawn_blocking(move || {
            paths
                .into_iter()
                .map(|(name, v, kept, tmp)| {
                    let kept = digests.of(&kept).ok().map(|b| b.sha);
                    let tmp = digests.of(&tmp).ok().map(|b| b.sha);
                    ((name, v), (kept, tmp))
                })
                .collect::<HashMap<_, _>>()
        })
        .await
        .unwrap_or_default();
        let mut done = Vec::new();
        let needs: VecDeque<Need> = needs(&shipped, |name, v| {
            have.get(&(name.to_string(), v.to_string()))
                .and_then(|(kept, _)| kept.clone())
        })
        .into_iter()
        .filter(|n| {
            let fetched = have
                .get(&(n.app.clone(), n.version.clone()))
                .and_then(|(_, tmp)| tmp.as_deref())
                == Some(n.bundle.sha.as_str());
            if fetched {
                done.push((
                    n.clone(),
                    node.store.tmp_path(&format!("dr-{}.rbp", n.bundle.sha)),
                ));
            }
            !fetched
        })
        .collect();
        drop(node);
        tracing::info!(
            from,
            bundles = needs.len(),
            "fetching the primary's deployments"
        );
        self.fetch = Some(Fetch {
            from,
            digest,
            shipped,
            needs,
            done,
            part: None,
        });
        self.next().await;
    }

    /// Ask for the next bundle, or hold the fetched state once every one
    /// is here.
    async fn next(&mut self) {
        let Some(node) = self.node.upgrade() else {
            return;
        };
        let Some(fetch) = self.fetch.as_mut() else {
            return;
        };
        if let Some(need) = fetch.needs.pop_front() {
            let path = node.store.tmp_path(&format!("dr-{}.rbp", need.bundle.sha));
            let _ = std::fs::remove_file(&path);
            let ask = DeployMsg::AskBundle {
                app: need.app.clone(),
                version: need.version.clone(),
                sha: need.bundle.sha.clone(),
                offset: 0,
            };
            let from = fetch.from;
            note(&self.held, |h| {
                h.fetching = Some(format!(
                    "{} {}, 0 of {} bytes",
                    need.app, need.version, need.bundle.size
                ))
            });
            fetch.part = Some(Part {
                need,
                path,
                offset: 0,
                asked: Instant::now(),
                misses: 0,
            });
            self.send(from, &ask);
            return;
        }
        let Some(fetch) = self.fetch.take() else {
            return;
        };
        note(&self.held, |h| h.fetching = None);
        match node
            .hold(fetch.from, &fetch.shipped, fetch.done, &self.held)
            .await
        {
            Ok(()) => self.holding = Some(fetch.digest),
            Err(e) => {
                tracing::error!(error = %e, "the primary's deployments not held");
                note(&self.held, |h| h.refused = Some(e.to_string()));
            }
        }
    }

    async fn chunk(
        &mut self,
        from: u32,
        (app, version, sha): (&str, &str, &str),
        offset: u64,
        total: u64,
        bytes: Vec<u8>,
    ) {
        let Some(fetch) = self.fetch.as_mut() else {
            return;
        };
        let Some(part) = fetch.part.as_mut() else {
            return;
        };
        if fetch.from != from
            || part.need.app != app
            || part.need.version != version
            || part.need.bundle.sha != sha
            || part.offset != offset
        {
            return;
        }
        let end = offset.saturating_add(bytes.len() as u64);
        if total != part.need.bundle.size || end > total || (bytes.is_empty() && end < total) {
            self.drop_fetch("a chunk does not fit the bundle it is of");
            return;
        }
        let path = part.path.clone();
        let wrote = tokio::task::spawn_blocking(move || {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            file.write_all(&bytes)?;
            file.sync_data()
        })
        .await
        .map_err(std::io::Error::other)
        .and_then(|r| r);
        if let Err(e) = wrote {
            tracing::error!(error = %e, "a bundle from the primary not written");
            self.drop_fetch("a chunk not written");
            return;
        }
        part.offset = end;
        part.asked = Instant::now();
        part.misses = 0;
        if end < total {
            let ask = DeployMsg::AskBundle {
                app: app.to_string(),
                version: version.to_string(),
                sha: sha.to_string(),
                offset: end,
            };
            let progress = format!("{app} {version}, {end} of {total} bytes");
            note(&self.held, |h| h.fetching = Some(progress));
            self.send(from, &ask);
            return;
        }
        let Some(part) = fetch.part.take() else {
            return;
        };
        let (digests, path) = (self.digests.clone(), part.path.clone());
        let got = tokio::task::spawn_blocking(move || digests.of(&path).ok().map(|b| b.sha))
            .await
            .ok()
            .flatten();
        if got.as_deref() != Some(sha) {
            let _ = std::fs::remove_file(&part.path);
            self.drop_fetch("a bundle does not match its digest");
            return;
        }
        fetch.done.push((part.need, part.path));
        self.next().await;
    }

    fn timeouts(&mut self, now: Instant) {
        let Some(fetch) = self.fetch.as_mut() else {
            return;
        };
        let Some(part) = fetch.part.as_mut() else {
            return;
        };
        if now.duration_since(part.asked) < CHUNK_TIMEOUT {
            return;
        }
        part.misses += 1;
        if part.misses >= CHUNK_MISSES {
            self.drop_fetch("the primary stopped answering");
            return;
        }
        part.asked = now;
        let ask = DeployMsg::AskBundle {
            app: part.need.app.clone(),
            version: part.need.version.clone(),
            sha: part.need.bundle.sha.clone(),
            offset: part.offset,
        };
        let from = fetch.from;
        self.send(from, &ask);
    }

    fn drop_fetch(&mut self, why: &str) {
        if let Some(f) = self.fetch.take() {
            tracing::warn!(
                from = f.from,
                reason = why,
                "fetching the primary's deployments stopped; trying again next round"
            );
        }
        note(&self.held, |h| h.fetching = None);
    }
}

#[cfg(test)]
#[path = "dr_deploys_tests.rs"]
mod dr_deploys_tests;
