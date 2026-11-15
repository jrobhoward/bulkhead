//! Node-level runtime configuration.

use std::collections::BTreeMap;
use std::path::PathBuf;

use bulkhead_bundle::manifest::Durability;
use std::time::Duration;

use crate::transport::membership::Role;

/// Most host objects one instance may hold. These live in host memory,
/// outside `memory_max` (a pending timer holds its envelope), so each is
/// charged to the instance that creates it. Over a limit, the instance
/// crashes with `timer_limit`, `monitor_limit`, `link_limit` or
/// `pending_call_limit`; a `bulkhead:groups` join or a `bulkhead:watch` watch
/// over its limit returns `limit-reached`. Registered names need no limit: an instance may
/// hold only the names its manifest lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostObjects {
    /// Pending `send-after` timers.
    pub timers: u32,
    /// Monitors held on other instances.
    pub monitors: u32,
    /// Links, counted when the instance links again. Links others make to
    /// it are charged to them and never refused.
    pub links: u32,
    /// Calls answered `noreply` and not yet replied to.
    pub pending_calls: u32,
    /// Process groups joined (`bulkhead:groups`).
    pub groups: u32,
    /// Keyvalue watches held (`bulkhead:watch`).
    pub watches: u32,
    /// Sender channels a native instance holds at once (ROADMAP AP1).
    pub senders: u32,
}

impl Default for HostObjects {
    /// 10,000 of each, the default `mailbox_max`, 1,000 groups and
    /// watches, and 8 sender channels.
    fn default() -> Self {
        Self {
            timers: 10_000,
            monitors: 10_000,
            links: 10_000,
            pending_calls: 10_000,
            groups: 1_000,
            watches: 1_000,
            senders: 8,
        }
    }
}

impl HostObjects {
    /// What an instance asking for `requested` gets on a node whose limits
    /// are `self`: an unset field takes the node's value.
    ///
    /// # Errors
    ///
    /// A message naming the first field that asks for more than the node
    /// allows.
    pub fn grant(&self, requested: &HostObjectRequest) -> Result<Self, String> {
        let pick = |name: &str, asked: Option<u32>, node: u32| match asked {
            None => Ok(node),
            Some(n) if n <= node => Ok(n),
            Some(n) => Err(format!("{name} of {n} exceeds the node limit of {node}")),
        };
        Ok(Self {
            timers: pick("timers_max", requested.timers, self.timers)?,
            monitors: pick("monitors_max", requested.monitors, self.monitors)?,
            links: pick("links_max", requested.links, self.links)?,
            pending_calls: pick(
                "pending_calls_max",
                requested.pending_calls,
                self.pending_calls,
            )?,
            groups: pick("groups_max", requested.groups, self.groups)?,
            watches: pick("watches_max", requested.watches, self.watches)?,
            senders: pick("senders_max", requested.senders, self.senders)?,
        })
    }
}

/// The host-object limits an instance asks for (manifest `limits`); an
/// unset field takes the node's value from [`HostObjects`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HostObjectRequest {
    /// `timers_max`.
    pub timers: Option<u32>,
    /// `monitors_max`.
    pub monitors: Option<u32>,
    /// `links_max`.
    pub links: Option<u32>,
    /// `pending_calls_max`.
    pub pending_calls: Option<u32>,
    /// `groups_max`.
    pub groups: Option<u32>,
    /// `watches_max`.
    pub watches: Option<u32>,
    /// `senders_max`.
    pub senders: Option<u32>,
}

/// A shared keyvalue bucket: named in node configuration, opened by every
/// app whose manifest lists it and whose grant includes it. Its scope is
/// this node (a `cluster` scope is Phase 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SharedBucket {
    /// `Memory` or `Disk`; `Replicated` buckets fail every operation until
    /// Phase 3.
    pub durability: Durability,
    /// Most bytes the bucket may hold, across every app writing it.
    pub quota: Option<u64>,
    /// Reads take no lock (memory buckets; spike S26): see the manifest's
    /// `read_concurrency`.
    pub read_concurrency: bool,
    /// Published (ROADMAP T4): checked for changes this often, and each
    /// change's snapshot passed to every native instance that reads the
    /// bucket, which reads it there without asking the node. `None`: not
    /// published.
    pub publish: Option<Duration>,
}

