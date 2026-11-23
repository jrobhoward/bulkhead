//! The node's HTTP ingress (`[http.<name>]`) and the routes apps declare.
//! Since ROADMAP DA2 the node parses no HTTP: it binds each address and
//! starts the ingress driver, `bulkhead-ingress`, a native app of its own
//! (`INGRESS_APP`) that accepts, reads TLS and HTTP, and calls the app
//! owning each request's route. The node keeps the route table, sends it
//! to the driver (host event `routes`), lets the driver call exactly the
//! apps that own routes, and answers its question where an app that does
//! not run here runs (`Request::Elsewhere`, a `CallApp` with `partner`).
//! Without the `http` feature a node has none, and refuses node config or
//! apps that need one.
//!
//! The driver reads its certificates through the rules or descriptors it
//! was given at start, bound to the files then (Landlock, FreeBSD's
//! descriptors), so a certificate replaced by rename, as certbot renews,
//! is not one it can read. The node keeps each certificate and key file's
//! identity (device and inode, links followed) from the driver's start
//! and checks it on a reload and every `tls_reload_s`: when one changed,
//! it restarts the driver, which then gets the new files (ROADMAP DA2b).
//! The node keeps the listeners, so connections made meanwhile wait;
//! those in flight are dropped. A file rewritten in place is reloaded by
//! the driver itself.

#[cfg(feature = "http")]
use std::path::PathBuf;
#[cfg(feature = "http")]
use std::sync::Arc;
#[cfg(feature = "http")]
use std::time::Duration;

#[cfg(feature = "http")]
use bulkhead_bundle::manifest::AppManifest;
#[cfg(feature = "http")]
use bulkhead_host::routes::{HttpRoute, RouteTable};
use bulkhead_host::{AppDeployment, Apps, InstanceSpec, Runtime};

use crate::node::config::NodeConfig;
#[cfg(feature = "http")]
use crate::node::config::{INGRESS_APP, StandbyMode};
use crate::node::error::NodeError;

/// The schema of the node's call asking the driver to reload certificates
/// (`bulkhead_ingress::driver::RELOAD_SCHEMA`).
#[cfg(feature = "http")]
const RELOAD_SCHEMA: &str = "bulkhead/ingress-reload";

/// How long the node waits for the driver's answer to a reload.
#[cfg(feature = "http")]
const RELOAD_TIMEOUT: Duration = Duration::from_secs(10);

/// The node's ingress: the route table, and whether a driver serves it.
#[derive(Debug, Clone, Default)]
pub(super) struct Ingress {
    #[cfg(feature = "http")]
    routes: Option<Arc<RouteTable>>,
    /// The driver runs (node config has `[http]` addresses).
    #[cfg(feature = "http")]
    serving: bool,
    #[cfg(feature = "http")]
    apps: Option<Apps>,
    /// How to start the driver again, and its files' identities.
    #[cfg(feature = "http")]
    driver: Option<Arc<Driver>>,
}

/// The driver as started: what starts it again, and the identity of each
/// file it reads.
#[cfg(feature = "http")]
#[derive(Debug)]
struct Driver {
    rt: Runtime,
    program: PathBuf,
    args: serde_json::Value,
    /// Every certificate and key file, with its identity at the start.
    files: std::sync::Mutex<Vec<(PathBuf, Identity)>>,
    /// Checked this often for a renamed file (`None`: on reloads only).
    every: Option<Duration>,
    /// One restart at a time.
    restarting: tokio::sync::Mutex<()>,
}

/// A file's device and inode, links followed.
#[cfg(feature = "http")]
type Identity = (u64, u64);

/// `path`'s identity.
#[cfg(feature = "http")]
fn identity(path: &std::path::Path) -> Result<Identity, String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((meta.dev(), meta.ino()))
}

/// What a reload did: HTTPS listeners reloaded, those that failed, and the
/// files whose replacement restarted the driver.
#[derive(Debug, Default)]
pub(super) struct Reload {
    pub(super) certificates: usize,
    pub(super) failed: Vec<String>,
    pub(super) restarted_for: Vec<std::path::PathBuf>,
}

