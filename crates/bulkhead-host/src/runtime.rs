//! The node's runtime: instance spawning and the node-wide tables.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use crate::apps::{self, AppDeployment, Apps};
use crate::config::{HostObjects, NativeConfig, RuntimeConfig, SqlDatasource};
use crate::delivery::{self, Origin};
use crate::error::HostError;
use crate::instance::{self, InstanceEvent, InstanceHandle};
use crate::links::{LinkInfo, LinkTable};
use crate::listener::{self, Listener, ListenerSpec};
use crate::message::{CallError, Envelope};
use crate::metrics::Metrics;
use crate::native::{NativeCode, Program};
use crate::pid::Pid;
use crate::pool::Pool;
use crate::registry::Registry;
use crate::services;
use crate::spec::InstanceSpec;
use crate::supervisor::{self, SupervisorHandle, SupervisorSpec};
use crate::table::ProcessTable;
use crate::transport::liveness::{HeartbeatPath, Liveness, NodeEvent, PeerStatus};
use crate::transport::membership::{Member, Membership};
use crate::transport::{self, IncomingCalls, Packet, PendingCalls, Transport};
use bulkhead_bundle::manifest::LogLevel;
use bulkhead_priv_wire::{Reply as PrivReply, VolumeRequest, VolumeRow};

/// Hosts instances on one node.
///
/// Must be used from a multi-threaded tokio runtime: native instances'
/// Where an app that does not run here runs now: the failover partner's
/// node, or `None` ([`Runtime::set_elsewhere`]).
pub type Elsewhere = Arc<dyn Fn(&str) -> Option<u32> + Send + Sync>;

/// [`Elsewhere`] as the runtime holds it.
type WeakElsewhere = std::sync::Weak<dyn Fn(&str) -> Option<u32> + Send + Sync>;

/// Serves the node's end of a stream a driver opened into the admin API
/// ([`Runtime::set_admin_streams`]).
#[cfg(unix)]
pub type AdminStreams = Arc<dyn Fn(std::os::unix::net::UnixStream) + Send + Sync>;

/// [`AdminStreams`] as the runtime holds it.
#[cfg(unix)]
type WeakAdminStreams = std::sync::Weak<dyn Fn(std::os::unix::net::UnixStream) + Send + Sync>;

/// The node's metrics as text ([`Runtime::set_metrics_text`]).
pub type MetricsText = Arc<dyn Fn() -> String + Send + Sync>;

/// [`MetricsText`] as the runtime holds it.
type WeakMetricsText = std::sync::Weak<dyn Fn() -> String + Send + Sync>;

/// A driver's call to `app` on the failover partner, where it runs now
/// (`Request::CallApp`'s `partner`, ROADMAP DA2): `access-denied` unless
/// the driver's profile forwards and its `may_call` lists the app;
/// `noproc` unless the app runs on the partner now.
pub(crate) async fn call_app_partner(
    rt: &RuntimeInner,
    from: &InstanceHandle,
    driver: Option<&crate::config::DriverProfile>,
    app: &str,
    msg: Envelope,
    timeout: Duration,
) -> Result<Envelope, CallError> {
    if !driver.is_some_and(|d| d.forward) || !crate::acl::may_call(&from.acl().may_call, app) {
        return Err(CallError::AccessDenied);
    }
    let node = rt.elsewhere(app).ok_or(CallError::Noproc)?;
    transport::call_app(rt, node, app.to_string(), msg, timeout).await
}

/// waits spin on it (spike S32).
#[derive(Debug, Clone)]
pub struct Runtime {
    inner: Arc<RuntimeInner>,
}

pub(crate) struct RuntimeInner {
    pub(crate) table: ProcessTable,
    pub(crate) registry: Registry,
    pub(crate) links: LinkTable,
    pools: RwLock<HashMap<String, Arc<Pool>>>,
    pub(crate) services: services::Shared,
    /// Apps' metrics and the host's counters.
    pub(crate) metrics: Metrics,
    /// This node's failover role, for `hot-standby` instances.
    failover_role: RwLock<Option<String>>,
    /// The HTTP route table as JSON, for driver apps whose profile asks
    /// for it (ROADMAP DA1); `None` before an ingress sets one.
    http_routes: RwLock<Option<String>>,
    /// The apps that own HTTP routes, in order: the `may_call` of a driver
    /// whose profile says `calls_routed` (ROADMAP DA2).
    route_owners: RwLock<Vec<String>>,
    /// Where an app that does not run here runs now, if on the failover
    /// partner (ROADMAP DA2). Weak: the closure holds the node's parts,
    /// the runtime among them, and its owner keeps it.
    elsewhere: RwLock<Option<WeakElsewhere>>,
    /// Who serves a driver's admin streams, held weakly as `elsewhere` is.
    #[cfg(unix)]
    admin_streams: RwLock<Option<WeakAdminStreams>>,
    /// Who renders the metrics a driver reads, held weakly.
    metrics_text: RwLock<Option<WeakMetricsText>>,

