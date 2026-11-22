//! `bulkhead:http-egress`: HTTP requests from apps.
//!
//! An app sends a request as a frame, and this module's filter (`Hooks`)
//! checks it. A request goes out only if the operator granted the
//! capability, its scheme, host and port match an entry of the manifest's
//! `allow` (the `policy` module), its method is in `methods` (when
//! listed), and it sets no forbidden header, at most `max_concurrent` per
//! app at once (node config's default otherwise). Refusals are answers
//! (`HttpError::Denied`, `Limited`, `Invalid`), never a crash of the
//! instance.
//!
//! Since ROADMAP DA4 the node sends nothing itself: a request it allows is
//! a call to the app's egress driver (`bulkhead-egress-<app>`, schema
//! [`EGRESS_SCHEMA`]), which runs HTTP and TLS in a process of its own and
//! answers with the whole response. The app's permit is held until it
//! answers. The driver's connections are opened by the node
//! (`Request::ConnectTo`) only where the app's running instances may send
//! requests (`Shared::reach`); a name that resolves only to loopback or
//! link-local addresses is refused unless node config sets
//! `allow_loopback`, and an address written in an allow entry is reached
//! as written.
//!
//! Node config's `[egress] allow` is a ceiling every app's `allow` must fit
//! inside; the node checks it at deploy ([`within`]).
//!
//! The `allow` entries of each app's running instances are kept here by
//! pid: an upgrade, or a blue-green pair with two lists, needs no driver
//! restart.
//!
//! Without the `egress` cargo feature the interface is not linked, so a
//! native app's request is answered `Other`.

mod policy;

#[cfg(feature = "egress")]
pub(crate) use policy::guarded;
pub use policy::{parse_deny, parse_proxy, within};

use std::sync::Arc;

use bulkhead_bundle::manifest::{AppSpec, Capability};

use super::{InstanceInfo, Service};
use crate::config::RuntimeConfig;
use crate::error::HostError;
use crate::pid::Pid;

/// The schema of the node's calls to an egress driver and of its answers
/// (`bulkhead_egress::EGRESS_SCHEMA`).
pub const EGRESS_SCHEMA: &str = "bulkhead/egress";

/// The egress service.
pub(crate) struct Egress;

/// What the manifest asks of egress for one app.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct EgressSpec {
    /// Allow entries (`https://api.example.com`, `*.internal`).
    pub allow: Vec<String>,
    /// Methods allowed, upper case; empty for any.
    pub methods: Vec<String>,
    /// Requests in flight at once; `None` for node config's default.
    pub max_concurrent: Option<u32>,
}

impl EgressSpec {
    /// Whether an app with this spec, granted `bulkhead:http-egress`, can
    /// send any request, and so needs an egress driver.
    #[must_use]
    pub fn sends(&self) -> bool {
        !self.allow.is_empty()
    }
}

/// Node-wide egress state.
pub(crate) struct Shared {
    #[cfg(feature = "egress")]
    config: crate::config::Egress,
    /// The trusted roots for every driver, PEM: the system's (when
    /// `system_roots`) and the CA file's, read at the start and again by
    /// [`Shared::reload_roots`] (ROADMAP DA4b).
    #[cfg(feature = "egress")]
    roots_pem: std::sync::RwLock<String>,
    /// [`crate::config::Egress::deny`], parsed.
    #[cfg(feature = "egress")]
    deny: policy::DenyList,
    /// [`crate::config::Egress::proxy`] as host and port.
    #[cfg(feature = "egress")]
    proxy: Option<(String, u16)>,
    /// Each app's limit on requests in flight, and its semaphore.
    #[cfg(feature = "egress")]
    limits:
        std::sync::Mutex<std::collections::HashMap<String, (usize, Arc<tokio::sync::Semaphore>)>>,
    /// The `allow` entries of each app's running, granted instances, by
    /// pid: where the app's egress driver may connect.
    #[cfg(feature = "egress")]
    allows: std::sync::Mutex<
        std::collections::HashMap<String, std::collections::BTreeMap<Pid, Vec<policy::Allow>>>,
    >,
    /// The runtime, through which requests reach the drivers.
    #[cfg(feature = "egress")]
    rt: std::sync::OnceLock<std::sync::Weak<crate::runtime::RuntimeInner>>,
}