/// Where a secret's value comes from. Read on every access.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SecretSource {
    /// A file's whole contents.
    File(PathBuf),
    /// An environment variable of the node process.
    Env(String),
}

/// A SQL datasource the node binds (`[datasources.<name>]`):
/// a Postgres database, pooled. Apps name it; they never see its address
/// or credentials.
///
/// Since ROADMAP DA3 its client is the driver `bulkhead-sql`, an app of
/// its own ([`SqlDatasource::driver_app`]) that the node starts: the
/// runtime gives that app the profile [`SqlDatasource::driver_profile`]
/// (the connection to this address, the password and CA files) and
/// forwards each app's statements to it, keeping the app's connection cap
/// and its transactions itself. [`SqlDatasource::driver_args`] are the
/// driver's init arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlDatasource {
    /// Host name or address; a path (starting `/`) is a Unix socket
    /// directory.
    pub host: String,
    /// TCP port, or the Unix socket's number.
    pub port: u16,
    /// Database name.
    pub dbname: String,
    /// Database user.
    pub user: String,
    /// Where the password comes from (a node secret's source); `None` for
    /// none (trust or peer authentication).
    pub password: Option<SecretSource>,
    /// TLS to the server.
    pub tls: SqlTls,
    /// Most connections its driver opens to it, for every app together.
    pub pool_size: usize,
    /// Time allowed to open a connection.
    pub connect_timeout: Duration,
    /// Time a call waits for a connection (the app's cap or the pool) before
    /// `limit-reached`.
    pub acquire_timeout: Duration,
    /// The server cancels a statement that runs longer (`timeout`).
    pub statement_timeout: Duration,
    /// Largest result a call returns, in bytes (`too-large` above it).
    pub max_result_bytes: u64,
}