    transport: std::sync::OnceLock<Arc<dyn Transport>>,
    pub(crate) pending_calls: PendingCalls,
    pub(crate) incoming_calls: IncomingCalls,
    /// Credit for waited casts between nodes, both ways (ROADMAP I12).
    pub(crate) credits: crate::credit::Credits,
    pub(crate) liveness: Liveness,
    pub(crate) membership: Membership,
    pub(crate) failover: crate::failover::Shared,
    /// Moved singletons' state, for their first instance's start.
    pub(crate) handovers: crate::handover::Pending,
    /// A pair's version newer than this build, once replication applied
    /// one ([`Runtime::formats_outrun`]).
    pub(crate) formats_outrun: tokio::sync::watch::Sender<Option<u16>>,
    /// Instance exits and supervisor restarts, for whoever listens
    /// ([`Runtime::instance_events`]).
    pub(crate) instance_events: tokio::sync::broadcast::Sender<InstanceEvent>,
    next_id: AtomicU64,
    config: RuntimeConfig,
}

impl RuntimeInner {
    /// This node's id, for pids.
    pub(crate) fn node(&self) -> u32 {
        self.config.node
    }

    /// This start of the node.
    pub(crate) fn creation(&self) -> u32 {
        self.config.creation
    }

    /// This node's failover role, if failover runs.
    pub(crate) fn failover_role(&self) -> Option<String> {
        self.failover_role
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The node `app` runs on now, if it does not run here and runs on
    /// the failover partner.
    pub(crate) fn elsewhere(&self, app: &str) -> Option<u32> {
        let f = self
            .elsewhere
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()?
            .upgrade()?;
        f(app)
    }

    /// A stream into the node's admin API for a driver: the node's end
    /// handed to [`Runtime::set_admin_streams`]'s server, the other
    /// returned to pass to the driver.
    #[cfg(unix)]
    pub(crate) fn admin_stream(&self) -> Result<std::os::unix::net::UnixStream, String> {
        let serve = self
            .admin_streams
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .ok_or("this node serves no admin streams")?;
        let (node, driver) =
            std::os::unix::net::UnixStream::pair().map_err(|e| format!("admin stream: {e}"))?;
        serve(node);
        Ok(driver)
    }

    /// The node's metrics as text: [`Runtime::set_metrics_text`]'s, or the
    /// runtime's own.
    pub(crate) fn metrics_text(self: &Arc<Self>) -> String {
        let render = self
            .metrics_text
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        match render {
            Some(render) => render(),
            None => Runtime {
                inner: self.clone(),
            }
            .render_metrics(),
        }
    }

    /// The HTTP route table as JSON, once an ingress has set one.
    pub(crate) fn http_routes(&self) -> Option<String> {
        self.http_routes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The node's host-object limits.
    pub(crate) fn host_objects(&self) -> HostObjects {
        self.config.host_objects
    }

    /// How the node runs native-process apps.
    pub(crate) fn native_config(&self) -> &NativeConfig {
        &self.config.native
    }

    /// The transport to other nodes, once set.
    pub(crate) fn transport(&self) -> Option<&Arc<dyn Transport>> {
        self.transport.get()
    }

    /// `spec` with the driver profile node config gives its app, if any
    /// (ROADMAP DA1): its listeners and files, and its messaging ACL's
    /// `may_call` from the profile's `calls`. An app named
    /// `bulkhead-egress-<app>` is `<app>`'s egress driver (DA4), with a
    /// profile that serves that app and nothing else.
    pub(crate) fn with_driver(&self, mut spec: InstanceSpec) -> InstanceSpec {
        let egress = spec
            .app
            .strip_prefix(crate::config::EGRESS_DRIVER_PREFIX)
            .filter(|_| !self.config.drivers.contains_key(&spec.app))
            .map(|served| crate::config::DriverProfile {
                egress: Some(served.to_string()),
                // Its identity's certificate and key (ROADMAP DA4b).
                files: self.services.egress().driver_files(served),
                ..crate::config::DriverProfile::default()
            });
        if let Some(profile) = egress
            .as_ref()
            .or_else(|| self.config.drivers.get(&spec.app))
        {
            spec.acl.may_call = if profile.calls_routed {
                crate::acl::only(
                    &self
                        .route_owners
                        .read()
                        .unwrap_or_else(PoisonError::into_inner),
                )
            } else {
                profile.calls.clone()
            };
            spec.driver = Some(profile.clone());
        }
        spec
    }

    /// The messaging ACL of app `name`, if an instance of it runs here.
    pub(crate) fn app_acl(&self, name: &str) -> Option<Arc<crate::acl::Acl>> {
        self.pools
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()?
            .pick(None)
            .map(|h| h.acl())
    }

    /// The instance of the running app called `name` that `msg` routes to
    /// under its `instances.routing`, as for a local caller.
    pub(crate) fn route_app(&self, name: &str, msg: &Envelope) -> Option<InstanceHandle> {
        self.pools
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()?
            .pick(Some(msg))
    }

    /// A live instance of the running app called `name`, round-robin. A
    /// member that has exited is skipped, though its supervisor may not have
    /// noticed yet: its exit notices go out first, and a watcher asking for
    /// the name again on its `down` must not get the dead pid back.
    pub(crate) fn pool_member(&self, name: &str) -> Option<InstanceHandle> {
        let pool = self
            .pools
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()?;
        pool.pick_live(|h| self.table.lookup(h.pid()).is_some())
    }
}

impl fmt::Debug for RuntimeInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeInner")
            .field("config", &self.config)
            .field("instances", &self.table.len())
            .finish_non_exhaustive()
    }
}