/// How an egress driver may reach a destination ([`Shared::reach`]).
#[cfg(feature = "egress")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reach {
    /// An address an allow entry names: reached as written.
    Address(std::net::IpAddr),
    /// A host name, resolved on each connect; an address
    /// [`Shared::refused`] says no to is not connected.
    Name,
}

impl Shared {
    /// Egress as node config describes it.
    ///
    /// # Errors
    ///
    /// [`HostError::Egress`]: the CA file cannot be read or holds no
    /// certificate.
    pub(crate) fn new(config: &RuntimeConfig) -> Result<Self, HostError> {
        #[cfg(not(feature = "egress"))]
        let _ = config;
        Ok(Self {
            #[cfg(feature = "egress")]
            config: config.egress.clone(),
            #[cfg(feature = "egress")]
            roots_pem: std::sync::RwLock::new(
                roots::pem(&config.egress).map_err(HostError::Egress)?,
            ),
            #[cfg(feature = "egress")]
            deny: policy::parse_deny(&config.egress.deny).map_err(HostError::Egress)?,
            #[cfg(feature = "egress")]
            proxy: config
                .egress
                .proxy
                .as_deref()
                .map(policy::parse_proxy)
                .transpose()
                .map_err(HostError::Egress)?,
            #[cfg(feature = "egress")]
            limits: std::sync::Mutex::default(),
            #[cfg(feature = "egress")]
            allows: std::sync::Mutex::default(),
            #[cfg(feature = "egress")]
            rt: std::sync::OnceLock::new(),
        })
    }

    /// Calls to the drivers go through `rt`.
    pub(crate) fn attach(&self, rt: &Arc<crate::runtime::RuntimeInner>) {
        #[cfg(feature = "egress")]
        let _ = self.rt.set(Arc::downgrade(rt));
        #[cfg(not(feature = "egress"))]
        let _ = rt;
    }

    /// The init arguments of `app`'s egress driver
    /// (`bulkhead_egress::config::Config`); `None` without the `egress`
    /// feature.
    pub(crate) fn driver_args(&self, app: &str) -> Option<serde_json::Value> {
        #[cfg(feature = "egress")]
        {
            let ms = |d: std::time::Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
            let roots_pem = self
                .roots_pem
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let mut args = serde_json::json!({
                "app": app,
                "connect_timeout_ms": ms(self.config.connect_timeout),
                "pool_idle_timeout_ms": ms(self.config.pool_idle_timeout),
                "pool_max_idle_per_host": self.config.pool_max_idle_per_host,
                "max_response_body": self.config.max_response_body,
                "roots_pem": roots_pem,
                "traceparent": self.config.traceparent,
            });
            // The files' paths in the driver's root, never their contents.
            if let Some(identity) = self.config.identities.get(app) {
                let at = |p: &std::path::Path| {
                    format!(
                        "/driver/{}",
                        p.file_name().unwrap_or_default().to_string_lossy()
                    )
                };
                args["client_cert"] = serde_json::Value::String(at(&identity.cert));
                args["client_key"] = serde_json::Value::String(at(&identity.key));
            }
            Some(args)
        }
        #[cfg(not(feature = "egress"))]
        {
            let _ = app;
            None
        }
    }
}

#[cfg(feature = "egress")]
mod roots {
    //! The roots every egress driver trusts, as PEM text for its init
    //! arguments: read by the node from the operating system's files and
    //! the operator's CA file, which it trusts; the driver parses them.

    use base64::Engine;
    use rustls_pki_types::CertificateDer;
    use rustls_pki_types::pem::PemObject;