/// Whether two versions of an app have the same HTTP routes, as both run
/// at once in a blue-green upgrade.
pub(super) fn same_routes(
    a: &bulkhead_bundle::manifest::AppManifest,
    b: &bulkhead_bundle::manifest::AppManifest,
) -> bool {
    #[cfg(feature = "http")]
    {
        HttpRoute::from_manifest(a) == HttpRoute::from_manifest(b)
    }
    #[cfg(not(feature = "http"))]
    {
        let _ = (a, b);
        true
    }
}

/// An app's routes, kept to put back after a failed upgrade.
pub(super) struct SavedRoutes(#[cfg(feature = "http")] Option<Vec<HttpRoute>>);

/// `[http]` bound for the driver: its profile (listeners, certificate
/// files, calls) and its init arguments; `None` without `[http]`.
///
/// # Errors
///
/// An address cannot be bound.
#[cfg(feature = "http")]
pub(super) fn bind(
    config: &NodeConfig,
    native: &bulkhead_host::NativeConfig,
) -> Result<Option<(bulkhead_host::DriverProfile, serde_json::Value)>, NodeError> {
    if config.http.is_empty() {
        return Ok(None);
    }
    let mut listeners = Vec::new();
    let mut files = Vec::new();
    let mut args = Vec::new();
    let mut forward = false;
    let mut threads = 1;
    for (name, binding) in &config.http {
        let socket = bulkhead_host::bind_tcp(binding.address, native).map_err(|e| {
            NodeError::Invalid(format!("http.{name}: bind {}: {e}", binding.address))
        })?;
        listeners.push(bulkhead_host::DriverListener {
            name: name.clone(),
            socket: Arc::new(socket),
        });
        let mut certificates = Vec::new();
        for t in &binding.tls {
            let guest = |p: &PathBuf| {
                format!(
                    "/driver/{}",
                    p.file_name().unwrap_or_default().to_string_lossy()
                )
            };
            certificates.push(serde_json::json!({
                "cert": guest(&t.cert),
                "key": guest(&t.key),
                "hosts": t.hosts,
            }));
            files.push(t.cert.clone());
            files.push(t.key.clone());
        }
        let client_auth = binding.client_ca.as_ref().map(|ca| {
            files.push(ca.clone());
            serde_json::json!({
                "ca": format!("/driver/{}", ca.file_name().unwrap_or_default().to_string_lossy()),
                "required": binding.client_auth
                    != Some(crate::node::config::ClientAuthMode::Optional),
            })
        });
        let standby = match (binding.standby_mode(), &binding.partner_url) {
            (StandbyMode::Redirect, Some(url)) => serde_json::json!({
                "redirect": { "partner_url": url.trim_end_matches('/') }
            }),
            (StandbyMode::Forward, _) => {
                forward = true;
                serde_json::json!("forward")
            }
            // Config checks refuse a redirect without a URL.
            _ => serde_json::json!("refuse"),
        };
        threads = threads.max(binding.acceptors);
        args.push(serde_json::json!({
            "name": name,
            "max_connections": binding.max_connections,
            "header_timeout_ms": binding.header_timeout_ms.max(1),
            "body_timeout_ms": binding.body_timeout_ms.max(1),
            "max_response": binding.max_response.bytes(),
            "standby": standby,
            "tls": certificates,
            "tls_reload_s": binding.tls_reload_s,
            "client_auth": client_auth,
            "http2": binding.http2,
            "max_streams": binding.max_streams.max(1),
        }));
    }
    let profile = bulkhead_host::DriverProfile {
        listeners,
        files,
        calls: Vec::new(),
        routes: true,
        calls_routed: true,
        forward,
        admin: false,
        metrics: false,
        connect: None,
        egress: None,
        endpoints: Vec::new(),
        delivers: false,
        seccomp_profile: Some("ingress".into()),
    };
    let args = serde_json::json!({
        "threads": threads.clamp(1, 64),
        "listeners": args,
        "trace_sample_ppm": config.tracing.sample_ppm,
        "accept_traceparent": config.tracing.accept_traceparent,
    });
    Ok(Some((profile, args)))
}

/// No ingress in this build.
#[cfg(not(feature = "http"))]
#[allow(clippy::unnecessary_wraps)] // The same signature as with `http`.
pub(super) fn bind(
    _config: &NodeConfig,
    _native: &bulkhead_host::NativeConfig,
) -> Result<Option<(bulkhead_host::DriverProfile, serde_json::Value)>, NodeError> {
    Ok(None)
}

/// Every certificate, key and client CA file of `[http]`, in listener
/// order.
#[cfg(feature = "http")]
fn tls_files(config: &NodeConfig) -> Vec<PathBuf> {
    config
        .http
        .values()
        .flat_map(|b| {
            b.tls
                .iter()
                .flat_map(|t| [t.cert.clone(), t.key.clone()])
                .chain(b.client_ca.clone())
        })
        .collect()
}

/// The driver's manifest: one instance, its init arguments, restarts that
/// never give up, its metrics named as the node's ingress's were
/// (`bulkhead_http_*`).
#[cfg(feature = "http")]
fn ingress_manifest(args: serde_json::Value) -> Result<AppManifest, NodeError> {
    let spec = serde_json::json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "instances": { "count": 1 },
        "lifecycle": { "start_args": args },
        "supervision": super::builtin::supervision(),
        "limits": {
            "memory_max": "512MiB",
            "message_max_bytes": "16MiB",
        },
        "capabilities": { "bulkhead:metrics": { "prefix": "bulkhead_http" } },
    });
    super::builtin::manifest(INGRESS_APP, "The node's HTTP ingress", &spec)
}