impl SqlDatasource {
    /// A datasource at `host` with the defaults: port 5432, no password,
    /// no TLS, 10 connections, 5 s to connect and to acquire, statements
    /// cancelled after 30 s, results up to 16 MiB.
    #[must_use]
    pub fn new(
        host: impl Into<String>,
        dbname: impl Into<String>,
        user: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port: 5432,
            dbname: dbname.into(),
            user: user.into(),
            password: None,
            tls: SqlTls::Disable,
            pool_size: 10,
            connect_timeout: Duration::from_secs(5),
            acquire_timeout: Duration::from_secs(5),
            statement_timeout: Duration::from_secs(30),
            max_result_bytes: 16 << 20,
        }
    }

    /// The driver app serving datasource `name`.
    #[must_use]
    pub fn driver_app(name: &str) -> String {
        format!("{SQL_DRIVER_PREFIX}{name}")
    }

    /// Where the node connects the driver: the Unix socket
    /// `<host>/.s.PGSQL.<port>` for a directory, else TCP to the host
    /// (its name resolved on each connect) and port.
    #[must_use]
    pub fn endpoint(&self) -> DriverEndpoint {
        if self.host.starts_with('/') {
            DriverEndpoint::Unix(PathBuf::from(&self.host).join(format!(".s.PGSQL.{}", self.port)))
        } else {
            DriverEndpoint::Tcp {
                host: self.host.clone(),
                port: self.port,
                timeout: self.connect_timeout,
            }
        }
    }

    /// The files the driver reads: the password file and the CA file.
    fn driver_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        if let Some(SecretSource::File(path)) = &self.password {
            files.push(path.clone());
        }
        if let SqlTls::Require { ca_file } = &self.tls {
            files.push(ca_file.clone());
        }
        files
    }

    /// The driver's profile: the connection to [`SqlDatasource::endpoint`]
    /// and its files, nothing else.
    #[must_use]
    pub fn driver_profile(&self) -> DriverProfile {
        DriverProfile {
            files: self.driver_files(),
            connect: Some(self.endpoint()),
            ..DriverProfile::default()
        }
    }

    /// How long the node waits for the driver's answer: longer than the
    /// driver itself may take (a free connection, a new one opened and
    /// reset, the statement), so a transaction it began is never left
    /// unknown to the node.
    #[must_use]
    pub fn call_timeout(&self) -> Duration {
        self.acquire_timeout + self.connect_timeout * 2 + self.statement_timeout + CALL_MARGIN
    }

    /// The driver's init arguments for datasource `name`
    /// (`bulkhead_sql::config::Config`): files by their path under
    /// `/driver/`; a password from the node's environment as its value.
    ///
    /// # Errors
    ///
    /// The password's variable is not set or not UTF-8, or the password
    /// and CA files have the same file name.
    pub fn driver_args(&self, name: &str) -> Result<serde_json::Value, String> {
        let under_driver = |path: &std::path::Path| {
            path.file_name()
                .map(|f| format!("/driver/{}", f.to_string_lossy()))
                .ok_or_else(|| format!("datasource {name}: {} names no file", path.display()))
        };
        let files = self.driver_files();
        let names: std::collections::BTreeSet<_> =
            files.iter().filter_map(|f| f.file_name()).collect();
        if names.len() != files.len() {
            return Err(format!(
                "datasource {name}: its password and CA files have the same file name"
            ));
        }
        let password = match &self.password {
            None => serde_json::Value::Null,
            Some(SecretSource::File(path)) => serde_json::json!({ "file": under_driver(path)? }),
            Some(SecretSource::Env(var)) => {
                let value = std::env::var(var)
                    .map_err(|e| format!("datasource {name}: password variable {var}: {e}"))?;
                serde_json::json!({ "value": value })
            }
        };
        let tls = match &self.tls {
            SqlTls::Disable => serde_json::json!("disable"),
            SqlTls::Require { ca_file } => {
                serde_json::json!({ "require": { "ca_file": under_driver(ca_file)? } })
            }
        };
        let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        Ok(serde_json::json!({
            "datasource": name,
            "host": self.host,
            "transport": if self.host.starts_with('/') { "unix" } else { "tcp" },
            "dbname": self.dbname,
            "user": self.user,
            "password": password,
            "tls": tls,
            "pool_size": self.pool_size.max(1),
            "connect_timeout_ms": ms(self.connect_timeout),
            "acquire_timeout_ms": ms(self.acquire_timeout),
            "statement_timeout_ms": ms(self.statement_timeout),
            "max_result_bytes": self.max_result_bytes,
        }))
    }
}

/// The prefix of a datasource's driver app (`bulkhead-sql-<datasource>`),
/// reserved for those apps.
pub const SQL_DRIVER_PREFIX: &str = "bulkhead-sql-";

/// The prefix of an app's egress driver (`bulkhead-egress-<app>`, ROADMAP
/// DA4), reserved for those apps.
pub const EGRESS_DRIVER_PREFIX: &str = "bulkhead-egress-";

/// The egress driver app serving `app`'s HTTP requests.
#[must_use]
pub fn egress_driver_app(app: &str) -> String {
    format!("{EGRESS_DRIVER_PREFIX}{app}")
}

/// What [`SqlDatasource::call_timeout`] adds to the driver's own bound.
const CALL_MARGIN: Duration = Duration::from_secs(5);

/// TLS to a SQL datasource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlTls {
    /// Plain connections: a Unix socket or a trusted network.
    Disable,
    /// TLS, verifying the server against the CA certificates in this PEM
    /// file (no system roots: the operator names whom to trust).
    Require {
        /// PEM file of CA certificates.
        ca_file: PathBuf,
    },
}