    /// One certificate as PEM.
    fn pem_of(der: &[u8]) -> String {
        let b64 = base64::engine::general_purpose::STANDARD.encode(der);
        let mut out = String::from("-----BEGIN CERTIFICATE-----\n");
        for line in b64.as_bytes().chunks(64) {
            out.push_str(&String::from_utf8_lossy(line));
            out.push('\n');
        }
        out.push_str("-----END CERTIFICATE-----\n");
        out
    }

    /// The system's roots (when `system_roots`) and the CA file's.
    pub(super) fn pem(config: &crate::config::Egress) -> Result<String, String> {
        let mut out = String::new();
        if config.system_roots {
            let found = rustls_native_certs::load_native_certs();
            for e in &found.errors {
                tracing::warn!(error = %e, "egress: a system root certificate could not be read");
            }
            for cert in &found.certs {
                out.push_str(&pem_of(cert));
            }
        }
        if let Some(path) = &config.ca_file {
            let fail =
                |e: &dyn std::fmt::Display| format!("egress CA file {}: {e}", path.display());
            let mut any = false;
            for cert in CertificateDer::pem_file_iter(path).map_err(|e| fail(&e))? {
                out.push_str(&pem_of(&cert.map_err(|e| fail(&e))?));
                any = true;
            }
            if !any {
                return Err(fail(&"no certificates"));
            }
        }
        Ok(out)
    }
}

#[cfg(feature = "egress")]
impl Shared {
    /// Record instance `pid` of `app` with its `allow` entries, or forget
    /// it (`None`: not granted).
    fn set_allow(&self, app: &str, pid: Pid, allow: Option<Vec<policy::Allow>>) {
        let mut allows = self
            .allows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match allow {
            Some(allow) => {
                allows
                    .entry(app.to_string())
                    .or_default()
                    .insert(pid, allow);
            }
            None => {
                if let Some(by_pid) = allows.get_mut(app) {
                    by_pid.remove(&pid);
                    if by_pid.is_empty() {
                        allows.remove(app);
                    }
                }
            }
        }
    }

    /// Forget instance `pid`, whichever app it ran.
    fn forget(&self, pid: Pid) {
        let mut allows = self
            .allows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        allows.retain(|_, by_pid| {
            by_pid.remove(&pid);
            !by_pid.is_empty()
        });
    }

    /// Whether `app`'s egress driver may connect to `host` (a name, or an
    /// address without brackets) and `port`, and how: some running instance
    /// of `app` has an allow entry for them, either scheme. `None` refuses.
    pub(crate) fn reach(&self, app: &str, host: &str, port: u16) -> Option<Reach> {
        let allows = self
            .allows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let allowed = allows.get(app)?.values().flatten().any(|a| {
            a.allows(policy::Scheme::Https, host, port)
                || a.allows(policy::Scheme::Http, host, port)
        });
        if !allowed {
            return None;
        }
        Some(match host.parse::<std::net::IpAddr>() {
            Ok(ip) => Reach::Address(ip),
            Err(_) => Reach::Name,
        })
    }

    /// Whether a host name resolving to `ip` is not connected: loopback and
    /// link-local unless `allow_loopback`, and the deny list's ranges
    /// (ROADMAP DA4b).
    pub(crate) fn refused(&self, ip: std::net::IpAddr) -> bool {
        (!self.config.allow_loopback && guarded(ip)) || policy::denied(ip, &self.deny)
    }

    /// The proxy every driver connection goes through, if any.
    pub(crate) fn proxy(&self) -> Option<&(String, u16)> {
        self.proxy.as_ref()
    }

    /// The files `app`'s egress driver reads, bound at `/driver/`: its
    /// identity's certificate and key, if node config gives it one.
    pub(crate) fn driver_files(&self, app: &str) -> Vec<std::path::PathBuf> {
        self.config
            .identities
            .get(app)
            .map(|i| vec![i.cert.clone(), i.key.clone()])
            .unwrap_or_default()
    }

    /// Read the trusted roots again (ROADMAP DA4b); whether they changed.
    /// A failure keeps the roots read before.
    ///
    /// # Errors
    ///
    /// The CA file cannot be read or holds no certificate.
    pub(crate) fn reload_roots(&self) -> Result<bool, String> {
        let pem = roots::pem(&self.config)?;
        let mut kept = self
            .roots_pem
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *kept == pem {
            return Ok(false);
        }
        *kept = pem;
        Ok(true)
    }

