//! The driver's init arguments: what the node writes from node config's
//! `[http.<name>]` blocks (ROADMAP DA2), as JSON.
//!
//! ```json
//! {"threads": 4,
//!  "listeners": [{"name": "web", "max_connections": 1024,
//!                 "header_timeout_ms": 10000, "body_timeout_ms": 30000,
//!                 "max_response": 16777216,
//!                 "standby": {"redirect": {"partner_url": "https://node2:8443"}},
//!                 "tls": [{"cert": "/driver/web.crt", "key": "/driver/web.key",
//!                          "hosts": ["shop.example"]}],
//!                 "tls_reload_s": 60}]}
//! ```
//!
//! Each listener is a listening socket the node passes by the same name
//! (`bulkhead_native::driver`); its certificate and key files are guest
//! paths under `/driver/`.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::Deserialize;

/// Most worker threads.
pub const MAX_THREADS: usize = 64;

/// The whole configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Worker threads, each with a runtime and a sender channel of its
    /// own.
    #[serde(default = "default_threads")]
    pub threads: usize,
    /// The listening sockets to serve.
    pub listeners: Vec<Listener>,
}

fn default_threads() -> usize {
    4
}

/// One listening socket and its limits, as `[http.<name>]` gives them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listener {
    /// The name the node passes its socket by.
    pub name: String,
    /// Most connections open at once; more are closed on accept.
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    /// Time a client has to send a request's headers (and finish a TLS
    /// handshake), in milliseconds.
    #[serde(default = "default_header_timeout_ms")]
    pub header_timeout_ms: u64,
    /// Time a client has to send a request's body (408 after), in
    /// milliseconds.
    #[serde(default = "default_body_timeout_ms")]
    pub body_timeout_ms: u64,
    /// Largest reply body from an app, in bytes (502 above it).
    #[serde(default = "default_max_response")]
    pub max_response: u64,
    /// What to do with a request for an app that runs on the partner now.
    #[serde(default)]
    pub standby: Standby,
    /// Serve HTTPS with these certificates, the first the default; none
    /// serves plain HTTP.
    #[serde(default)]
    pub tls: Vec<Certificate>,
    /// Check the certificate files for changes this often, in seconds; 0
    /// reloads only when the node asks.
    #[serde(default)]
    pub tls_reload_s: u64,
    /// Ask HTTPS clients for a certificate from these CAs (ROADMAP DA2b).
    #[serde(default)]
    pub client_auth: Option<ClientAuth>,
}

/// Client certificates on an HTTPS listener.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientAuth {
    /// The CAs' PEM file, a guest path under `/driver/`.
    pub ca: String,
    /// Refuse a client without one; otherwise it is served as before,
    /// with no `client_cert` in its requests.
    #[serde(default = "default_required")]
    pub required: bool,
}

fn default_required() -> bool {
    true
}

fn default_max_connections() -> usize {
    1_024
}
fn default_header_timeout_ms() -> u64 {
    10_000
}
fn default_body_timeout_ms() -> u64 {
    30_000
}
fn default_max_response() -> u64 {
    16 << 20
}

impl Listener {
    /// [`Listener::header_timeout_ms`] as a duration.
    #[must_use]
    pub fn header_timeout(&self) -> Duration {
        Duration::from_millis(self.header_timeout_ms)
    }

    /// [`Listener::body_timeout_ms`] as a duration.
    #[must_use]
    pub fn body_timeout(&self) -> Duration {
        Duration::from_millis(self.body_timeout_ms)
    }
}

/// What a listener does with a request for an app that runs on the
/// partner node now, not here.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Standby {
    /// `503 app not running`, as a load balancer's health check expects.
    #[default]
    Refuse,
    /// `307 Temporary Redirect` to the same path and query under the
    /// partner's URL for this listener (no trailing `/`).
    Redirect {
        /// The partner's address as clients reach it.
        partner_url: String,
    },
    /// Send the request to the app on the partner, through the node.
    Forward,
}

/// One certificate: its chain and key files, and the hosts it serves.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Certificate {
    /// PEM certificate chain, leaf first: a guest path.
    pub cert: String,
    /// PEM private key (PKCS#8, PKCS#1 or SEC1): a guest path.
    pub key: String,
    /// Host names it serves (`shop.example`, `*.example.com`); empty for
    /// the default only.
    #[serde(default)]
    pub hosts: Vec<String>,
}

impl Config {
    /// The configuration in `args`, checked.
    ///
    /// # Errors
    ///
    /// Why it is not a configuration: not this JSON, no listeners or more
    /// than `bulkhead_native::driver::MAX_LISTENERS`, a name twice,
    /// threads outside 1 to [`MAX_THREADS`], a zero limit or timeout.
    pub fn from_args(args: &[u8]) -> Result<Self, String> {
        let config: Self =
            serde_json::from_slice(args).map_err(|e| format!("init arguments: {e}"))?;
        config.check()?;
        Ok(config)
    }

    fn check(&self) -> Result<(), String> {
        let max = bulkhead_native::driver::MAX_LISTENERS;
        if self.listeners.is_empty() || self.listeners.len() > max {
            return Err(format!("between 1 and {max} listeners"));
        }
        if !(1..=MAX_THREADS).contains(&self.threads) {
            return Err(format!("threads must be 1 to {MAX_THREADS}"));
        }
        let mut names = BTreeSet::new();
        for l in &self.listeners {
            if !names.insert(l.name.as_str()) {
                return Err(format!("listener {} twice", l.name));
            }
            if l.max_connections == 0
                || l.header_timeout_ms == 0
                || l.body_timeout_ms == 0
                || l.max_response == 0
            {
                return Err(format!(
                    "listener {}: limits and timeouts must be above 0",
                    l.name
                ));
            }
            if let Standby::Redirect { partner_url } = &l.standby
                && partner_url.is_empty()
            {
                return Err(format!(
                    "listener {}: a redirect needs a partner URL",
                    l.name
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