/// A native-process app's program, or an in-process test app.
#[derive(Clone)]
pub struct LoadedApp {
    code: Arc<Program>,
}

impl LoadedApp {
    /// The app's program.
    pub(crate) fn native(&self) -> &Arc<Program> {
        &self.code
    }
}

impl fmt::Debug for LoadedApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadedApp").finish_non_exhaustive()
    }
}

impl Runtime {
    /// Create the runtime.
    ///
    /// # Errors
    ///
    /// [`HostError::NativeCgroup`] if
    /// `native.cgroup` is set but the node cannot move itself into it.
    pub fn new(mut config: RuntimeConfig) -> Result<Self, HostError> {
        // Each datasource's driver gets its connection and files (ROADMAP
        // DA3), unless the configuration gives it a profile itself.
        for (name, ds) in &config.datasources {
            config
                .drivers
                .entry(SqlDatasource::driver_app(name))
                .or_insert_with(|| ds.driver_profile());
        }
        if !(1..=crate::pid::MAX_CREATION).contains(&config.creation) {
            return Err(HostError::Engine(format!(
                "creation {} is outside 1..={}",
                config.creation,
                crate::pid::MAX_CREATION
            )));
        }
        if let Some(root) = &config.native.cgroup {
            if !cfg!(target_os = "linux") {
                return Err(HostError::NativeCgroup(format!(
                    "{}: cgroups are Linux's; on FreeBSD memory_max is RLIMIT_DATA \
                     until the privileged helper sets rctl rules (ROADMAP PS1)",
                    root.display()
                )));
            }
            crate::native::cgroup::prepare(root)
                .map_err(|e| HostError::NativeCgroup(format!("{}: {e}", root.display())))?;
        }
        let services = services::Shared::new(&config)?;
        let inner = Arc::new(RuntimeInner {
            table: ProcessTable::default(),
            registry: Registry::default(),
            links: LinkTable::new(config.node),
            pools: RwLock::default(),
            services,
            metrics: Metrics::default(),
            failover_role: RwLock::default(),
            http_routes: RwLock::default(),
            route_owners: RwLock::default(),
            elsewhere: RwLock::default(),
            #[cfg(unix)]
            admin_streams: RwLock::default(),
            metrics_text: RwLock::default(),
            transport: std::sync::OnceLock::new(),
            pending_calls: PendingCalls::default(),
            incoming_calls: IncomingCalls::new(config.incoming_calls_per_peer),
            credits: crate::credit::Credits::new(config.cast_credit, config.cast_credit_bytes),
            liveness: Liveness::new(config.heartbeat_interval, config.node_timeout),
            membership: Membership::new(Member::new(
                config.node,
                config.creation,
                config.address.clone(),
                config.role,
            )),
            failover: crate::failover::Shared::default(),
            handovers: crate::handover::Pending::default(),
            formats_outrun: tokio::sync::watch::channel(None).0,
            instance_events: tokio::sync::broadcast::channel(1024).0,
            next_id: AtomicU64::new(crate::pid::instance_id(config.creation, 1)),
            config,
        });
        inner.services.groups().attach(&inner);
        inner.services.names().attach(&inner);
        inner.services.sql().attach(&inner);
        inner.services.egress().attach(&inner);
        Ok(Self { inner })
    }