    /// Node config's egress settings.
    pub(crate) fn config(&self) -> &crate::config::Egress {
        &self.config
    }
}

/// Without the `egress` feature no driver runs: nothing to bind or reload.
#[cfg(not(feature = "egress"))]
impl Shared {
    pub(crate) fn driver_files(&self, _app: &str) -> Vec<std::path::PathBuf> {
        Vec::new()
    }

    pub(crate) fn reload_roots(&self) -> Result<bool, String> {
        Ok(false)
    }
}

/// Egress state for one instance.
pub(crate) struct Instance {
    #[cfg(feature = "egress")]
    hooks: hooks::Hooks,
}

impl Service for Egress {
    const CAPABILITY: Capability = Capability::HttpOutgoing;
    type Spec = EgressSpec;
    type Shared = Shared;
    type Instance = Instance;

    fn spec(manifest: &AppSpec) -> EgressSpec {
        manifest
            .capabilities
            .http_outgoing
            .as_ref()
            .map(|s| EgressSpec {
                allow: s.allow.clone(),
                methods: s.methods.clone(),
                max_concurrent: s.max_concurrent,
            })
            .unwrap_or_default()
    }

    fn shared(config: &RuntimeConfig) -> Result<Shared, HostError> {
        Shared::new(config)
    }

    #[cfg_attr(not(feature = "egress"), allow(unused_variables))]
    fn instance(shared: &Arc<Shared>, spec: &EgressSpec, info: &InstanceInfo<'_>) -> Instance {
        Instance {
            #[cfg(feature = "egress")]
            hooks: hooks::Hooks::new(shared, spec, info),
        }
    }

    #[cfg_attr(not(feature = "egress"), allow(unused_variables))]
    fn exited(shared: &Shared, pid: Pid) {
        #[cfg(feature = "egress")]
        shared.forget(pid);
    }
}

impl Instance {
    /// An app's HTTP request (ROADMAP Z8f).
    pub(crate) async fn native(
        &self,
        request: bulkhead_native::protocol::HttpRequest,
        trace: Option<crate::message::Trace>,
    ) -> Result<bulkhead_native::protocol::HttpResponse, bulkhead_native::protocol::HttpError> {
        #[cfg(feature = "egress")]
        {
            self.hooks.native(request, trace).await
        }
        #[cfg(not(feature = "egress"))]
        {
            let _ = (request, trace);
            Err(bulkhead_native::protocol::HttpError::Other(
                "this node was built without egress (cargo feature `egress`)".into(),
            ))
        }
    }
}

#[cfg(feature = "egress")]
mod hooks {
    use std::sync::{Arc, PoisonError};
    use std::time::Duration;

    use bulkhead_native::protocol as p;
    use serde::{Deserialize, Serialize};
    use tokio::sync::Semaphore;
    use tokio::time::Instant;

    use super::policy::{self, Allow, Scheme};
    use super::{EGRESS_SCHEMA, EgressSpec, InstanceInfo, Shared};
    use crate::message::{CallError, Envelope};
    use crate::metrics::AppStats;

    const SENT: usize = 0;
    const DENIED: usize = 1;
    const LIMITED: usize = 2;
    const FAILED: usize = 3;

    /// How often a request is tried again while the app's driver is not
    /// running.
    const RETRY_EVERY: Duration = Duration::from_millis(100);

    /// The body the driver reads per wait in the worst case: the
    /// between-bytes timeout counts once per this much of the cap.
    const CHUNK: u64 = 64 << 10;

    /// What the node sends the driver (`bulkhead_egress::EgressRequest`,
    /// the same fields).
    #[derive(Debug, Serialize, Deserialize)]
    struct EgressRequest {
        request: p::HttpRequest,
        first_byte_ms: u64,
        between_ms: u64,
    }