impl Ingress {
    /// The route table, and with `[http]` the driver started with `args`
    /// (from [`bind`]).
    ///
    /// # Errors
    ///
    /// The driver's program is missing, or it did not start.
    #[cfg(feature = "http")]
    pub(super) async fn open(
        config: &NodeConfig,
        rt: &Runtime,
        apps: &Apps,
        args: Option<serde_json::Value>,
    ) -> Result<Self, NodeError> {
        let mut ingress = Self {
            routes: Some(Arc::new(RouteTable::new(rt.clone()))),
            serving: false,
            apps: Some(apps.clone()),
            driver: None,
        };
        let Some(args) = args else {
            return Ok(ingress);
        };
        let program = super::builtin::program(config, config.native.ingress.as_ref(), INGRESS_APP);
        if !program.is_file() {
            return Err(NodeError::Invalid(format!(
                "[http] needs the HTTP ingress program, and {} is not one ([native] ingress)",
                program.display()
            )));
        }
        let code = rt.load_native(bulkhead_host::NativeCode {
            path: program.clone(),
            kind: bulkhead_bundle::manifest::NativeProgram::Executable,
        });
        let deployment = AppDeployment::new(ingress_manifest(args.clone())?, code);
        super::builtin::start(rt, apps, deployment).await?;
        ingress.serving = true;
        let files = tls_files(config)
            .into_iter()
            .map(|p| {
                let id = identity(&p).map_err(NodeError::Invalid)?;
                Ok((p, id))
            })
            .collect::<Result<Vec<_>, NodeError>>()?;
        let every = config
            .http
            .values()
            .filter(|b| !b.tls.is_empty() && b.tls_reload_s > 0)
            .map(|b| Duration::from_secs(b.tls_reload_s))
            .min();
        ingress.driver = Some(Arc::new(Driver {
            rt: rt.clone(),
            program,
            args,
            files: std::sync::Mutex::new(files),
            every,
            restarting: tokio::sync::Mutex::new(()),
        }));
        tracing::info!(listeners = config.http.len(), "HTTP ingress driver started");
        Ok(ingress)
    }

    /// `app`'s routes the ingress serves: (host, prefix).
    pub(super) fn routes_of(&self, app: &str) -> Vec<(Option<String>, String)> {
        #[cfg(feature = "http")]
        if let Some(table) = &self.routes {
            return table
                .routes(app)
                .into_iter()
                .map(|r| (r.host, r.path_prefix))
                .collect();
        }
        let _ = app;
        Vec::new()
    }

