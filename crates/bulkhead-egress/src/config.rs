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
//!
//! `client_cert` and `client_key`, with node config's
//! `[egress.identities.<app>]` (ROADMAP DA4b), are the paths of the app's
//! client certificate and key in the driver's root (`/driver/...`), read
//! at its start and presented when a server asks for a certificate.

use std::time::Duration;

use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
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
    /// The client certificate chain's path (PEM), with `client_key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert: Option<String>,
    /// The client key's path (PEM), with `client_cert`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key: Option<String>,
    /// Whether a traced request carries a `traceparent` header (ROADMAP O1;
    /// node config's `[egress] traceparent`, true by default).
    #[serde(default = "yes")]
    pub traceparent: bool,
}

fn yes() -> bool {
    true
}

/// A client certificate chain and its key.
pub type Identity = (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>);

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

impl Config {
    /// The client identity, its files read with `read` (the driver's
    /// `bulkhead_sdk::files::read`); `None` without one.
    ///
    /// # Errors
    ///
    /// Only one of the two set, a file not read, or not a certificate chain
    /// and a private key.
    pub fn identity(
        &self,
        read: impl Fn(&str) -> std::io::Result<Vec<u8>>,
    ) -> Result<Option<Identity>, String> {
        let (cert, key) = match (&self.client_cert, &self.client_key) {
            (None, None) => return Ok(None),
            (Some(c), Some(k)) => (c, k),
            _ => return Err("client_cert and client_key go together".into()),
        };
        let cert_pem = read(cert).map_err(|e| format!("client certificate {cert}: {e}"))?;
        let chain = CertificateDer::pem_slice_iter(&cert_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("client certificate {cert}: {e}"))?;
        if chain.is_empty() {
            return Err(format!("client certificate {cert}: no certificate"));
        }
        let key_pem = read(key).map_err(|e| format!("client key {key}: {e}"))?;
        let key_der = PrivateKeyDer::from_pem_slice(&key_pem)
            .map_err(|e| format!("client key {key}: {e}"))?;
        Ok(Some((chain, key_der)))
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