    /// The filter for one instance.
    pub(crate) struct Hooks {
        granted: bool,
        app: String,
        driver: String,
        allow: Vec<Allow>,
        methods: Vec<String>,
        limit: Arc<Semaphore>,
        shared: Arc<Shared>,
        stats: Arc<AppStats>,
    }

    impl Hooks {
        pub(crate) fn new(
            shared: &Arc<Shared>,
            spec: &EgressSpec,
            info: &InstanceInfo<'_>,
        ) -> Self {
            let app = info.handle.app().to_string();
            // The node refused bad entries at deploy; keep the good ones.
            let allow: Vec<Allow> = spec
                .allow
                .iter()
                .filter_map(|e| policy::parse(e).ok())
                .collect();
            let max = usize::try_from(spec.max_concurrent.unwrap_or(shared.config.max_concurrent))
                .unwrap_or(usize::MAX)
                .min(Semaphore::MAX_PERMITS);
            let limit = {
                let mut limits = shared.limits.lock().unwrap_or_else(PoisonError::into_inner);
                let entry = limits
                    .entry(app.clone())
                    .or_insert_with(|| (max, Arc::new(Semaphore::new(max))));
                // An upgrade that changes the limit starts a new count;
                // requests in flight finish on the old one.
                if entry.0 != max {
                    *entry = (max, Arc::new(Semaphore::new(max)));
                }
                entry.1.clone()
            };
            shared.set_allow(
                &app,
                info.handle.pid(),
                (info.granted && !allow.is_empty()).then(|| allow.clone()),
            );
            Self {
                granted: info.granted,
                stats: info.metrics.stats(&app),
                driver: crate::config::egress_driver_app(&app),
                app,
                allow,
                methods: spec.methods.clone(),
                limit,
                shared: shared.clone(),
            }
        }

        /// Why `request` may not go out, if it may not: the grant, the
        /// URL's scheme, host and port against `allow`, the method against
        /// `methods`, and forbidden headers (`Invalid`).
        fn refuse(&self, request: &p::HttpRequest) -> Result<(), p::HttpError> {
            let invalid = |what: &str, e: &dyn std::fmt::Display| {
                p::HttpError::Invalid(format!("{what}: {e}"))
            };
            let method = http::Method::from_bytes(request.method.as_bytes())
                .map_err(|e| invalid("method", &e))?;
            let uri: http::Uri = request.url.parse().map_err(|e| invalid("url", &e))?;
            for (name, value) in &request.headers {
                let name = http::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|e| invalid("header", &e))?;
                if forbidden(&name) {
                    return Err(p::HttpError::Invalid(format!("header {name} is forbidden")));
                }
                http::header::HeaderValue::from_bytes(&value.0)
                    .map_err(|e| invalid("header", &e))?;
            }
            let denied = |why: &str| {
                tracing::debug!(app = %self.app, uri = %uri, why, "egress request denied");
                Err(p::HttpError::Denied)
            };
            if !self.granted {
                return denied("the capability is not granted");
            }
            let scheme = match uri.scheme_str() {
                Some("https") => Scheme::Https,
                Some("http") => Scheme::Http,
                _ => return denied("not http or https"),
            };
            let Some(host) = uri.host() else {
                return denied("no host");
            };
            let port = uri.port_u16().unwrap_or(match scheme {
                Scheme::Https => 443,
                Scheme::Http => 80,
            });
            if !self.allow.iter().any(|a| a.allows(scheme, host, port)) {
                return denied("not in the manifest's allow list");
            }
            if !self.methods.is_empty() && !self.methods.iter().any(|m| m == method.as_str()) {
                return denied("method not in the manifest's methods");
            }
            Ok(())
        }

