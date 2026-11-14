//! Host names to addresses (ROADMAP DA9), for apps' `connect_host`, HTTP
//! egress and datasources. The node parses no DNS: a name is a call to the
//! resolver driver (`bulkhead-resolve`), which answers addresses the
//! asker then checks against its own rules. An address written as text is
//! read here and never sent.
//!
//! [`Resolver`] says where names go: the driver by app name (a node), a
//! table (tests), or nowhere. There is no fallback to the system's
//! resolver.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::message::{CallError, Envelope};
use crate::runtime::RuntimeInner;

/// The schema of a lookup call (`bulkhead_resolve::lookup::RESOLVE_SCHEMA`).
pub const RESOLVE_SCHEMA: &str = "bulkhead/resolve";

/// How long a lookup waits between tries while the driver is not running
/// (restarting after a crash or a changed `resolv.conf`).
const RETRY_EVERY: Duration = Duration::from_millis(100);

/// Where host names are resolved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Resolver {
    /// Nowhere: every name fails ([`ResolveError::Refused`]).
    #[default]
    None,
    /// The resolver driver, the app of this name.
    App(String),
    /// A fixed table, by lower-case name: for tests.
    Table(BTreeMap<String, Vec<IpAddr>>),
}

/// Why a name gave no address.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// The name does not exist, or has no address.
    #[error("{0}: no such name")]
    NotFound(String),
    /// No answer in time, or the resolver is not running: trying again
    /// may work.
    #[error("{0}")]
    Temporary(String),
    /// Not a name to look up, or not this asker's to look up.
    #[error("{0}")]
    Refused(String),
}

/// The driver's answer (`bulkhead_resolve::lookup::Answer`).
#[derive(Debug, Deserialize)]
struct Answer {
    #[serde(default)]
    addrs: Vec<IpAddr>,
    #[serde(default)]
    failure: Option<Failure>,
    #[serde(default)]
    detail: String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Failure {
    NotFound,
    Temporary,
    Invalid,
}

/// The runtime's resolver, shared by the services that look names up. It
/// holds the runtime weakly: the egress client keeps a copy, and the
/// runtime keeps the client.
#[derive(Debug, Clone, Default)]
pub struct Names {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    resolver: Resolver,
    rt: OnceLock<Weak<RuntimeInner>>,
}

impl Names {
    pub(crate) fn new(resolver: Resolver) -> Self {
        Self {
            inner: Arc::new(Inner {
                resolver,
                rt: OnceLock::new(),
            }),
        }
    }

    /// Calls to the driver go through `rt`.
    pub(crate) fn attach(&self, rt: &Arc<RuntimeInner>) {
        let _ = self.inner.rt.set(Arc::downgrade(rt));
    }

    /// Whether `app` is the resolver driver, which may not ask for names
    /// itself (its own lookups would be calls to itself).
    pub(crate) fn is_resolver(&self, app: &str) -> bool {
        matches!(&self.inner.resolver, Resolver::App(name) if name == app)
    }

    /// `host`'s addresses on `port`, within `timeout`. An address written
    /// as text (`10.0.0.1`, `::1`, `[::1]`) is read here.
    ///
    /// # Errors
    ///
    /// [`ResolveError`]: no such name, no answer in time, or no resolver.
    pub async fn resolve(
        &self,
        host: &str,
        port: u16,
        timeout: Duration,
    ) -> Result<Vec<SocketAddr>, ResolveError> {
        let bare = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        if let Ok(ip) = bare.parse::<IpAddr>() {
            return Ok(vec![SocketAddr::new(ip, port)]);
        }
        let ips = match &self.inner.resolver {
            Resolver::None => {
                return Err(ResolveError::Refused(format!(
                    "{host}: this runtime resolves no host names"
                )));
            }
            Resolver::Table(table) => table
                .get(&host.to_ascii_lowercase())
                .cloned()
                .ok_or_else(|| ResolveError::NotFound(host.to_string()))?,
            Resolver::App(app) => self.ask(app, host, timeout).await?,
        };
        Ok(ips
            .into_iter()
            .map(|ip| SocketAddr::new(ip, port))
            .collect())
    }

    /// A call to the driver `app`; while it is not running, or if it ends
    /// before answering, tried again until `timeout`.
    async fn ask(
        &self,
        app: &str,
        host: &str,
        timeout: Duration,
    ) -> Result<Vec<IpAddr>, ResolveError> {
        let deadline = Instant::now() + timeout;
        let msg = Envelope {
            content_type: "application/json".into(),
            schema: Some(RESOLVE_SCHEMA.into()),
            body: serde_json::to_vec(&serde_json::json!({ "host": host }))
                .map_err(|e| ResolveError::Refused(e.to_string()))?,
        };
        let reply = loop {
            let Some(rt) = self.inner.rt.get().and_then(Weak::upgrade) else {
                return Err(ResolveError::Temporary("the runtime is stopping".into()));
            };
            let left = deadline.saturating_duration_since(Instant::now());
            let result = match rt.route_app(app, &msg) {
                Some(target) => target.call(msg.clone(), left).await,
                None => Err(CallError::Noproc),
            };
            drop(rt);
            match result {
                Ok(reply) => break reply,
                // Not running, or ended while it looked the name up (a
                // crash, a restart with changed files): a lookup changes
                // nothing, so it is asked again.
                Err(CallError::Noproc | CallError::Exited(_))
                    if Instant::now() + RETRY_EVERY < deadline =>
                {
                    tokio::time::sleep(RETRY_EVERY).await;
                }
                Err(CallError::Noproc | CallError::Exited(_)) => {
                    return Err(ResolveError::Temporary(format!(
                        "{host}: the resolver ({app}) is not running"
                    )));
                }
                Err(CallError::Timeout) => {
                    return Err(ResolveError::Temporary(format!(
                        "{host}: no answer from the resolver in {timeout:?}"
                    )));
                }
                Err(e) => {
                    return Err(ResolveError::Temporary(format!(
                        "{host}: the resolver: {e}"
                    )));
                }
            }
        };
        let answer: Answer = serde_json::from_slice(&reply.body)
            .map_err(|e| ResolveError::Temporary(format!("{host}: the resolver's answer: {e}")))?;
        let detail = if answer.detail.is_empty() {
            host.to_string()
        } else {
            format!("{host}: {}", answer.detail)
        };
        match (answer.failure, answer.addrs.is_empty()) {
            (None, false) => Ok(answer.addrs),
            (Some(Failure::Invalid), _) => Err(ResolveError::Refused(detail)),
            (Some(Failure::Temporary), _) => Err(ResolveError::Temporary(detail)),
            (Some(Failure::NotFound) | None, _) => Err(ResolveError::NotFound(host.to_string())),
        }
    }
}

#[cfg(test)]
#[path = "names_tests.rs"]
mod tests;
