//! TLS for a listener: rustls on ring, certificates from PEM files, chosen
//! by the client's SNI host name, and reloaded from their files without a
//! restart (the node's ingress's, moved with it, ROADMAP DA2).
//!
//! Each certificate lists the hosts it serves; a host matches exactly or
//! through one leading wildcard label (`*.example.com` matches
//! `a.example.com`). The first certificate also answers clients that send
//! no SNI or a name no certificate lists. A reload reads every file again
//! and swaps the set in one step; if any file fails, the old set stays and
//! the error is returned. Files are read through [`Files`]: the driver's
//! own under `/driver/` ([`SdkFiles`]), which on FreeBSD are opened under
//! the preopen's descriptor.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::SystemTime;

use rustls::ServerConfig;
use rustls::crypto::ring::{default_provider, sign::any_supported_type};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

use crate::config::Certificate;

/// Where certificate files are read from.
pub trait Files: Send + Sync + 'static {
    /// The whole file at `path`.
    ///
    /// # Errors
    ///
    /// The file cannot be opened or read.
    fn read(&self, path: &str) -> io::Result<Vec<u8>>;

    /// The file's modification time, if it can be read.
    fn modified(&self, path: &str) -> Option<SystemTime>;
}

/// The driver's files, by guest path (`bulkhead_sdk::files`).
#[derive(Debug, Clone, Copy, Default)]
pub struct SdkFiles;

impl Files for SdkFiles {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        bulkhead_sdk::files::read(path)
    }

    fn modified(&self, path: &str) -> Option<SystemTime> {
        bulkhead_sdk::files::open(path, bulkhead_sdk::files::Mode::Read)
            .and_then(|f| f.metadata())
            .and_then(|m| m.modified())
            .ok()
    }
}

/// The certificates as last loaded.
#[derive(Debug)]
struct Loaded {
    default: Arc<CertifiedKey>,
    by_host: HashMap<String, Arc<CertifiedKey>>,
}

/// A listener's certificates. Cheap to clone.
#[derive(Clone)]
pub struct IngressTls {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for IngressTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IngressTls")
            .field("entries", &self.inner.entries)
            .finish_non_exhaustive()
    }
}

struct Inner {
    entries: Vec<Certificate>,
    files: Box<dyn Files>,
    /// Shared with the resolver rustls asks during each handshake.
    current: Arc<RwLock<Arc<Loaded>>>,
    /// The files' modification times at the last load.
    stamps: Mutex<Vec<Option<SystemTime>>>,
    acceptor: TlsAcceptor,
}

#[derive(Debug)]
struct Resolver(Arc<RwLock<Arc<Loaded>>>);

fn load_key(files: &dyn Files, entry: &Certificate) -> Result<Arc<CertifiedKey>, String> {
    let read = |path: &str| files.read(path).map_err(|e| format!("{path}: {e}"));
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&read(&entry.cert)?)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {e}", entry.cert))?;
    if chain.is_empty() {
        return Err(format!("{}: no certificate", entry.cert));
    }
    let key = PrivateKeyDer::from_pem_slice(&read(&entry.key)?)
        .map_err(|e| format!("{}: {e}", entry.key))?;
    let signer = any_supported_type(&key).map_err(|e| format!("{}: {e}", entry.key))?;
    let certified = CertifiedKey::new(chain, signer);
    certified
        .keys_match()
        .map_err(|e| format!("{} and {}: {e}", entry.cert, entry.key))?;
    Ok(Arc::new(certified))
}

fn load(files: &dyn Files, entries: &[Certificate]) -> Result<Loaded, String> {
    let mut by_host = HashMap::new();
    let mut default = None;
    for entry in entries {
        let key = load_key(files, entry)?;
        for host in &entry.hosts {
            by_host.insert(host.to_ascii_lowercase(), key.clone());
        }
        default.get_or_insert(key);
    }
    let default = default.ok_or("no certificates")?;
    Ok(Loaded { default, by_host })
}

fn stamps(files: &dyn Files, entries: &[Certificate]) -> Vec<Option<SystemTime>> {
    entries
        .iter()
        .flat_map(|e| [&e.cert, &e.key])
        .map(|p| files.modified(p))
        .collect()
}

impl Loaded {
    fn pick(&self, sni: Option<&str>) -> Arc<CertifiedKey> {
        let Some(name) = sni.map(str::to_ascii_lowercase) else {
            return self.default.clone();
        };
        if let Some(k) = self.by_host.get(&name) {
            return k.clone();
        }
        name.split_once('.')
            .and_then(|(_, parent)| self.by_host.get(&format!("*.{parent}")))
            .cloned()
            .unwrap_or_else(|| self.default.clone())
    }
}

impl ResolvesServerCert for Resolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let loaded = self
            .0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        Some(loaded.pick(hello.server_name()))
    }
}

impl IngressTls {
    /// Load `entries` from `files` (the first is the default).
    ///
    /// # Errors
    ///
    /// No entries, a file that cannot be read or parsed, or a key that
    /// does not match its certificate.
    pub fn load(entries: Vec<Certificate>, files: impl Files) -> Result<Self, String> {
        let current = Arc::new(RwLock::new(Arc::new(load(&files, &entries)?)));
        let mut config = ServerConfig::builder_with_provider(Arc::new(default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Resolver(current.clone())));
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self {
            inner: Arc::new(Inner {
                stamps: Mutex::new(stamps(&files, &entries)),
                entries,
                files: Box::new(files),
                current,
                acceptor: TlsAcceptor::from(Arc::new(config)),
            }),
        })
    }

    /// Read every file again and serve the new set; on any error keep the
    /// old one.
    ///
    /// # Errors
    ///
    /// As for [`IngressTls::load`].
    pub fn reload(&self) -> Result<(), String> {
        let files = self.inner.files.as_ref();
        let loaded = load(files, &self.inner.entries)?;
        *self
            .inner
            .current
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(loaded);
        *self
            .inner
            .stamps
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = stamps(files, &self.inner.entries);
        Ok(())
    }

    /// Reload if any file's modification time changed since the last
    /// load. Returns whether it reloaded.
    ///
    /// # Errors
    ///
    /// As for [`IngressTls::reload`]; a failed reload is tried again next
    /// time.
    pub fn reload_if_changed(&self) -> Result<bool, String> {
        let now = stamps(self.inner.files.as_ref(), &self.inner.entries);
        let changed = *self
            .inner
            .stamps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            != now;
        if changed {
            self.reload()?;
        }
        Ok(changed)
    }

    /// The acceptor that runs handshakes with the current certificates.
    #[must_use]
    pub fn acceptor(&self) -> &TlsAcceptor {
        &self.inner.acceptor
    }
}

#[cfg(test)]
#[path = "tls_tests.rs"]
mod tls_tests;