    /// A native-process app whose program is `program`: its
    /// instances run it through `bulkhead-runner`. Nothing is checked here;
    /// a program that does not start fails each instance's start.
    #[must_use]
    pub fn load_native(&self, program: NativeCode) -> LoadedApp {
        LoadedApp {
            code: Arc::new(Program::Process(program)),
        }
    }

    /// An in-process test app (cargo feature `test-apps`): its instances
    /// run `A` in their own tasks, over the native protocol's frames
    /// ([`crate::in_process`]).
    #[cfg(feature = "test-apps")]
    #[must_use]
    pub fn load_in_process<A: crate::in_process::TestApp>(&self) -> LoadedApp {
        LoadedApp {
            code: Arc::new(Program::InProcess(
                crate::in_process::InProcessApp::of::<A>(),
            )),
        }
    }

    /// Bind a listener and hand each accepted connection to an instance of
    /// the native app `app`, which takes it in `handle_connection`
    /// ([`crate::listener`]). Must be called inside the tokio runtime.
    ///
    /// # Errors
    ///
    /// [`HostError::Listen`] if the address cannot be bound, or off Linux.
    pub fn listen_native(
        &self,
        listener: ListenerSpec,
        app: crate::AppHandle,
    ) -> Result<Listener, HostError> {
        listener::listen_native(&self.inner, listener, app)
    }

