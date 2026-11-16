//! The node: a runtime, its apps, the deploy store and the operations the
//! admin API exposes.

mod blue_green;
mod builtin;
mod cluster;
mod connections;
mod deploy;
mod egress;
mod ingress;
mod metrics;
mod modbus;
mod queries;
#[cfg(feature = "tls")]
mod remote;
mod resolve;
mod sql;
mod upgrade;

pub use blue_green::Traffic;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::api::Done;
use bulkhead_bundle::OperatorPolicy;
use bulkhead_bundle::manifest::Durability;
use bulkhead_host::failover::FailoverHandle;
use bulkhead_host::transport::quic::QuicTransport;
use bulkhead_host::{Apps, HostObjects, Role, Runtime, RuntimeConfig, SecretSource, SharedBucket};
use serde_json::json;

use super::audit::Audit;
use super::config::{BucketScope, ClusterRole, NodeConfig};
use super::error::{NodeError, internal};
use super::events::{self, Events};
use super::gate::Gate;
use super::logring::LogRing;
use super::store::{State, Store};

use ingress::Ingress;

/// A running Bulkhead node.
#[derive(Debug)]
pub struct Node {
    config: NodeConfig,
    rt: Runtime,
    apps: Apps,
    store: Store,
    audit: Audit,
    /// The event stream (`GET /v1/events`), which the audit log feeds too.
    events: Events,
    logs: Arc<LogRing>,
    policy: OperatorPolicy,
    /// Held for the whole of a change (deploy, upgrade, rollback, start,
    /// stop, remove, restore), so changes run one at a time.
    change: tokio::sync::Mutex<()>,
    /// What is deployed and what each deploy decided. Locked only briefly,
    /// never across an await, so reads (`list`, `status`) never wait behind
    /// a change; a change commits to it when it is done.
    state: std::sync::Mutex<NodeState>,
    /// Native apps' listeners, by app: each hands connections to the app's
    /// instances (ROADMAP Z8g). Locked only briefly, never across an
    /// await.
    native_listeners: std::sync::Mutex<HashMap<String, Vec<bulkhead_host::Listener>>>,
    /// Extensions' node-level work, stopped on shutdown.
    /// HTTP ingress, if node config has `[http]` addresses.
    ingress: Ingress,
    /// The resolver of host names (ROADMAP DA9), on a member.
    resolver: resolve::Resolver,
    /// Stops background tasks: certificate checks, the resolver's
    /// reloads.
    stop: tokio::sync::watch::Sender<bool>,
    /// The cluster transport, with `cluster.address`.
    quic: Arc<std::sync::Mutex<Option<QuicTransport>>>,
    /// Who may use the remote admin API (`[admin] operators`).
    #[cfg(feature = "tls")]
    operators: remote::Operators,
    /// The singletons, with `[failover]`.
    gate: Option<Arc<Gate>>,
    /// This node's failover, once started.
    failover: std::sync::OnceLock<FailoverHandle>,
    /// Where a singleton runs while this node is its standby, given to the
    /// runtime (which holds it weakly) for the HTTP ingress driver.
    #[cfg(feature = "http")]
    elsewhere: std::sync::OnceLock<KeptElsewhere>,
    /// Raised by every change of the failover's leadership, for
    /// `GET /v1/failover?after=`.
    leadership: tokio::sync::watch::Sender<u64>,
    /// How the metrics driver's text is rendered, given to the runtime
    /// (which holds it weakly).
    metrics_text: std::sync::OnceLock<KeptMetricsText>,
    /// The server for the admin driver's streams, given to the runtime
    /// (which holds it weakly).
    #[cfg(feature = "tls")]
    admin_streams: std::sync::OnceLock<remote::KeptAdminStreams>,
}

#[derive(Debug, Default)]
struct NodeState {
    desired: State,
    info: HashMap<String, DeployInfo>,
    previous_info: HashMap<String, DeployInfo>,
    /// Blue-green upgrades' new versions, until promoted or dropped.
    new_info: HashMap<String, DeployInfo>,
    /// Apps that should run but could not be restored, with why.
    restore_errors: HashMap<String, String>,
}

