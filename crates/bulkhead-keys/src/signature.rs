//! An operator's signature on a remote admin request (ROADMAP DA7).
//!
//! TLS proves the operator's key to whatever terminates it, which since
//! DA7 is the admin driver, not the node. So the operator's CLI also signs
//! each request, and the node checks the signature itself before acting:
//! a compromised driver can watch, delay or drop requests, but cannot
//! forge one, change one, or replay one to this or another node.
//!
//! The header `Bulkhead-Auth: v1 key=<public key> ts=<unix ms>
//! nonce=<32 hex> sig=<base64>` signs, with Ed25519, the text
//!
//! ```text
//! bulkhead-admin-v1
//! <the node's admin public key>
//! <method>
//! <path and query>
//! <SHA-256 of the body, hex>
//! <ts>
//! <nonce>
//! ```
//!
//! The node accepts it if the key is in its operators file, `ts` is
//! within [`SKEW`] of its clock and the nonce is new within
//! [`NONCE_WINDOW`] ([`Nonces`]).

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use rustls_pki_types::PrivateKeyDer;
use rustls_pki_types::pem::PemObject;

use crate::check_key_permissions;
use crate::key::{KeyError, PublicKey};
use crate::operators::operator_of;

/// The request header carrying the signature.
pub const HEADER: &str = "bulkhead-auth";

/// How far a request's time may be from the node's clock.
pub const SKEW: Duration = Duration::from_secs(60);

/// How long a nonce is remembered: a request older than [`SKEW`] is
/// refused for its time, so twice that covers both sides of the clock.
pub const NONCE_WINDOW: Duration = Duration::from_secs(120);

/// The most nonces remembered; past it the oldest go first.
pub const MAX_NONCES: usize = 100_000;

/// The SHA-256 of `body`.
#[must_use]
pub fn body_hash(body: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, body);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

/// A body's SHA-256 as it streams.
pub struct BodyHash(ring::digest::Context);

impl std::fmt::Debug for BodyHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BodyHash").finish_non_exhaustive()
    }
}

impl Default for BodyHash {
    fn default() -> Self {
        Self(ring::digest::Context::new(&ring::digest::SHA256))
    }
}

impl BodyHash {
    /// Add `chunk`.
    pub fn update(&mut self, chunk: &[u8]) {
        self.0.update(chunk);
    }

    /// The hash of everything added.
    #[must_use]
    pub fn finish(self) -> [u8; 32] {
        let mut out = [0u8; 32];
        out.copy_from_slice(self.0.finish().as_ref());
        out
    }
}

/// What is signed of one request.
#[derive(Debug, Clone, Copy)]
pub struct Signed<'a> {
    /// The node's admin public key, as the operator pins it.
    pub node: &'a PublicKey,
    /// `GET`, `POST`, ...
    pub method: &'a str,
    /// The path and query, as sent.
    pub target: &'a str,
    /// The body's SHA-256 ([`body_hash`]).
    pub body: [u8; 32],
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn message(signed: &Signed<'_>, ts: u64, nonce: &str) -> Vec<u8> {
    format!(
        "bulkhead-admin-v1\n{}\n{}\n{}\n{}\n{ts}\n{nonce}",
        signed.node,
        signed.method,
        signed.target,
        hex(&signed.body),
    )
    .into_bytes()
}

/// An operator's key, to sign requests with.
pub struct Signer {
    pair: Ed25519KeyPair,
    public: PublicKey,
    random: SystemRandom,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer")
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

impl Signer {
    /// The Ed25519 key in PEM file `path`, its owner's alone.
    ///
    /// # Errors
    ///
    /// The file cannot be read, others may read it, or it is not an
    /// Ed25519 key.
    pub fn from_file(path: &Path) -> Result<Self, KeyError> {
        check_key_permissions(path).map_err(|e| KeyError(e.to_string()))?;
        let pem = std::fs::read(path).map_err(|e| KeyError(format!("{}: {e}", path.display())))?;
        Self::from_pem(&pem).map_err(|e| KeyError(format!("{}: {e}", path.display())))
    }

    /// The Ed25519 key in `pem` (PKCS#8).
    ///
    /// # Errors
    ///
    /// Not a PKCS#8 Ed25519 key.
    pub fn from_pem(pem: &[u8]) -> Result<Self, KeyError> {
        let key = PrivateKeyDer::from_pem_slice(pem).map_err(|e| KeyError(e.to_string()))?;
        let PrivateKeyDer::Pkcs8(pkcs8) = key else {
            return Err(KeyError("not a PKCS#8 key".into()));
        };
        let pair = Ed25519KeyPair::from_pkcs8_maybe_unchecked(pkcs8.secret_pkcs8_der())
            .map_err(|e| KeyError(format!("not an Ed25519 key: {e}")))?;
        let public = PublicKey::from_ed25519(pair.public_key().as_ref());
        Ok(Self {
            pair,
            public,
            random: SystemRandom::new(),
        })
    }

    /// Its public key.
    #[must_use]
    pub fn public(&self) -> &PublicKey {
        &self.public
    }