/// Configuration for a [`Runtime`](crate::Runtime).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    /// This node's id, carried in every pid.
    pub node: u32,
    /// Most instances in the process table when `max_processes` is unset.
    pub max_instances: u32,
    /// Most instances in the process table; `None` is `max_instances`. A start past it fails with
    /// [`HostError::InstanceLimit`](crate::HostError::InstanceLimit).
    pub max_processes: Option<u32>,
    /// Where node state lives (`disk` keyvalue buckets). `None` disables
    /// disk buckets.
    pub data_dir: Option<PathBuf>,
    /// How often this node tells each peer it is alive
    /// ([`crate::transport::liveness`]). Zero sends no heartbeats.
    pub heartbeat_interval: Duration,
    /// How long a peer may stay silent on a path before that path counts as
    /// silent; a silent cluster link ends the session with the peer.
    pub node_timeout: Duration,
    /// This start of the node, raised on every start and kept on disk by the
    /// node ([`crate::transport::membership`]). From 1 to
    /// [`crate::pid::MAX_CREATION`]; it is the high bits of every instance
    /// id.
    pub creation: u32,
    /// What this node does in the cluster.
    pub role: Role,
    /// Where other nodes reach this one; its meaning is the transport's.
    pub address: String,
    /// Host-object limits for instances whose manifest sets none, and the
    /// most any manifest may ask for.
    pub host_objects: HostObjects,
    /// Shared keyvalue buckets, by name.
    pub buckets: BTreeMap<String, SharedBucket>,
    /// Secrets, by name.
    pub secrets: BTreeMap<String, SecretSource>,
    /// SQL datasources, by name.
    pub datasources: BTreeMap<String, SqlDatasource>,
    /// HTTP egress for every app.
    pub egress: Egress,
    /// Most log records per second per app, `bulkhead:logging` records and
    /// captured stdout and stderr lines together; an app's own
    /// `rate_limit_per_sec` can only lower it. 0 for no ceiling.
    pub log_records_per_sec: u32,
    /// Most calls from one other node handled at once; a call beyond it is
    /// answered `overloaded` at once. An honest peer is bounded by its
    /// instances' `pending_calls_max`; this bounds a faulty one. 0 for no
    /// limit.
    pub incoming_calls_per_peer: u32,
    /// Waited casts (`cast_timeout`) a sender on another node may have
    /// sent ahead of answers, per target: room this node reserves in the
    /// target's mailbox for each such sender (ROADMAP I12). 0 reserves none,
    /// and every waited cast from another node waits for its answer.
    pub cast_credit: u32,
    /// Bytes of bodies reserved with [`RuntimeConfig::cast_credit`].
    pub cast_credit_bytes: u64,
    /// Native-process apps.
    pub native: NativeConfig,
    /// Driver apps (node config `[drivers.<app>]`, ROADMAP DA1): the
    /// profile each app of that name gets beyond its manifest.
    pub drivers: BTreeMap<String, DriverProfile>,
    /// Where host names are resolved (ROADMAP DA9): a node names its
    /// resolver driver; the default resolves none.
    pub resolver: crate::names::Resolver,
}