    /// How often the node checks for a renamed certificate, if at all.
    #[cfg(feature = "http")]
    pub(super) fn check_every(&self) -> Option<Duration> {
        self.driver.as_ref().and_then(|d| d.every)
    }

    /// Restart the driver if a certificate or key file was replaced (its
    /// identity changed since the driver started): the files that were.
    /// Nothing restarts while a file cannot be read.
    ///
    /// # Errors
    ///
    /// A file cannot be read, or the driver did not start again (the
    /// next check tries again).
    #[cfg(feature = "http")]
    pub(super) async fn restart_if_renewed(&self) -> Result<Vec<PathBuf>, String> {
        let (Some(driver), Some(apps)) = (&self.driver, &self.apps) else {
            return Ok(Vec::new());
        };
        let _one = driver.restarting.lock().await;
        let known = driver
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut now = Vec::with_capacity(known.len());
        let mut changed = Vec::new();
        for (path, was) in known {
            let id = identity(&path)?;
            if id != was {
                changed.push(path.clone());
            }
            now.push((path, id));
        }
        if changed.is_empty() {
            return Ok(changed);
        }
        let code = driver.rt.load_native(bulkhead_host::NativeCode {
            path: driver.program.clone(),
            kind: bulkhead_bundle::manifest::NativeProgram::Executable,
        });
        let deployment = AppDeployment::new(
            ingress_manifest(driver.args.clone()).map_err(|e| e.to_string())?,
            code,
        );
        // Requests in flight end with it; the listeners stay the node's.
        apps.take_out(INGRESS_APP).await;
        super::builtin::start(&driver.rt, apps, deployment)
            .await
            .map_err(|e| format!("the HTTP ingress did not start again: {e}"))?;
        *driver
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = now;
        Ok(changed)
    }

    /// No ingress in this build: refuse node config that asks for one.
    #[cfg(not(feature = "http"))]
    #[allow(clippy::unused_async)] // The same signature as with `http`.
    pub(super) async fn open(
        config: &NodeConfig,
        _rt: &Runtime,
        _apps: &Apps,
        _args: Option<serde_json::Value>,
    ) -> Result<Self, NodeError> {
        match config.http.keys().next() {
            Some(name) => Err(NodeError::Invalid(format!(
                "http.{name}: this build of bulkhead has no HTTP ingress (cargo feature `http`)"
            ))),
            None => Ok(Self::default()),
        }
    }

    /// Give `name` the HTTP routes `deployment` declares, if the operator
    /// granted them (none otherwise).
    #[cfg(feature = "http")]
    pub(super) fn set_routes(
        &self,
        name: &str,
        deployment: &AppDeployment,
    ) -> Result<(), NodeError> {
        self.set_routes_of(name, &deployment.manifest, &deployment.spec)
    }

    #[cfg(feature = "http")]
    fn set_routes_of(
        &self,
        name: &str,
        manifest: &AppManifest,
        spec: &InstanceSpec,
    ) -> Result<(), NodeError> {
        let routes = if granted_routes(spec) {
            HttpRoute::from_manifest(manifest)
        } else {
            Vec::new()
        };
        match &self.routes {
            Some(table) if self.serving || routes.is_empty() => table
                .set(name, routes)
                .map_err(|e| NodeError::Conflict(e.to_string())),
            None if routes.is_empty() => Ok(()),
            _ => Err(NodeError::Invalid(format!(
                "{name} declares HTTP routes, and this node has no HTTP ingress ([http.<name>] in node config)"
            ))),
        }
    }

    /// No ingress in this build: refuse an app granted routes.
    #[cfg(not(feature = "http"))]
    pub(super) fn set_routes(
        &self,
        name: &str,
        deployment: &AppDeployment,
    ) -> Result<(), NodeError> {
        let declares = deployment
            .manifest
            .spec
            .capabilities
            .http_incoming
            .as_ref()
            .is_some_and(|scope| !scope.routes.is_empty());
        if granted_routes(&deployment.spec) && declares {
            return Err(NodeError::Invalid(format!(
                "{name} declares HTTP routes, and this build of bulkhead has no HTTP ingress (cargo feature `http`)"
            )));
        }
        Ok(())
    }

