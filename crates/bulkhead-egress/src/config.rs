//! The init arguments the node writes for one app's egress driver (JSON):
//!
//! ```json
//! {"app": "shop", "connect_timeout_ms": 10000,
//!  "pool_idle_timeout_ms": 90000, "pool_max_idle_per_host": 32,
//!  "max_response_body": 16777216,
//!  "roots_pem": "-----BEGIN CERTIFICATE-----\n..."}
//! ```
//!
//! `roots_pem` holds every certificate authority the driver trusts: the
//! system's (read by the node, when `[egress] system_roots`) and
//! `[egress] ca_file`'s. Empty, no HTTPS server is trusted.

use std::time::Duration;

use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;
use serde::{Deserialize, Serialize};

/// The init arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The app whose requests this driver sends, for messages.
    pub app: String,
    /// How long a connection may take: the driver's own bound on the node's
    /// connect.
    pub connect_timeout_ms: u64,
    /// How long an idle pooled connection is kept.
    pub pool_idle_timeout_ms: u64,
    /// Idle connections kept per host.
    pub pool_max_idle_per_host: usize,
    /// The largest response body read; a larger one fails the request.
    pub max_response_body: u64,
    /// The trusted certificate authorities, PEM.
    #[serde(default)]
    pub roots_pem: String,
}

impl Config {
    /// Read the init arguments.
    ///
    /// # Errors
    ///
    /// They are not this JSON object.
    pub fn from_args(args: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(args).map_err(|e| format!("init arguments: {e}"))
    }

    /// The connect timeout.
    #[must_use]
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_millis(self.connect_timeout_ms)
    }

    /// The pool's idle timeout.
    #[must_use]
    pub fn pool_idle_timeout(&self) -> Duration {
        Duration::from_millis(self.pool_idle_timeout_ms)
    }

    /// The trusted roots.
    ///
    /// # Errors
    ///
    /// `roots_pem` holds something that is not a certificate.
    pub fn roots(&self) -> Result<rustls::RootCertStore, String> {
        let mut roots = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(self.roots_pem.as_bytes()) {
            let cert = cert.map_err(|e| format!("roots: {e}"))?;
            roots.add(cert).map_err(|e| format!("roots: {e}"))?;
        }
        Ok(roots)
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
