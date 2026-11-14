//! The node's HTTP route table: the routes apps declare (manifest
//! `bulkhead:http-routes`), at most one app owning a host and path prefix,
//! and the whole table announced to driver apps as the host event
//! `routes` (ROADMAP DA1; `GUEST_FORMATS.md`, "Routes"). Free of hyper:
//! the node keeps it when its ingress runs as a driver (ROADMAP DA2).
//!
//! A request matches a route by host (any case; a route without one
//! matches every host) and by path prefix on segment boundaries; the
//! longest prefix wins, then a route naming the host.

use std::collections::BTreeMap;
use std::sync::{PoisonError, RwLock};
use std::time::Duration;

use bulkhead_bundle::manifest::{AppManifest, ByteSize};

use crate::error::HostError;
use crate::runtime::Runtime;

/// Largest request body when the manifest sets no `max_body`.
pub const DEFAULT_MAX_BODY: ByteSize = ByteSize::mib(1);
/// Reply timeout when the manifest sets no `timeout_ms`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// One route an app owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRoute {
    /// Host to match, without port, any case; `None` matches any host.
    pub host: Option<String>,
    /// Path prefix, matched on segment boundaries.
    pub path_prefix: String,
    /// A header whose value becomes the request's `key`.
    pub hash_header: Option<String>,
    /// Largest body accepted, in bytes.
    pub max_body: u64,
    /// Time allowed for the reply.
    pub timeout: Duration,
}

impl HttpRoute {
    /// The routes a manifest's `bulkhead:http-routes` scope declares
    /// (none if it declares no scope).
    #[must_use]
    pub fn from_manifest(manifest: &AppManifest) -> Vec<Self> {
        let Some(scope) = &manifest.spec.capabilities.http_incoming else {
            return Vec::new();
        };
        let max_body = scope.max_body.unwrap_or(DEFAULT_MAX_BODY).bytes();
        let timeout = scope
            .timeout_ms
            .map_or(DEFAULT_TIMEOUT, Duration::from_millis);
        scope
            .routes
            .iter()
            .map(|r| Self {
                host: r.host.as_ref().map(|h| h.to_ascii_lowercase()),
                path_prefix: r.path_prefix.clone(),
                hash_header: r.hash_header.clone(),
                max_body,
                timeout,
            })
            .collect()
    }

    /// Whether a request for `host` (without port) and `path` matches.
    #[must_use]
    pub fn matches(&self, host: Option<&str>, path: &str) -> bool {
        let host_ok = match (&self.host, host) {
            (None, _) => true,
            (Some(want), Some(got)) => want.eq_ignore_ascii_case(got),
            (Some(_), None) => false,
        };
        host_ok && prefix_matches(&self.path_prefix, path)
    }
}

/// Whether `prefix` covers `path` on a segment boundary.
fn prefix_matches(prefix: &str, path: &str) -> bool {
    let Some(rest) = path.strip_prefix(prefix) else {
        return false;
    };
    prefix.ends_with('/') || rest.is_empty() || rest.starts_with('/')
}

/// The node's routes by app. Changes are announced to driver apps.
#[derive(Debug)]
pub struct RouteTable {
    runtime: Runtime,
    routes: RwLock<BTreeMap<String, Vec<HttpRoute>>>,
}

impl RouteTable {
    /// An empty table announcing its changes on `runtime`.
    #[must_use]
    pub fn new(runtime: Runtime) -> Self {
        Self {
            runtime,
            routes: RwLock::default(),
        }
    }

    /// Give `app` exactly `routes`, replacing any it had.
    ///
    /// # Errors
    ///
    /// [`HostError::InvalidApps`] if another app owns a route with the same
    /// host and prefix; nothing changes then.
    pub fn set(&self, app: &str, routes: Vec<HttpRoute>) -> Result<(), HostError> {
        let mut table = self.routes.write().unwrap_or_else(PoisonError::into_inner);
        if let Some((owner, route)) = conflict(&table, app, &routes) {
            return Err(HostError::InvalidApps(format!(
                "HTTP route {}{} belongs to {owner}",
                route.host.as_deref().unwrap_or("*"),
                route.path_prefix
            )));
        }
        if routes.is_empty() {
            table.remove(app);
        } else {
            table.insert(app.to_string(), routes);
        }
        let (json, owners) = (routes_json(&table), table.keys().cloned().collect());
        drop(table);
        self.runtime.set_http_routes(json, owners);
        Ok(())
    }

    /// Withdraw `app`'s routes.
    pub fn remove(&self, app: &str) {
        let mut table = self.routes.write().unwrap_or_else(PoisonError::into_inner);
        table.remove(app);
        let (json, owners) = (routes_json(&table), table.keys().cloned().collect());
        drop(table);
        self.runtime.set_http_routes(json, owners);
    }

    /// `app`'s routes.
    #[must_use]
    pub fn routes(&self, app: &str) -> Vec<HttpRoute> {
        self.routes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(app)
            .cloned()
            .unwrap_or_default()
    }

    /// The best route for a request, and its app.
    #[must_use]
    pub fn pick(&self, host: Option<&str>, path: &str) -> Option<(String, HttpRoute)> {
        pick(
            &self.routes.read().unwrap_or_else(PoisonError::into_inner),
            host,
            path,
        )
    }
}

/// A route of `routes` whose host and prefix another app than `app` owns,
/// and that app.
fn conflict<'a>(
    table: &'a BTreeMap<String, Vec<HttpRoute>>,
    app: &str,
    routes: &'a [HttpRoute],
) -> Option<(&'a str, &'a HttpRoute)> {
    routes.iter().find_map(|route| {
        table
            .iter()
            .filter(|(owner, _)| *owner != app)
            .find(|(_, rs)| {
                rs.iter()
                    .any(|r| r.host == route.host && r.path_prefix == route.path_prefix)
            })
            .map(|(owner, _)| (owner.as_str(), route))
    })
}

/// The route table as driver apps get it (host event `routes`, ROADMAP
/// DA1; `GUEST_FORMATS.md`, "Routes"): every route with its app, in app
/// order.
fn routes_json(table: &BTreeMap<String, Vec<HttpRoute>>) -> String {
    let routes: Vec<serde_json::Value> = table
        .iter()
        .flat_map(|(app, rs)| {
            rs.iter().map(move |r| {
                serde_json::json!({
                    "app": app,
                    "host": r.host,
                    "path_prefix": r.path_prefix,
                    "hash_header": r.hash_header,
                    "max_body": r.max_body,
                    "timeout_ms": u64::try_from(r.timeout.as_millis()).unwrap_or(u64::MAX),
                })
            })
        })
        .collect();
    serde_json::json!({ "routes": routes }).to_string()
}

/// The best route for a request: longest prefix, and a route naming the
/// host before one that does not.
fn pick(
    table: &BTreeMap<String, Vec<HttpRoute>>,
    host: Option<&str>,
    path: &str,
) -> Option<(String, HttpRoute)> {
    table
        .iter()
        .flat_map(|(app, rs)| rs.iter().map(move |r| (app, r)))
        .filter(|(_, r)| r.matches(host, path))
        .max_by_key(|(_, r)| (r.path_prefix.len(), r.host.is_some()))
        .map(|(app, r)| (app.clone(), r.clone()))
}

#[cfg(test)]
#[path = "routes_tests.rs"]
mod routes_tests;