/// What a deploy decided, kept for `status`.
#[derive(Debug, Clone, Default)]
struct DeployInfo {
    signer: String,
    granted: Vec<String>,
    denied: Vec<String>,
    warnings: Vec<String>,
    /// The registered names the manifest lists, reserved for this app.
    names: Vec<String>,
}

impl Node {
    fn st(&self) -> std::sync::MutexGuard<'_, NodeState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Write the desired state to the store. Called under the change lock,
    /// so no other change moves it meanwhile.
    fn save_desired(&self) -> Result<(), NodeError> {
        let desired = self.st().desired.clone();
        self.store.save_state(&desired)
    }

    /// Open the deploy store, start the runtime and restore every app that
    /// was running. An app that can no longer be verified or started is
    /// reported as failed, not fatal.
    ///
    /// # Errors
    ///
    /// [`NodeError::Invalid`] for an unreadable policy file;
    /// [`NodeError::Conflict`] if another node has `data_dir` open;
    /// [`NodeError::Internal`] if the store or runtime cannot start.
    pub async fn open(config: NodeConfig, logs: Arc<LogRing>) -> Result<Self, NodeError> {
        // What the node makes is its own (ROADMAP PS2); what an app reads,
        // the privileged helper gives to the app's group.
        #[cfg(unix)]
        rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));
        let store = Store::open(&config.data_dir)?;
        let policy = match &config.policy {
            Some(path) => {
                OperatorPolicy::from_file(path).map_err(|e| NodeError::Invalid(e.to_string()))?
            }
            None => OperatorPolicy::default(),
        };
        if config.trusted_keys.is_empty() {
            tracing::warn!("no trusted keys configured: every deploy will be refused");
        }
        let desired = store.load_state()?;
        if config.cluster.role == ClusterRole::Witness && !desired.apps.is_empty() {
            return Err(NodeError::Invalid(format!(
                "this node is a witness, which runs no apps, but {} holds deployed apps",
                config.data_dir.display()
            )));
        }
        let creation = store.next_creation()?;
        let native = native_config(&config)?;
        let (drivers, ingress_args, modbus) = builtin_drivers(&config)?;
        #[cfg(not(feature = "sql"))]
        if let Some(name) = config.datasources.keys().next() {
            return Err(NodeError::Invalid(format!(
                "datasources.{name}: this build of bulkhead has no SQL (cargo feature `sql`)"
            )));
        }
        let datasources: std::collections::BTreeMap<String, bulkhead_host::SqlDatasource> = config
            .datasources
            .iter()
            .map(|(name, d)| (name.clone(), runtime_datasource(&config, d)))
            .collect();
        let rt = Runtime::new(RuntimeConfig {
            node: config.node,
            max_instances: config.max_instances,
            max_processes: config.max_processes,
            data_dir: Some(store.kv_dir()),
            heartbeat_interval: Duration::from_millis(config.cluster.heartbeat_interval_ms),
            node_timeout: Duration::from_millis(config.cluster.node_timeout_ms),
            creation,
            role: match config.cluster.role {
                ClusterRole::Member => Role::Member,
                ClusterRole::Witness => Role::Witness,
            },
            address: config
                .cluster
                .advertise
                .clone()
                .or_else(|| config.cluster.address.clone())
                .unwrap_or_default(),
            host_objects: HostObjects {
                timers: config.limits.timers_max,
                monitors: config.limits.monitors_max,
                links: config.limits.links_max,
                pending_calls: config.limits.pending_calls_max,
                groups: config.limits.groups_max,
                watches: config.limits.watches_max,
                senders: config.limits.senders_max,
            },
            log_records_per_sec: config.limits.log_records_per_sec,
            incoming_calls_per_peer: config.cluster.incoming_calls_max,
            cast_credit: config.cluster.cast_credit,
            cast_credit_bytes: config.cluster.cast_credit_bytes,
            native,
            drivers,
            // A member's names go to its resolver driver (ROADMAP
            // DA9); a witness resolves none.
            resolver: match config.cluster.role {
                ClusterRole::Member => {
                    bulkhead_host::Resolver::App(crate::node::config::RESOLVE_APP.to_string())
                }
                ClusterRole::Witness => bulkhead_host::Resolver::None,
            },
            secrets: config
                .secrets
                .iter()
                .filter_map(|(name, s)| {
                    let source = match (&s.file, &s.env) {
                        (Some(f), _) => SecretSource::File(f.clone()),
                        (None, Some(v)) => SecretSource::Env(v.clone()),
                        (None, None) => return None,
                    };
                    Some((name.clone(), source))
                })
                .collect(),
            egress: config.egress.runtime(),
            blobs: config.blobs.runtime(),
            modbus: modbus::drivers(modbus.as_ref()),
            datasources: datasources.clone(),
            buckets: config
                .buckets
                .iter()
                .map(|(name, b)| {
                    // A cluster bucket is on disk on both nodes,
                    // replicated between them.
                    let durability = match b.scope {
                        BucketScope::Cluster => Durability::Replicated,
                        BucketScope::Node => b.durability,
                    };
                    let bucket = SharedBucket {
                        durability,
                        quota: b.quota.map(|q| q.bytes()),
                        read_concurrency: b.read_concurrency,
                        publish: b.publish_ms.map(std::time::Duration::from_millis),
                    };
                    (name.clone(), bucket)
                })
                .collect(),
        })
        .map_err(internal)?;
        rt.check_formats()
            .await
            .map_err(|e| NodeError::Invalid(format!("{}: {e}", config.data_dir.display())))?;
        let apps = rt.start_apps(Vec::new()).await.map_err(internal)?;
        let stop = tokio::sync::watch::channel(false).0;
        // Under the helper a driver is a user of its own: its files are
        // given to it before it starts (ROADMAP DA2).
        for e in rt.share_driver_files().await {
            tracing::warn!(error = %e, "a driver's file not shared with it");
        }
        let ingress = Ingress::open(&config, &rt, &apps, ingress_args).await?;
        let resolver = resolve::Resolver::open(&config, &rt, &apps).await?;
        let events = Events::new();
        let audit = Audit::new(store.audit_path(), events.clone());
        let gate = config.failover.is_some().then(|| {
            Arc::new(Gate::new(
                rt.clone(),
                apps.clone(),
                config.cluster_buckets(),
            ))
        });
        let node = Self {
            rt,
            apps,
            store,
            audit,
            events,
            logs,
            policy,
            change: tokio::sync::Mutex::new(()),
            state: std::sync::Mutex::new(NodeState {
                desired,
                ..NodeState::default()
            }),
            native_listeners: std::sync::Mutex::default(),
            ingress,
            resolver,
            stop,
            quic: Arc::default(),
            #[cfg(feature = "tls")]
            operators: Arc::default(),
            gate,
            failover: std::sync::OnceLock::new(),
            #[cfg(feature = "http")]
            elsewhere: std::sync::OnceLock::new(),
            leadership: tokio::sync::watch::channel(0).0,
            metrics_text: std::sync::OnceLock::new(),
            #[cfg(feature = "tls")]
            admin_streams: std::sync::OnceLock::new(),
            config,
        };
        node.resolver.watch(node.stop.subscribe());
        tokio::spawn(events::forward(
            node.events.clone(),
            node.rt.instance_events(),
            node.rt.node_events(),
            node.stop.subscribe(),
        ));
        // The metrics a driver reads: the runtime's and the node's own,
        // kept here while the runtime holds the closure weakly (ROADMAP DA7).
        let render: bulkhead_host::MetricsText = {
            let (rt, quic) = (node.rt.clone(), node.quic.clone());
            Arc::new(move || metrics_text(&rt, &quic))
        };
        node.rt.set_metrics_text(&render);
        let _ = node.metrics_text.set(KeptMetricsText(render));
        metrics::start(&node.config, &node.rt, &node.apps).await?;
        // Before the apps, which may use a datasource as they start.
        sql::start(&node.config, &datasources, &node.rt, &node.apps).await?;
        modbus::start(&node.config, modbus.as_ref(), &node.rt, &node.apps).await?;
        node.restore().await;
        let desired = node.st().desired.clone();
        node.store.prune_compiled(&desired);
        if let Err(e) = node.start_cluster().and_then(|()| node.start_failover()) {
            node.shutdown().await;
            return Err(e);
        }
        Ok(node)
    }

    /// Stop every app, in reverse start order.
    pub async fn shutdown(&self) {
        self.stop.send_replace(true);
        if let Some(gate) = &self.gate {
            gate.close().await;
        }
        let quic = self
            .quic
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(quic) = quic {
            self.rt.leave_cluster();
            quic.shutdown_gracefully().await;
        }
        self.ingress.stop().await;
        let names: Vec<String> = self.native_listener_map().keys().cloned().collect();
        for name in names {
            self.stop_native_listeners(&name);
        }
        self.apps.stop().await;
    }

    /// Every app's metrics and the node's, in the Prometheus text format:
    /// the runtime's, then the HTTP ingress's.
    #[must_use]
    pub fn metrics(&self) -> String {
        metrics_text(&self.rt, &self.quic)
    }

    /// Reload every HTTPS ingress's certificates from their files. An
    /// ingress whose files fail keeps its old certificates.
    ///
    /// # Errors
    ///
    /// [`NodeError::Invalid`] naming each ingress that failed.
    pub async fn reload_tls(&self) -> Result<Done, NodeError> {
        // A file replaced by rename is not yet its driver's under the
        // helper.
        for e in self.rt.share_driver_files().await {
            tracing::warn!(error = %e, "a driver's file not shared with it");
        }
        let (ingresses, mut failed) = self.ingress.reload_tls().await;
        match self.resolver.reload().await {
            Ok(true) => tracing::info!("resolver restarted with changed resolv.conf or hosts"),
            Ok(false) => {}
            Err(e) => failed.push(format!("resolver: {e}")),
        }
        #[cfg(feature = "tls")]
        if let Err(e) = self.reload_operators().await {
            failed.push(e.to_string());
        }
        let quic = self
            .quic
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(quic) = quic
            && let Err(e) = quic.reload()
        {
            failed.push(format!("cluster: {e}"));
        }
        self.audit.record(
            "reload_tls",
            json!({ "ingresses": ingresses, "failed": failed }),
        );
        if !failed.is_empty() {
            return Err(NodeError::Invalid(failed.join("; ")));
        }
        Ok(Done {
            message: format!("reloaded {ingresses} HTTPS ingress(es)"),
        })
    }

    /// The admin socket this node's configuration names.
    #[must_use]
    pub fn socket(&self) -> std::path::PathBuf {
        self.config.socket()
    }
}