    /// The header value signing `signed` at `ts` (unix milliseconds) with
    /// `nonce`.
    #[must_use]
    pub fn header_at(&self, signed: &Signed<'_>, ts: u64, nonce: [u8; 16]) -> String {
        let nonce = hex(&nonce);
        let sig = self.pair.sign(&message(signed, ts, &nonce));
        format!(
            "v1 key={} ts={ts} nonce={nonce} sig={}",
            self.public,
            STANDARD.encode(sig.as_ref())
        )
    }

    /// The header value signing `signed` now, with a fresh nonce.
    ///
    /// # Errors
    ///
    /// The system's random source failed.
    pub fn header(&self, signed: &Signed<'_>) -> Result<String, KeyError> {
        let mut nonce = [0u8; 16];
        self.random
            .fill(&mut nonce)
            .map_err(|_| KeyError("no random nonce".into()))?;
        Ok(self.header_at(signed, now_ms(), nonce))
    }
}

/// Milliseconds since the Unix epoch, now.
#[must_use]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Why a request's signature was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    /// No `Bulkhead-Auth` header.
    #[error("unsigned: remote requests need the operator's signature (Bulkhead-Auth)")]
    Unsigned,
    /// The header is not `v1 key=... ts=... nonce=... sig=...`.
    #[error("malformed signature header: {0}")]
    Malformed(String),
    /// The key is in no operator's list.
    #[error("the key {0} is not an operator's")]
    Unknown(PublicKey),
    /// The time is too far from the node's clock.
    #[error("signed at {signed} ms, {skew_ms} ms from the node's clock (at most {max} s)", max = SKEW.as_secs())]
    Stale {
        /// The request's time.
        signed: u64,
        /// How far it is from the node's.
        skew_ms: u64,
    },
    /// The nonce was used before.
    #[error("replayed: this request's nonce was seen before")]
    Replayed,
    /// The signature does not match the request.
    #[error("the signature does not match the request")]
    BadSignature,
}

/// What a valid signature says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// The operator's name, from the operators file.
    pub operator: String,
    /// The key that signed.
    pub key: PublicKey,
}

/// Nonces seen within [`NONCE_WINDOW`].
#[derive(Debug, Default)]
pub struct Nonces(Mutex<NonceSet>);

#[derive(Debug, Default)]
struct NonceSet {
    order: VecDeque<(u64, [u8; 16])>,
    seen: HashSet<[u8; 16]>,
}

impl Nonces {
    /// Record `nonce` at `now` (ms): false if it was seen within the window.
    fn record(&self, nonce: [u8; 16], now: u64) -> bool {
        let mut set = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let window = u64::try_from(NONCE_WINDOW.as_millis()).unwrap_or(u64::MAX);
        while let Some(&(at, old)) = set.order.front() {
            if now.saturating_sub(at) <= window && set.order.len() < MAX_NONCES {
                break;
            }
            set.order.pop_front();
            set.seen.remove(&old);
        }
        if !set.seen.insert(nonce) {
            return false;
        }
        set.order.push_back((now, nonce));
        true
    }
}

fn field<'a>(header: &'a str, name: &str) -> Result<&'a str, Refused> {
    header
        .split_whitespace()
        .find_map(|part| part.strip_prefix(name)?.strip_prefix('='))
        .ok_or_else(|| Refused::Malformed(format!("no {name}")))
}

fn unhex(text: &str) -> Option<[u8; 16]> {
    if text.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Check `header` (the `Bulkhead-Auth` value, if any) against `signed`,
/// at `now` (unix ms), with `operators` and `nonces`.
///
/// # Errors
///
/// [`Refused`], saying why.
pub fn verify(
    header: Option<&str>,
    signed: &Signed<'_>,
    now: u64,
    operators: &BTreeMap<String, Vec<PublicKey>>,
    nonces: &Nonces,
) -> Result<Verified, Refused> {
    let header = header.ok_or(Refused::Unsigned)?;
    let mut parts = header.split_whitespace();
    if parts.next() != Some("v1") {
        return Err(Refused::Malformed("not v1".into()));
    }
    let key: PublicKey = field(header, "key")?.parse().map_err(Refused::Malformed)?;
    let ts: u64 = field(header, "ts")?
        .parse()
        .map_err(|_| Refused::Malformed("ts".into()))?;
    let nonce_text = field(header, "nonce")?;
    let nonce = unhex(nonce_text).ok_or_else(|| Refused::Malformed("nonce".into()))?;
    let sig = STANDARD
        .decode(field(header, "sig")?)
        .map_err(|_| Refused::Malformed("sig".into()))?;
    let operator = operator_of(operators, &key).ok_or_else(|| Refused::Unknown(key.clone()))?;
    let skew_ms = now.abs_diff(ts);
    if skew_ms > u64::try_from(SKEW.as_millis()).unwrap_or(u64::MAX) {
        return Err(Refused::Stale {
            signed: ts,
            skew_ms,
        });
    }
    let raw = key
        .ed25519()
        .ok_or_else(|| Refused::Malformed("not an Ed25519 key".into()))?;
    UnparsedPublicKey::new(&ED25519, raw)
        .verify(&message(signed, ts, nonce_text), &sig)
        .map_err(|_| Refused::BadSignature)?;
    // Only a valid signature spends its nonce: nobody can use up another's.
    if !nonces.record(nonce, now) {
        return Err(Refused::Replayed);
    }
    Ok(Verified { operator, key })
}

#[cfg(test)]
#[path = "signature_tests.rs"]
mod tests;