        /// An app's request: checked against the filter, then sent by the
        /// app's egress driver while the app's permit is held; with
        /// `trace`, the requesting callback's, as a child span of it
        /// (ROADMAP O1), which the driver sends on as `traceparent`.
        pub(crate) async fn native(
            &self,
            request: p::HttpRequest,
            trace: Option<crate::message::Trace>,
        ) -> Result<p::HttpResponse, p::HttpError> {
            if let Err(e) = self.refuse(&request) {
                if e == p::HttpError::Denied {
                    self.stats.egress(DENIED);
                }
                return Err(e);
            }
            let Ok(_permit) = self.limit.clone().try_acquire_owned() else {
                self.stats.egress(LIMITED);
                return Err(p::HttpError::Limited);
            };
            let config = &self.shared.config;
            let asked = request
                .timeout_ms
                .map(|ms| Duration::from_millis(ms.into()));
            let shorter = |node: Duration| asked.map_or(node, |a| a.min(node));
            let (first_byte, between) = (
                shorter(config.first_byte_timeout),
                shorter(config.between_bytes_timeout),
            );
            let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
            let body = EgressRequest {
                request,
                first_byte_ms: ms(first_byte),
                between_ms: ms(between),
            };
            // Longer than the driver may take: a connect, the headers, and
            // a wait per chunk of the largest body.
            let chunks =
                u32::try_from(config.max_response_body.div_ceil(CHUNK)).unwrap_or(u32::MAX);
            let backstop =
                config.connect_timeout + first_byte + between.saturating_mul(chunks.max(1));
            let mut cbor = Vec::new();
            let response = match ciborium::into_writer(&body, &mut cbor) {
                Ok(()) => {
                    let msg = Envelope {
                        content_type: "application/cbor".into(),
                        schema: Some(EGRESS_SCHEMA.into()),
                        body: cbor,
                        trace: trace.map(|t| Box::new(t.child())),
                    };
                    self.ask(msg, backstop).await
                }
                Err(e) => Err(p::HttpError::Other(format!("request: {e}"))),
            };
            self.stats
                .egress(if response.is_ok() { SENT } else { FAILED });
            response
        }

        /// `msg` to the app's driver, and its answer. While the driver is
        /// not running (starting, or restarting) it is tried again until
        /// the connect timeout.
        async fn ask(
            &self,
            msg: Envelope,
            timeout: Duration,
        ) -> Result<p::HttpResponse, p::HttpError> {
            let deadline = Instant::now() + self.shared.config.connect_timeout;
            loop {
                let Some(rt) = self.shared.rt.get().and_then(std::sync::Weak::upgrade) else {
                    return Err(p::HttpError::Other("the runtime is stopping".into()));
                };
                let target = rt.route_app(&self.driver, &msg);
                drop(rt);
                if let Some(target) = target {
                    return match target.call(msg, timeout).await {
                        Ok(reply) => answer(&self.driver, &reply),
                        Err(CallError::Timeout) => Err(p::HttpError::Timeout),
                        Err(e) => Err(p::HttpError::Other(format!("{}: {e}", self.driver))),
                    };
                }
                if Instant::now() + RETRY_EVERY >= deadline {
                    return Err(p::HttpError::Other(format!(
                        "{} (the app's egress driver) is not running",
                        self.driver
                    )));
                }
                tokio::time::sleep(RETRY_EVERY).await;
            }
        }
    }

    /// The driver's answer, from its reply.
    fn answer(driver: &str, reply: &Envelope) -> Result<p::HttpResponse, p::HttpError> {
        if reply.schema.as_deref() != Some(EGRESS_SCHEMA) {
            return Err(p::HttpError::Other(format!("{driver}: not an answer")));
        }
        ciborium::from_reader::<Result<p::HttpResponse, p::HttpError>, _>(reply.body.as_slice())
            .map_err(|e| p::HttpError::Other(format!("{driver}'s answer: {e}")))?
    }

    /// Whether `name` is a header an app may not set (hop-by-hop headers
    /// and `host`).
    fn forbidden(name: &http::header::HeaderName) -> bool {
        use http::header;
        matches!(
            *name,
            header::CONNECTION
                | header::PROXY_AUTHENTICATE
                | header::PROXY_AUTHORIZATION
                | header::TRANSFER_ENCODING
                | header::UPGRADE
                | header::HOST
        ) || matches!(
            name.as_str(),
            "keep-alive" | "proxy-connection" | "http2-settings"
        )
    }
}