/// The node's `elsewhere` closure, kept for the node's life while the
/// runtime holds it weakly.
#[cfg(feature = "http")]
pub(super) struct KeptElsewhere(
    #[allow(dead_code, reason = "held, never read: it keeps the closure alive")]
    pub(super)  bulkhead_host::Elsewhere,
);

#[cfg(feature = "http")]
impl std::fmt::Debug for KeptElsewhere {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeptElsewhere")
    }
}

/// The node's metrics closure, kept for the node's life while the
/// runtime holds it weakly.
pub(super) struct KeptMetricsText(
    #[allow(dead_code, reason = "held, never read: it keeps the closure alive")]
    bulkhead_host::MetricsText,
);

impl std::fmt::Debug for KeptMetricsText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeptMetricsText")
    }
}

/// The runtime's metrics (apps', the HTTP ingress driver's among them),
/// then the cluster transport's.
fn metrics_text(rt: &Runtime, quic: &std::sync::Mutex<Option<QuicTransport>>) -> String {
    let mut text = rt.render_metrics();
    let quic = quic
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(quic) = quic {
        text.push_str(&cluster_metrics(&quic.send_stats()));
    }
    if let Some(s) = rt.replication_status() {
        text.push_str(&replication_metrics(
            s.commit,
            s.last,
            s.partner_matched,
            s.alone,
        ));
    }
    text
}

