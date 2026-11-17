//! Pinned public keys, as WireGuard pins them, with no certificate
//! authority: what a node, its operators' CLI and the drivers that speak
//! for the node all read.
//!
//! - [`key`]: Ed25519 key pairs and public keys (`ed25519:<base64>`),
//!   read from PEM files or text.
//! - [`tls`]: TLS 1.3 with raw public keys (RFC 7250) accepting only
//!   listed keys: the cluster's QUIC, and the remote admin API's server
//!   and client.
//! - [`operators`]: the operators file, who may administer a node.
//! - [`signature`]: an operator's signature on each remote admin request,
//!   which the node checks itself, so that whatever carries the request
//!   (the admin driver, ROADMAP DA7) cannot forge or replay one.
//! - [`check_key_permissions`]: a private key file its owner alone reads.

#![forbid(unsafe_code)]

pub mod key;
mod key_file;
pub mod operators;
pub mod signature;
pub mod tls;

pub use key::{KeyError, PublicKey, generate_key, load_key, load_key_pem, public_key_of};
pub use key_file::check_key_permissions;
pub use operators::parse_operators;
pub use tls::{Accepts, PinnedKeys, admin_client_tls, admin_server_tls, presented_key};
