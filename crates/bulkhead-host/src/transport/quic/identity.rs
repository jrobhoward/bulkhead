//! Node identity for the QUIC transport: pinned public keys, as WireGuard
//! does, with no certificate authority.
//!
//! Each node has one Ed25519 key pair ([`generate_key`]). Its private key
//! stays in a file on the node; its public key ([`PublicKey`], written
//! `ed25519:<base64>`) goes into every other node's peers file, against the
//! node's id:
//!
//! ```toml
//! # peers.toml
//! [peers]
//! 1 = ["ed25519:MCowBQYDK2VwAyEA..."]
//! 2 = ["ed25519:MCowBQYDK2VwAyEA...", "ed25519:...new key during a rotation..."]
//! ```
//!
//! The TLS 1.3 handshake carries raw public keys (RFC 7250), not
//! certificates: each side proves it holds its private key, and accepts
//! the other only if the key is in its peers file. Nothing expires. To
//! rotate a node's key, list the new key beside the old at every peer,
//! reload them, switch the node to its new key, then remove the old one.
//!
//! The keys, the TLS verifier and the remote admin API's TLS are
//! `bulkhead_keys`', shared with the CLI and the drivers.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bulkhead_keys::PinnedKeys;
pub use bulkhead_keys::{
    Accepts, PublicKey, admin_client_tls, admin_server_tls, parse_operators, presented_key,
};
use rustls::sign::CertifiedKey;
use serde::Deserialize;

use crate::error::HostError;

/// A new Ed25519 key pair: the private key as PKCS#8 PEM, and its public
/// key.
///
/// # Errors
///
/// If the system's random source fails.
pub fn generate_key() -> Result<(String, PublicKey), HostError> {
    bulkhead_keys::generate_key().map_err(identity_error)
}

/// The public key of the private key in PEM file `path`.
///
/// # Errors
///
/// [`HostError::Listen`] if the file cannot be read, its group or others
/// may access it, or it holds no usable key.
pub fn public_key_of(path: &Path) -> Result<PublicKey, HostError> {
    bulkhead_keys::public_key_of(path).map_err(identity_error)
}

/// Where a node's identity lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeIdentity {
    /// This node's private key (PKCS#8 PEM).
    pub key: PathBuf,
    /// The peers file: each node's accepted public keys.
    pub peers: PathBuf,
}

/// The identity as loaded: this node's key and every peer's keys.
#[derive(Debug, Clone)]
pub(crate) struct Loaded {
    pub(crate) certified: Arc<CertifiedKey>,
    pub(crate) peers: Arc<BTreeMap<u32, Vec<PublicKey>>>,
}

fn identity_error(e: impl std::fmt::Display) -> HostError {
    HostError::Listen(format!("cluster identity: {e}"))
}

fn load_key(path: &Path) -> Result<(Arc<CertifiedKey>, PublicKey), HostError> {
    bulkhead_keys::load_key(path).map_err(identity_error)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PeersFile {
    #[serde(default)]
    peers: BTreeMap<String, Vec<String>>,
}

/// Parse a peers file's text.
pub(crate) fn parse_peers(text: &str) -> Result<BTreeMap<u32, Vec<PublicKey>>, String> {
    let file: PeersFile = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut peers = BTreeMap::new();
    for (node, keys) in file.peers {
        let id: u32 = node
            .parse()
            .map_err(|_| format!("peers.{node}: a node id is a number"))?;
        let keys = keys
            .iter()
            .map(|k| k.parse().map_err(|e| format!("peers.{node}: {e}")))
            .collect::<Result<Vec<PublicKey>, String>>()?;
        peers.insert(id, keys);
    }
    Ok(peers)
}

impl NodeIdentity {
    pub(crate) fn load(&self) -> Result<Loaded, HostError> {
        let (certified, _) = load_key(&self.key)?;
        let text = std::fs::read_to_string(&self.peers)
            .map_err(|e| identity_error(format!("{}: {e}", self.peers.display())))?;
        let peers = parse_peers(&text)
            .map_err(|e| identity_error(format!("{}: {e}", self.peers.display())))?;
        Ok(Loaded {
            certified,
            peers: Arc::new(peers),
        })
    }

    /// The files' modification times, to notice a change.
    pub(crate) fn stamps(&self) -> [Option<std::time::SystemTime>; 2] {
        [&self.key, &self.peers].map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
    }
}

impl Loaded {
    /// Whether `key` is one of node `node`'s keys.
    pub(crate) fn names(&self, node: u32, key: &PublicKey) -> bool {
        self.peers.get(&node).is_some_and(|keys| keys.contains(key))
    }

    fn known(&self, key: &PublicKey) -> bool {
        self.peers.values().any(|keys| keys.contains(key))
    }
}

/// The cluster's verifier: any key in the peers file (which node it
/// belongs to is checked against the peer's hello afterwards).
pub(crate) fn pinned(loaded: Loaded) -> PinnedKeys {
    PinnedKeys::accepting(Arc::new(move |key: &PublicKey| loaded.known(key)))
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod identity_tests;