/// The replicated log (`cluster` buckets) in the Prometheus text format:
/// its commit and last index, and on the primary how far the standby
/// trails and whether it commits alone.
fn replication_metrics(commit: u64, last: u64, matched: Option<u64>, alone: bool) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    let mut gauge = |name: &str, help: &str, value: u64| {
        let _ = writeln!(
            text,
            "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}"
        );
    };
    gauge(
        "bulkhead_replication_commit_index",
        "The last replicated log entry applied to this node's cluster buckets.",
        commit,
    );
    gauge(
        "bulkhead_replication_last_index",
        "The last entry in this node's replicated log.",
        last,
    );
    if let Some(matched) = matched {
        gauge(
            "bulkhead_replication_partner_lag_entries",
            "On the primary, log entries the standby has not acknowledged.",
            last.saturating_sub(matched),
        );
        gauge(
            "bulkhead_replication_alone",
            "On the primary, 1 while it commits without the standby.",
            u64::from(alone),
        );
    }
    text
}

/// Each peer's data lane in the Prometheus text format.
fn cluster_metrics(stats: &[(u32, bulkhead_host::transport::LaneStats)]) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    let _ = writeln!(
        text,
        "# HELP bulkhead_cluster_send_queue_bytes Bytes of messages queued for another node.\n\
         # TYPE bulkhead_cluster_send_queue_bytes gauge"
    );
    for (node, s) in stats {
        let _ = writeln!(
            text,
            "bulkhead_cluster_send_queue_bytes{{node=\"{node}\"}} {}",
            s.queued_bytes
        );
    }
    let _ = writeln!(
        text,
        "# HELP bulkhead_cluster_send_refused_total Messages and notices refused for a congested link to another node.\n\
         # TYPE bulkhead_cluster_send_refused_total counter"
    );
    for (node, s) in stats {
        let _ = writeln!(
            text,
            "bulkhead_cluster_send_refused_total{{node=\"{node}\"}} {}",
            s.refused
        );
    }
    text
}

