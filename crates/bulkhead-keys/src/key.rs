//! Ed25519 key pairs and public keys.
//!
//! A private key lives in a PKCS#8 PEM file its owner alone reads
//! ([`load_key`] checks); its public key, written `ed25519:<base64>` of its
//! DER SubjectPublicKeyInfo, goes into peers and operators files and onto
//! the command line. A key given to a driver under the privileged helper is
//! read by the driver's group, so a driver reads it as bytes
//! ([`load_key_pem`]), which checks nothing about a file.

use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rustls::crypto::ring::sign::any_supported_type;
use rustls::sign::CertifiedKey;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, SubjectPublicKeyInfoDer};

use crate::check_key_permissions;

/// The prefix of a public key's text form.
const PREFIX: &str = "ed25519:";

/// The DER SubjectPublicKeyInfo of an Ed25519 key, before its 32 bytes.
const ED25519_SPKI: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// Why a key could not be read or used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct KeyError(pub String);

fn error(e: impl std::fmt::Display) -> KeyError {
    KeyError(e.to_string())
}

/// A public key: its DER SubjectPublicKeyInfo.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PublicKey(Vec<u8>);

impl std::fmt::Display for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{PREFIX}{}", STANDARD.encode(&self.0))
    }
}

impl std::str::FromStr for PublicKey {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let b64 = s
            .strip_prefix(PREFIX)
            .ok_or_else(|| format!("a public key starts with '{PREFIX}'"))?;
        let der = STANDARD
            .decode(b64.trim())
            .map_err(|e| format!("public key: {e}"))?;
        webpki::RawPublicKeyEntity::try_from(&SubjectPublicKeyInfoDer::from(der.as_slice()))
            .map_err(|e| format!("public key: {e}"))?;
        Ok(Self(der))
    }
}

impl PublicKey {
    /// The key a peer presented in a handshake, as DER.
    #[must_use]
    pub fn from_der(der: &[u8]) -> Self {
        Self(der.to_vec())
    }

    /// Its DER SubjectPublicKeyInfo.
    #[must_use]
    pub fn der(&self) -> &[u8] {
        &self.0
    }

    /// Its 32 bytes, if it is an Ed25519 key.
    #[must_use]
    pub fn ed25519(&self) -> Option<&[u8]> {
        self.0
            .strip_prefix(&ED25519_SPKI[..])
            .filter(|raw| raw.len() == 32)
    }

    /// The public key of an Ed25519 key's 32 bytes.
    #[must_use]
    pub fn from_ed25519(raw: &[u8]) -> Self {
        let mut der = ED25519_SPKI.to_vec();
        der.extend_from_slice(raw);
        Self(der)
    }
}

/// A new Ed25519 key pair: the private key as PKCS#8 PEM, and its public
/// key.
///
/// # Errors
///
/// If the system's random source fails.
pub fn generate_key() -> Result<(String, PublicKey), KeyError> {
    let pem = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)
        .map_err(error)?
        .serialize_pem();
    let (_, public) = load_key_pem(pem.as_bytes())?;
    Ok((pem, public))
}

/// The certified raw key a private key makes, and its public key.
fn certify(key: &PrivateKeyDer<'_>) -> Result<(Arc<CertifiedKey>, PublicKey), KeyError> {
    let signer = any_supported_type(key).map_err(error)?;
    let spki = signer
        .public_key()
        .ok_or_else(|| error("the key has no public key"))?
        .as_ref()
        .to_vec();
    let public = PublicKey(spki.clone());
    Ok((
        Arc::new(CertifiedKey::new(vec![CertificateDer::from(spki)], signer)),
        public,
    ))
}

/// The private key in PEM file `path`, for TLS, and its public key. The
/// file must be its owner's alone ([`check_key_permissions`]).
///
/// # Errors
///
/// The file cannot be read, its group or others may access it, or it
/// holds no usable key.
pub fn load_key(path: &Path) -> Result<(Arc<CertifiedKey>, PublicKey), KeyError> {
    check_key_permissions(path).map_err(error)?;
    let pem = std::fs::read(path).map_err(|e| error(format!("{}: {e}", path.display())))?;
    load_key_pem(&pem).map_err(|e| error(format!("{}: {e}", path.display())))
}

/// [`load_key`] from the PEM's bytes, checking no file.
///
/// # Errors
///
/// The bytes hold no usable key.
pub fn load_key_pem(pem: &[u8]) -> Result<(Arc<CertifiedKey>, PublicKey), KeyError> {
    let key = PrivateKeyDer::from_pem_slice(pem).map_err(error)?;
    certify(&key)
}

/// The public key of the private key in PEM file `path`, checked as
/// [`load_key`] checks it.
///
/// # Errors
///
/// As [`load_key`].
pub fn public_key_of(path: &Path) -> Result<PublicKey, KeyError> {
    Ok(load_key(path)?.1)
}

#[cfg(test)]
#[path = "key_tests.rs"]
mod tests;