    pub(super) fn remove_routes(&self, name: &str) {
        #[cfg(feature = "http")]
        if let Some(table) = &self.routes {
            table.remove(name);
        }
        #[cfg(not(feature = "http"))]
        let _ = name;
    }

    /// `name`'s routes now, to put back with [`Ingress::restore_routes`].
    pub(super) fn save_routes(&self, name: &str) -> SavedRoutes {
        #[cfg(feature = "http")]
        return SavedRoutes(self.routes.as_ref().map(|t| t.routes(name)));
        #[cfg(not(feature = "http"))]
        {
            let _ = name;
            SavedRoutes()
        }
    }

    /// Put back routes saved before a failed upgrade: the old version keeps
    /// running with them. A failure is logged and returned, to add to the
    /// upgrade's error.
    pub(super) fn restore_routes(&self, name: &str, saved: SavedRoutes) -> Result<(), String> {
        #[cfg(feature = "http")]
        if let (Some(table), SavedRoutes(Some(before))) = (&self.routes, saved)
            && let Err(e) = table.set(name, before)
        {
            tracing::error!(app = name, error = %e, "HTTP routes not put back after a failed upgrade");
            return Err(format!("its HTTP routes could not be put back: {e}"));
        }
        #[cfg(not(feature = "http"))]
        let _ = (saved, name);
        Ok(())
    }

    /// Reload every HTTPS listener's certificates: restart the driver if
    /// a file was replaced by rename, otherwise ask it to read them again
    /// (a file rewritten in place). A listener that fails keeps its old
    /// certificates.
    pub(super) async fn reload_tls(&self) -> Reload {
        #[cfg(feature = "http")]
        if self.serving
            && let Some(apps) = &self.apps
        {
            let restarted_for = match self.restart_if_renewed().await {
                Ok(files) => files,
                Err(e) => {
                    let (certificates, mut failed) = reload_driver(apps).await;
                    failed.push(format!("http: {e}"));
                    return Reload {
                        certificates,
                        failed,
                        restarted_for: Vec::new(),
                    };
                }
            };
            let (certificates, failed) = reload_driver(apps).await;
            return Reload {
                certificates,
                failed,
                restarted_for,
            };
        }
        Reload::default()
    }

    /// Stop the driver first, so no request reaches an app that is
    /// stopping.
    pub(super) async fn stop(&self) {
        #[cfg(feature = "http")]
        if self.serving
            && let Some(apps) = &self.apps
        {
            apps.take_out(INGRESS_APP).await;
        }
    }
}

/// The driver's answer to a reload (`bulkhead_ingress::driver::Reloaded`).
#[cfg(feature = "http")]
#[derive(Debug, serde::Deserialize)]
struct Reloaded {
    certificates: usize,
    failed: Vec<String>,
}

#[cfg(feature = "http")]
async fn reload_driver(apps: &Apps) -> (usize, Vec<String>) {
    let Some(driver) = apps.app(INGRESS_APP) else {
        return (0, vec!["http: the HTTP ingress does not run".into()]);
    };
    let msg = bulkhead_host::Envelope {
        content_type: "application/json".into(),
        schema: Some(RELOAD_SCHEMA.into()),
        body: b"{}".to_vec(),
        trace: None,
    };
    match driver.call(msg, RELOAD_TIMEOUT).await {
        Ok(reply) => match serde_json::from_slice::<Reloaded>(&reply.body) {
            Ok(r) => (
                r.certificates,
                r.failed.into_iter().map(|f| format!("http.{f}")).collect(),
            ),
            Err(e) => (0, vec![format!("http: the ingress's answer: {e}")]),
        },
        Err(e) => (0, vec![format!("http: the ingress did not answer: {e}")]),
    }
}

/// Whether the operator granted an app its HTTP routes.
fn granted_routes(spec: &InstanceSpec) -> bool {
    spec.granted
        .contains(&bulkhead_bundle::manifest::Capability::HttpIncoming)
}