/// Without the `tls` feature there is no remote admin API.
#[cfg(not(feature = "tls"))]
impl Node {
    /// Refuse `[admin]`: this build has no TLS.
    ///
    /// # Errors
    ///
    /// [`NodeError::Invalid`] if the config has an `[admin]` section.
    #[allow(clippy::unused_async)] // The same signature as with `tls`.
    pub async fn start_remote_admin(self: &Arc<Self>) -> Result<(), NodeError> {
        if self.config.admin.is_some() {
            return Err(NodeError::Invalid(
                "[admin] needs a bulkhead built with the `tls` feature".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod daemon_tests;

/// The host's form of a `[datasources.<name>]` table: its password secret
/// resolved to the secret's source.
fn runtime_datasource(
    config: &NodeConfig,
    d: &crate::node::config::DatasourceBinding,
) -> bulkhead_host::SqlDatasource {
    let mut ds =
        bulkhead_host::SqlDatasource::new(d.host.clone(), d.dbname.clone(), d.user.clone());
    ds.port = d.port;
    ds.password = d
        .password_secret
        .as_ref()
        .and_then(|name| config.secrets.get(name))
        .and_then(|s| match (&s.file, &s.env) {
            (Some(f), _) => Some(SecretSource::File(f.clone())),
            (None, Some(v)) => Some(SecretSource::Env(v.clone())),
            (None, None) => None,
        });
    ds.tls = match (d.tls, &d.ca_file) {
        (crate::node::config::DatasourceTls::Require, Some(ca)) => bulkhead_host::SqlTls::Require {
            ca_file: ca.clone(),
        },
        _ => bulkhead_host::SqlTls::Disable,
    };
    ds.pool_size = d.pool_size;
    ds.connect_timeout = Duration::from_millis(d.connect_timeout_ms);
    ds.acquire_timeout = Duration::from_millis(d.acquire_timeout_ms);
    ds.statement_timeout = Duration::from_millis(d.statement_timeout_ms);
    ds.max_result_bytes = d.max_result_bytes.bytes();
    ds
}

/// The runtime's native settings, with the privileged helper this node
/// was started by when node config sets `[priv]`.
/// Bind each driver app's listening sockets (ROADMAP DA1): the node keeps
/// them for its life, so a driver's restart or upgrade keeps its ports.
fn bind_drivers(
    config: &NodeConfig,
) -> Result<std::collections::BTreeMap<String, bulkhead_host::DriverProfile>, NodeError> {
    config
        .drivers
        .iter()
        .map(|(app, d)| {
            let listeners = d
                .listeners
                .iter()
                .map(|(name, address)| {
                    let socket = std::net::TcpListener::bind(address).map_err(|e| {
                        NodeError::Invalid(format!(
                            "node config: drivers.{app}.listeners.{name}: bind {address}: {e}"
                        ))
                    })?;
                    Ok(bulkhead_host::DriverListener {
                        name: name.clone(),
                        socket: std::sync::Arc::new(socket),
                    })
                })
                .collect::<Result<_, NodeError>>()?;
            Ok((
                app.clone(),
                bulkhead_host::DriverProfile {
                    listeners,
                    files: d.files.clone(),
                    calls: d.calls.clone(),
                    routes: d.routes,
                    calls_routed: false,
                    forward: false,
                    admin: false,
                    metrics: false,
                    connect: None,
                    egress: None,
                    endpoints: Vec::new(),
                    delivers: false,
                },
            ))
        })
        .collect()
}

/// Every driver's profile: node config's `[drivers.<app>]` and the node's
/// own drivers' (their sockets bound now), with the ingress's start
/// arguments and the Modbus drivers' plan.
#[allow(clippy::type_complexity)] // Three results, each used once.
fn builtin_drivers(
    config: &NodeConfig,
) -> Result<
    (
        std::collections::BTreeMap<String, bulkhead_host::DriverProfile>,
        Option<serde_json::Value>,
        Option<modbus::Bound>,
    ),
    NodeError,
> {
    let mut drivers = bind_drivers(config)?;
    if let Some(profile) = metrics::bind(config)? {
        drivers.insert(crate::node::config::METRICS_APP.to_string(), profile);
    }
    #[cfg(feature = "tls")]
    if let Some(profile) = remote::bind(config)? {
        drivers.insert(crate::node::config::ADMIN_APP.to_string(), profile);
    }
    let ingress_args = ingress::bind(config)?.map(|(profile, args)| {
        drivers.insert(crate::node::config::INGRESS_APP.to_string(), profile);
        args
    });
    let modbus = modbus::bind(config, &mut drivers)?;
    Ok((drivers, ingress_args, modbus))
}

fn native_config(config: &NodeConfig) -> Result<bulkhead_host::NativeConfig, NodeError> {
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "freebsd")),
        expect(unused_mut, reason = "the helper is Linux's and FreeBSD's")
    )]
    let mut native = config.native.runtime();
    let started_by_helper = std::env::var_os(bulkhead_priv_wire::SOCKET_ENV).is_some();
    match (config.privileges.is_some(), started_by_helper) {
        (false, false) => {}
        (true, false) => {
            return Err(NodeError::Invalid(
                "node config sets [priv], but this node was not started by bulkhead-priv".into(),
            ));
        }
        (false, true) => {
            return Err(NodeError::Invalid(format!(
                "bulkhead-priv started this node ({} is set), but node config has no [priv]",
                bulkhead_priv_wire::SOCKET_ENV
            )));
        }
        (true, true) => {
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            {
                native.helper = bulkhead_host::PrivHelper::inherited()
                    .map_err(|e| NodeError::Invalid(format!("the privileged helper: {e}")))?;
                tracing::info!("native instances start through the privileged helper");
            }
            #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
            return Err(NodeError::Invalid(
                "the privileged helper runs on Linux and FreeBSD only".into(),
            ));
        }
    }
    Ok(native)
}
