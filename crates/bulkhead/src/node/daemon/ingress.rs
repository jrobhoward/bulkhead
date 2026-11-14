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
    #[cfg(feature = "http")]
    rt: Option<Runtime>,
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
        let socket = std::net::TcpListener::bind(binding.address).map_err(|e| {
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
        }));
    }
    let profile = bulkhead_host::DriverProfile {
        listeners,
        files,
        calls: Vec::new(),
        routes: true,
        calls_routed: true,
        forward,
    };
    let args = serde_json::json!({ "threads": threads.clamp(1, 64), "listeners": args });
    Ok(Some((profile, args)))
}

/// No ingress in this build.
#[cfg(not(feature = "http"))]
#[allow(clippy::unnecessary_wraps)] // The same signature as with `http`.
pub(super) fn bind(
    _config: &NodeConfig,
) -> Result<Option<(bulkhead_host::DriverProfile, serde_json::Value)>, NodeError> {
    Ok(None)
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
            rt: Some(rt.clone()),
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
            path: program,
            kind: bulkhead_bundle::manifest::NativeProgram::Executable,
        });
        let deployment = AppDeployment::new(ingress_manifest(args)?, code);
        super::builtin::start(rt, apps, deployment).await?;
        ingress.serving = true;
        tracing::info!(listeners = config.http.len(), "HTTP ingress driver started");
        Ok(ingress)
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

    /// Ask the driver to reload every HTTPS listener's certificates from
    /// their files: how many listeners serve HTTPS, and each that failed
    /// (it keeps its old certificates).
    pub(super) async fn reload_tls(&self) -> (usize, Vec<String>) {
        #[cfg(feature = "http")]
        if self.serving
            && let Some(apps) = &self.apps
        {
            // A certificate replaced by rename is a new file, not yet the
            // driver's under the helper.
            if let Some(rt) = &self.rt {
                for e in rt.share_driver_files().await {
                    tracing::warn!(error = %e, "a driver's file not shared with it");
                }
            }
            return reload_driver(apps).await;
        }
        (0, Vec::new())
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