/// What node config gives a driver app beyond its manifest (ROADMAP DA1).
/// A manifest cannot ask for any of it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DriverProfile {
    /// Listening sockets the node bound for it and keeps, so its restart
    /// or upgrade keeps the port; each of its processes gets them
    /// inherited (`bulkhead_native::driver`), with `accept4` allowed on
    /// them alone. At most `bulkhead_native::driver::MAX_LISTENERS`.
    pub listeners: Vec<DriverListener>,
    /// Files it may read (certificates and keys), each under `/driver/`
    /// by its file name.
    pub files: Vec<PathBuf>,
    /// The apps it may send to: its messaging ACL's `may_call`, in place
    /// of its manifest's.
    pub calls: Vec<String>,
    /// Send it the HTTP route table as the host event `routes`, and
    /// answer its `Request::Elsewhere` (ROADMAP DA2).
    pub routes: bool,
    /// Its `may_call` is the apps that own HTTP routes, kept up to date as
    /// routes change, in place of `calls` (the ingress driver, ROADMAP
    /// DA2).
    pub calls_routed: bool,
    /// It may call an app on the failover partner where the app runs now
    /// (`Request::CallApp`'s `partner`), for a listener that forwards.
    pub forward: bool,
    /// It may open streams into the node's admin API, each request on them
    /// needing an operator's signature (`Request::AdminStream`, the remote
    /// admin driver, ROADMAP DA7; [`Runtime::set_admin_streams`]).
    ///
    /// [`Runtime::set_admin_streams`]: crate::Runtime::set_admin_streams
    pub admin: bool,
    /// It may read the node's metrics (`Request::MetricsText`, the metrics
    /// driver, ROADMAP DA7; [`Runtime::set_metrics_text`]).
    ///
    /// [`Runtime::set_metrics_text`]: crate::Runtime::set_metrics_text
    pub metrics: bool,
    /// The one endpoint the node connects it to on request
    /// (`Request::Connect`, a datasource's driver, ROADMAP DA3).
    pub connect: Option<DriverEndpoint>,
    /// The app whose HTTP requests it sends (an egress driver, ROADMAP
    /// DA4): the node connects it (`Request::ConnectTo`) where that app's
    /// running instances may send requests, and nowhere else. Given by the
    /// runtime to every app named [`EGRESS_DRIVER_PREFIX`]`<app>`.
    pub egress: Option<String>,
}

/// Where the node connects a driver ([`DriverProfile::connect`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverEndpoint {
    /// A TCP address, its host name resolved on every connect.
    Tcp {
        /// A host name or an address.
        host: String,
        /// The port.
        port: u16,
        /// How long a connect may take, the lookup included.
        timeout: Duration,
    },
    /// A Unix stream socket's path (Postgres's `<dir>/.s.PGSQL.<port>`).
    Unix(PathBuf),
}

/// A listening socket of a driver app's, by name.
#[derive(Debug, Clone)]
pub struct DriverListener {
    /// The name the driver finds it by.
    pub name: String,
    /// The socket, bound by the node.
    pub socket: std::sync::Arc<std::net::TcpListener>,
}

impl PartialEq for DriverListener {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && std::sync::Arc::ptr_eq(&self.socket, &other.socket)
    }
}

impl Eq for DriverListener {}

/// How a node runs native-process apps.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NativeConfig {
    /// The `bulkhead-runner` executable; `None` for the one beside the
    /// node's own executable.
    pub runner: Option<PathBuf>,
    /// A cgroup v2 directory delegated to the node (systemd's
    /// `Delegate=yes`). With it, the node moves itself into its `node`
    /// child and runs each native instance in a cgroup of its own under
    /// `native`, with `memory.max` from the app's `memory_max`, no swap,
    /// and `pids.max`. Without it, `memory_max` is the process's
    /// `RLIMIT_DATA`.
    pub cgroup: Option<PathBuf>,
    /// Most processes and threads in one native instance (with a cgroup).
    pub pids_max: u32,
    /// How long an idle native instance goes between pings.
    pub ping_interval: Duration,
    /// How long a ping may go unanswered before the instance is killed
    /// (`timeout`).
    pub ping_timeout: Duration,
    /// Whether to run native apps on a Linux kernel without Landlock (which
    /// the runner then cannot restrict to its files): off by default.
    /// FreeBSD's fence is Capsicum, always present.
    pub landlock_optional: bool,
    /// How long a side of a native instance's channel spins waiting for
    /// the other before it sleeps: the node's instance task and the app's
    /// waiting thread. It must outlast a wake-up to help (about 20 µs on
    /// an x86 machine with deep C-states, 7 µs on a Raspberry Pi 5; spike
    /// S32); zero always sleeps.
    pub spin: Duration,
    /// How long an app's thread waiting for an answer on a sender
    /// channel (ROADMAP AP1) spins before it sleeps; the node's task
    /// serving the channel keeps [`Self::spin`]. Longer than it: one
    /// client through a driver app waited about 15% less a request on a
    /// Raspberry Pi 5, and the node's task spinning as long cost a loaded
    /// driver 10 to 20% of its requests there (ROADMAP DA1b).
    pub sender_spin: Duration,
    /// The privileged helper that starts every instance under its app's
    /// user (node config `[priv]`, ROADMAP PS1); `None` to start them as
    /// the node's own children, under the node's user.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub helper: Option<crate::native::helper::PrivHelper>,
}