    /// Start a new instance (incarnation 1). It begins in `Starting`; watch
    /// [`InstanceHandle::state`] or await [`InstanceHandle::wait`].
    ///
    /// # Errors
    ///
    /// [`HostError::InstanceLimit`] when every slot is in use, and
    /// [`HostError::HostObjectsAboveNode`] when a host-object limit exceeds
    /// the node's.
    pub fn spawn(&self, app: &LoadedApp, spec: InstanceSpec) -> Result<InstanceHandle, HostError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.start(app, spec, id, 1)
    }

    /// Start the next incarnation of an instance that has exited: same id,
    /// incarnation one higher.
    ///
    /// # Errors
    ///
    /// As for [`Runtime::spawn`], and [`HostError::StillRunning`] if an
    /// instance with that id is still alive.
    pub fn respawn(
        &self,
        app: &LoadedApp,
        spec: InstanceSpec,
        previous: Pid,
    ) -> Result<InstanceHandle, HostError> {
        self.start(
            app,
            spec,
            previous.id,
            previous.incarnation.saturating_add(1),
        )
    }

    /// Start a supervisor and its children (none yet for
    /// `simple_one_for_one`; add them with
    /// [`SupervisorHandle::start_child`]).
    ///
    /// # Errors
    ///
    /// [`HostError::InvalidSupervisor`] for an invalid spec, and the errors
    /// of [`Runtime::spawn`] if a child cannot start; children already
    /// started are then killed.
    pub fn start_supervisor(&self, spec: SupervisorSpec) -> Result<SupervisorHandle, HostError> {
        supervisor::start(self, spec)
    }

    /// The live instance with exactly this pid. `None` for a dead pid or an
    /// earlier incarnation.
    #[must_use]
    pub fn lookup(&self, pid: Pid) -> Option<InstanceHandle> {
        self.inner.table.lookup(pid)
    }

    /// The live instance registered under `name`. A name no instance holds
    /// but that names a running app resolves to one of the app's instances,
    /// round-robin.
    #[must_use]
    pub fn whereis(&self, name: &str) -> Option<InstanceHandle> {
        match self.inner.registry.whereis(name) {
            Some(pid) => self.lookup(pid),
            None => self.inner.pool_member(name),
        }
    }

    /// Start a set of apps in dependency order, each under its own root
    /// supervisor. An app that cannot start, or whose dependency is not
    /// running, is marked failed and the rest still start.
    ///
    /// # Errors
    ///
    /// [`HostError::InvalidApps`] before anything starts, for a missing or
    /// mismatched dependency, a cycle, a duplicate name, or a supervision
    /// child naming an app that is not deployed.
    pub async fn start_apps(&self, deployments: Vec<AppDeployment>) -> Result<Apps, HostError> {
        apps::start(self, deployments).await
    }

    /// Override the log level of every instance of `app` from now on, or
    /// return to each instance's own level with `None`.
    pub fn set_log_level(&self, app: &str, level: Option<LogLevel>) {
        self.inner.services.logging.set_override(app, level);
        // Each instance works out its own effective level and tells its
        // guest (`log-level` host event).
        for instance in self.instances() {
            if instance.app() == app {
                instance.announce();
            }
        }
    }

    /// The log level override for `app`, if one is set.
    #[must_use]
    pub fn log_level(&self, app: &str) -> Option<LogLevel> {
        self.inner.services.logging.override_for(app)
    }

    pub(crate) fn add_pool(&self, name: &str, pool: Arc<Pool>) {
        self.inner
            .pools
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name.to_string(), pool);
    }

    pub(crate) fn remove_pool(&self, name: &str) {
        self.inner
            .pools
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(name);
    }

    /// Cast to a pid.
    ///
    /// # Errors
    ///
    /// [`CallError::Noproc`] for a dead pid or an earlier incarnation, and as
    /// for [`InstanceHandle::cast`].
    pub fn cast(&self, to: Pid, msg: Envelope) -> Result<(), CallError> {
        delivery::cast(&self.inner, Origin::Host, to, msg)
    }

    /// Cast to a pid, waiting up to `timeout` for room if its mailbox is
    /// full, on this node or another (as a native app's `cast_timeout`):
    /// `Ok` means the message is in the target's mailbox. For embedders
    /// and tests; the admin API and the HTTP ingress do not wait.
    ///
    /// # Errors
    ///
    /// [`CallError::Timeout`] if no room came in time (the cast was not
    /// queued; between nodes, unless the answer itself was late), and as
    /// for [`Runtime::cast`].
    pub async fn cast_timeout(
        &self,
        to: Pid,
        msg: Envelope,
        timeout: Duration,
    ) -> Result<(), CallError> {
        delivery::cast_wait(&self.inner, Origin::Host, to, msg, timeout).await
    }

    /// `host`'s addresses on `port`, as apps' `connect_host`, egress and
    /// datasources get them: through [`RuntimeConfig::resolver`]
    /// (ROADMAP DA9), within `timeout`.
    ///
    /// # Errors
    ///
    /// [`ResolveError`](crate::ResolveError): no such name, no answer in
    /// time, or no resolver.
    pub async fn resolve(
        &self,
        host: &str,
        port: u16,
        timeout: Duration,
    ) -> Result<Vec<std::net::SocketAddr>, crate::ResolveError> {
        self.inner
            .services
            .names()
            .resolve(host, port, timeout)
            .await
    }

    /// Call the app called `app` on node `node`, from this node's host. That
    /// node's pool for the app picks the instance under its
    /// `instances.routing`, as for one of its own callers.
    ///
    /// # Errors
    ///
    /// [`CallError::Noconnection`] if `node` cannot be reached,
    /// [`CallError::Noproc`] if the app does not run there, and as for
    /// [`InstanceHandle::call`].
    pub async fn call_app_on(
        &self,
        node: u32,
        app: &str,
        msg: Envelope,
        timeout: Duration,
    ) -> Result<Envelope, CallError> {
        if node == self.inner.node() {
            let target = self.inner.route_app(app, &msg).ok_or(CallError::Noproc)?;
            return target.call(msg, timeout).await;
        }
        transport::call_app(&self.inner, node, app.to_string(), msg, timeout).await
    }

    /// Call a pid.
    ///
    /// # Errors
    ///
    /// [`CallError::Noproc`] for a dead pid or an earlier incarnation, and as
    /// for [`InstanceHandle::call`].
    pub async fn call(
        &self,
        to: Pid,
        msg: Envelope,
        timeout: Duration,
    ) -> Result<Envelope, CallError> {
        delivery::call(&self.inner, Origin::Host, to, msg, timeout).await
    }

    pub(crate) fn inner(&self) -> &Arc<RuntimeInner> {
        &self.inner
    }

    /// This node's id, carried in every local pid.
    #[must_use]
    pub fn node(&self) -> u32 {
        self.inner.node()
    }

    /// Carry frames for pids on other nodes through `transport`
    /// ([`crate::transport`]). Set once; without one, every other node is
    /// unreachable (`noconnection`).
    ///
    /// # Errors
    ///
    /// [`HostError::Engine`] if a transport is already set.
    pub fn set_transport(&self, transport: Arc<dyn Transport>) -> Result<(), HostError> {
        self.inner
            .transport
            .set(transport)
            .map_err(|_| HostError::Engine("a transport is already set".into()))
    }

    /// Handle a packet that node `from` sent to this node. Called by the
    /// transport; must run inside the tokio runtime.
    pub fn deliver(&self, from: u32, packet: Packet) {
        transport::deliver(&self.inner, from, packet);
    }

    /// Node `node` cannot be reached: the session with it ends, so links and
    /// monitors with its pids end with `noconnection` and calls waiting on
    /// it fail. Called by a transport that detects loss itself; heartbeats
    /// ([`transport::liveness`]) detect it otherwise.
    pub fn node_down(&self, node: u32) {
        transport::end_session(&self.inner, node);
    }

    /// Send heartbeats to `node` and watch it. Starts the heartbeat task on
    /// first use, so call it inside the tokio runtime.
    pub fn add_peer(&self, node: u32) {
        self.inner.liveness.add_peer(&self.inner, node);
    }

    /// Also beat every peer on `path`, reported as `name` in
    /// [`NodeEvent`]s.
    pub fn add_heartbeat_path(&self, name: &str, path: Arc<dyn HeartbeatPath>) {
        self.inner.liveness.add_path(name, path);
    }

    /// A beat from `node` carrying `payload` arrived on the extra path
    /// `path`. Called by the path's receiving side.
    pub fn heard(&self, node: u32, path: &str, payload: &[u8]) {
        self.inner.liveness.on_beat(node, path, payload);
    }

    /// Changes in what this node knows about others, from now on.
    #[must_use]
    pub fn node_events(&self) -> tokio::sync::broadcast::Receiver<NodeEvent> {
        self.inner.liveness.subscribe()
    }

    /// Instance exits and supervisor restarts on this node, from now on. A
    /// listener that falls more than 1024 events behind loses the oldest
    /// (`RecvError::Lagged`).
    #[must_use]
    pub fn instance_events(&self) -> tokio::sync::broadcast::Receiver<InstanceEvent> {
        self.inner.instance_events.subscribe()
    }

    /// Tell listeners about `event`.
    pub(crate) fn instance_event(&self, event: InstanceEvent) {
        let _ = self.inner.instance_events.send(event);
    }

    /// Join the cluster through `seeds` (node ids the transport reaches):
    /// each is told about this node and answers with the members it knows,
    /// and this node then joins each of those
    /// ([`transport::membership`]). Call inside the tokio runtime, after
    /// [`Runtime::set_transport`].
    pub fn join_cluster(&self, seeds: &[u32]) {
        transport::join(&self.inner, seeds);
    }

    /// Leave the cluster: every member is told, and every session ends.
    pub fn leave_cluster(&self) {
        transport::leave(&self.inner);
    }

    /// The members this node knows, itself included, by node id.
    #[must_use]
    pub fn members(&self) -> Vec<Member> {
        self.inner.membership.list()
    }

    /// What this node knows about `node`, if it watches it.
    #[must_use]
    pub fn peer_status(&self, node: u32) -> Option<PeerStatus> {
        self.inner.liveness.status(node)
    }

    /// `pid`'s links and monitors as this node records them; for a pid on
    /// another node, the entries this node keeps with it.
    #[must_use]
    pub fn links(&self, pid: Pid) -> LinkInfo {
        self.inner.links.info(pid)
    }

    /// Every live instance, ordered by pid.
    #[must_use]
    pub fn instances(&self) -> Vec<InstanceHandle> {
        self.inner.table.all()
    }

    /// The init arguments of `app`'s egress driver, `bulkhead-egress-<app>`
    /// (ROADMAP DA4): node config's egress settings and the trusted roots
    /// as PEM. `None` without the `egress` feature.
    #[must_use]
    pub fn egress_driver_args(&self, app: &str) -> Option<serde_json::Value> {
        self.inner.services.egress().driver_args(app)
    }

    /// Give each driver app the files its profile names, read-only, to its
    /// group (ROADMAP DA2): under the privileged helper a driver runs as a
    /// user of its own, and its certificate and key files are the node's
    /// user's. Without the helper this does nothing. Each file that could
    /// not be shared, as `<app>: <why>`; the driver then reads it only if
    /// its mode lets everyone. Again before a driver reloads its files: one
    /// replaced by rename is a new file.
    pub async fn share_driver_files(&self) -> Vec<String> {
        let mut failed = Vec::new();
        for (app, profile) in &self.inner.config.drivers {
            for file in &profile.files {
                if let Err(e) = self.share_with_app(app, file, false).await {
                    failed.push(format!("{app}: {e}"));
                }
            }
        }
        // Each egress driver's identity (ROADMAP DA4b).
        for (app, identity) in &self.inner.config.egress.identities {
            let driver = crate::config::egress_driver_app(app);
            for file in [&identity.cert, &identity.key] {
                if let Err(e) = self.share_with_app(&driver, file, false).await {
                    failed.push(format!("{driver}: {e}"));
                }
            }
        }
        failed
    }

    /// Read egress's trusted roots again (ROADMAP DA4b), for the drivers
    /// started from now on; whether they changed. A failure keeps the
    /// roots read before.
    ///
    /// # Errors
    ///
    /// The CA file cannot be read or holds no certificate.
    pub fn reload_egress_roots(&self) -> Result<bool, String> {
        self.inner.services.egress().reload_roots()
    }

    /// Give `path`, a directory the node wrote for `app`, to that app's
    /// user (ROADMAP PS2), through the privileged helper: read-only, to its
    /// group (the program's and `priv/` directories, a driver's files);
    /// read-write, the directory itself (a volume). Without the helper the app runs as the
    /// node's user and this does nothing.
    ///
    /// # Errors
    ///
    /// The helper's refusal, or the helper gone.
    pub async fn share_with_app(
        &self,
        app: &str,
        path: &std::path::Path,
        writable: bool,
    ) -> Result<(), String> {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if let Some(helper) = &self.inner.config.native.helper {
            let path = path
                .to_str()
                .ok_or_else(|| format!("{} is not UTF-8, as the helper needs", path.display()))?;
            return helper.share(app, path, writable).await;
        }
        let _ = (app, path, writable);
        Ok(())
    }

    /// Make sure `app`'s ZFS volume `volume` exists with a quota of
    /// `refquota` bytes, mounted and its root the app's user's (ROADMAP
    /// ZV1), through the privileged helper; its mountpoint.
    ///
    /// # Errors
    ///
    /// No helper, or the helper refused (a quota over its ceiling or below
    /// what the volume holds, a `zfs` failure) or has gone.
    pub async fn ensure_volume(
        &self,
        app: &str,
        volume: &str,
        refquota: u64,
    ) -> Result<std::path::PathBuf, String> {
        let request = VolumeRequest::Ensure {
            app: app.to_string(),
            volume: volume.to_string(),
            refquota,
        };
        match self.volume_request(request).await? {
            PrivReply::Volume { mountpoint } => Ok(mountpoint.into()),
            other => Err(format!("the privileged helper answered {other:?}")),
        }
    }

    /// The ZFS volumes the privileged helper keeps (ROADMAP ZV1): its
    /// parent dataset and each app's volumes.
    ///
    /// # Errors
    ///
    /// No helper, or the helper refused or has gone.
    pub async fn volumes(&self) -> Result<(String, Vec<VolumeRow>), String> {
        match self.volume_request(VolumeRequest::List).await? {
            PrivReply::Volumes { parent, rows } => Ok((parent, rows)),
            other => Err(format!("the privileged helper answered {other:?}")),
        }
    }

    /// Remove `app`'s ZFS volume `volume` and its snapshots (ROADMAP ZV1).
    ///
    /// # Errors
    ///
    /// No helper, or the helper refused (an instance of the app runs, no
    /// such volume, a `zfs` failure) or has gone.
    pub async fn destroy_volume(&self, app: &str, volume: &str) -> Result<(), String> {
        let request = VolumeRequest::Destroy {
            app: app.to_string(),
            volume: volume.to_string(),
        };
        self.volume_done(request).await
    }

    /// Snapshot `app`'s ZFS volume `volume` as `@upgrade` before an
    /// upgrade (ROADMAP ZV3), replacing the last one.
    ///
    /// # Errors
    ///
    /// No helper, or the helper refused (no such volume, a `zfs` failure)
    /// or has gone.
    pub async fn snapshot_volume(&self, app: &str, volume: &str) -> Result<(), String> {
        let request = VolumeRequest::Snapshot {
            app: app.to_string(),
            volume: volume.to_string(),
        };
        self.volume_done(request).await
    }

    /// Roll `app`'s ZFS volume `volume` back to its `@upgrade` snapshot
    /// (ROADMAP ZV3), discarding what was written since.
    ///
    /// # Errors
    ///
    /// No helper, or the helper refused (an instance of the app runs, no
    /// such volume or snapshot, a newer snapshot) or has gone.
    pub async fn rollback_volume(&self, app: &str, volume: &str) -> Result<(), String> {
        let request = VolumeRequest::Rollback {
            app: app.to_string(),
            volume: volume.to_string(),
        };
        self.volume_done(request).await
    }

    async fn volume_done(&self, request: VolumeRequest) -> Result<(), String> {
        match self.volume_request(request).await? {
            PrivReply::Done => Ok(()),
            other => Err(format!("the privileged helper answered {other:?}")),
        }
    }

    async fn volume_request(&self, request: VolumeRequest) -> Result<PrivReply, String> {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if let Some(helper) = &self.inner.config.native.helper {
            return helper.volume(request).await;
        }
        let _ = request;
        Err("ZFS volumes need the privileged helper ([priv])".into())
    }

    /// Keep the HTTP route table (JSON, `GUEST_FORMATS.md`'s "Routes") and
    /// its apps, give them as `may_call` to the running drivers whose
    /// profile says `calls_routed`, and announce the table to the driver
    /// apps whose profile asks for it (host event `routes`, ROADMAP DA1).
    pub(crate) fn set_http_routes(&self, json: String, owners: Vec<String>) {
        *self
            .inner
            .http_routes
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(json);
        let may_call = crate::acl::only(&owners);
        *self
            .inner
            .route_owners
            .write()
            .unwrap_or_else(PoisonError::into_inner) = owners;
        for instance in self.instances() {
            let routed = self
                .inner
                .config
                .drivers
                .get(instance.app())
                .is_some_and(|d| d.calls_routed);
            if routed {
                instance.set_may_call(may_call.clone());
            }
            instance.announce();
        }
    }

    /// Say where an app that does not run here runs now: the failover
    /// partner's node for an app it runs and this node does not (ROADMAP
    /// DA2). Asked when a driver redirects or forwards a request for an
    /// app that does not run here (`Request::Elsewhere`, a `CallApp` with
    /// `partner`), so it must be cheap. The runtime keeps a weak
    /// reference: the caller keeps `elsewhere` for as long as it should
    /// answer, so a closure holding this runtime makes no cycle.
    pub fn set_elsewhere(&self, elsewhere: &Elsewhere) {
        *self
            .inner
            .elsewhere
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::downgrade(elsewhere));
    }

    /// Serve the streams drivers open into the admin API (ROADMAP DA7):
    /// `serve` gets the node's end of each and must answer only requests
    /// an operator signed. Held weakly, as [`Runtime::set_elsewhere`]'s.
    #[cfg(unix)]
    pub fn set_admin_streams(&self, serve: &AdminStreams) {
        *self
            .inner
            .admin_streams
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::downgrade(serve));
    }

    /// Render the metrics a driver reads (ROADMAP DA7), in place of
    /// [`Runtime::render_metrics`]: the node adds its own. Held weakly.
    pub fn set_metrics_text(&self, render: &MetricsText) {
        *self
            .inner
            .metrics_text
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::downgrade(render));
    }

    /// Set this node's failover role (`primary`, `standby`, `down`) and tell
    /// every `hot-standby` instance ([`InstanceSpec::announce_role`]).
    pub fn set_failover_role(&self, role: Option<&str>) {
        *self
            .inner
            .failover_role
            .write()
            .unwrap_or_else(PoisonError::into_inner) = role.map(str::to_string);
        for instance in self.instances() {
            instance.announce();
        }
    }

    /// How many blobs of `app` the node holds, and their bytes
    /// (`bulkhead:blobs`, ROADMAP BL1).
    #[must_use]
    pub fn blob_usage(&self, app: &str) -> (usize, u64) {
        self.inner.services.blobs().usage(app)
    }

    /// Every app's metrics and the host's, in the Prometheus text format.
    #[must_use]
    pub fn render_metrics(&self) -> String {
        self.inner.metrics.render(&self.instances())
    }

    /// Number of live instances.
    #[must_use]
    pub fn instance_count(&self) -> usize {
        self.inner.table.len()
    }

    fn start(
        &self,
        app: &LoadedApp,
        spec: InstanceSpec,
        id: u64,
        incarnation: u32,
    ) -> Result<InstanceHandle, HostError> {
        let spec = self.inner.with_driver(spec);
        if let Some(p) = spec.preopens.iter().find(|p| !p.host.is_dir()) {
            return Err(HostError::Preopen(format!(
                "{} (for {}) is not a directory",
                p.host.display(),
                p.guest
            )));
        }
        self.inner
            .config
            .host_objects
            .grant(&spec.host_objects)
            .map_err(HostError::HostObjectsAboveNode)?;
        let pid = Pid {
            node: self.inner.config.node,
            id,
            incarnation,
        };
        let (handle, task) = instance::prepare(pid, &spec, self.inner.metrics.stats(&spec.app));
        let config = &self.inner.config;
        let processes = config.max_processes.unwrap_or(config.max_instances);
        let capacity = usize::try_from(processes).unwrap_or(usize::MAX);
        self.inner
            .table
            .insert(handle.clone(), capacity)
            .map_err(|holder| match holder {
                Some(pid) => HostError::StillRunning(pid),
                None => HostError::InstanceLimit(processes),
            })?;
        tokio::spawn(task.run(self.inner.clone(), app.clone(), spec));
        Ok(handle)
    }
}