impl Default for NativeConfig {
    fn default() -> Self {
        Self {
            runner: None,
            cgroup: None,
            pids_max: 64,
            ping_interval: Duration::from_secs(10),
            ping_timeout: Duration::from_secs(5),
            landlock_optional: false,
            spin: Duration::from_micros(20),
            sender_spin: Duration::from_micros(50),
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            helper: None,
        }
    }
}

/// HTTP egress (`bulkhead:http-egress`): what every
/// app's outgoing requests share. Each app may reach only what its
/// manifest's `allow` lists, within [`Egress::allow`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Egress {
    /// The most any app may be allowed, in the manifest's `allow` syntax;
    /// `None` for no ceiling. A deploy whose manifest asks for more is
    /// refused (`services::egress::within`).
    pub allow: Option<Vec<String>>,
    /// Trust the operating system's root certificates.
    pub system_roots: bool,
    /// More trusted certificate authorities, PEM.
    pub ca_file: Option<PathBuf>,
    /// Allow host names that resolve to loopback or link-local addresses
    /// (`169.254.0.0/16`, `fe80::/10`: cloud metadata). An address written
    /// in an allow entry is always allowed.
    pub allow_loopback: bool,
    /// Time allowed to open a connection.
    pub connect_timeout: Duration,
    /// Most time from sending a request to its response headers; a guest's
    /// `first-byte-timeout` may only shorten it.
    pub first_byte_timeout: Duration,
    /// Most time between two chunks of a response body; a guest's
    /// `between-bytes-timeout` may only shorten it.
    pub between_bytes_timeout: Duration,
    /// How long an idle pooled connection is kept.
    pub pool_idle_timeout: Duration,
    /// Idle connections kept per host.
    pub pool_max_idle_per_host: usize,
    /// Requests in flight at once per app, when its manifest does not set
    /// `max_concurrent`.
    pub max_concurrent: u32,
}

impl Default for Egress {
    fn default() -> Self {
        Self {
            allow: None,
            system_roots: true,
            ca_file: None,
            allow_loopback: false,
            connect_timeout: Duration::from_secs(10),
            first_byte_timeout: Duration::from_secs(30),
            between_bytes_timeout: Duration::from_secs(30),
            pool_idle_timeout: Duration::from_secs(90),
            pool_max_idle_per_host: 32,
            max_concurrent: 64,
        }
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            node: 0,
            max_instances: 1_000,
            max_processes: None,
            data_dir: None,
            heartbeat_interval: Duration::from_secs(1),
            node_timeout: Duration::from_secs(5),
            creation: 1,
            role: Role::Member,
            address: String::new(),
            host_objects: HostObjects::default(),
            buckets: BTreeMap::new(),
            secrets: BTreeMap::new(),
            datasources: BTreeMap::new(),
            egress: Egress::default(),
            log_records_per_sec: 1_000,
            incoming_calls_per_peer: 10_000,
            cast_credit: 64,
            cast_credit_bytes: 1 << 20,
            native: NativeConfig::default(),
            drivers: BTreeMap::new(),
            resolver: crate::names::Resolver::None,
        }
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
