//! TLS 1.3 with raw public keys (RFC 7250): each side proves it holds its
//! private key, and the other accepts it only if the key is listed. No
//! certificates, nothing expires.

use std::path::Path;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::ring::default_provider;
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls13_signature_with_raw_key};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::sign::CertifiedKey;
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use rustls_pki_types::{CertificateDer, ServerName, SubjectPublicKeyInfoDer, UnixTime};

use crate::key::{KeyError, PublicKey, load_key};

/// Which public keys a TLS endpoint accepts from the other side. Asked on
/// every handshake, so it may follow a file that is reloaded.
pub type Accepts = Arc<dyn Fn(&PublicKey) -> bool + Send + Sync>;

/// Accepts the other side's raw public key if `accepts` does.
pub struct PinnedKeys {
    accepts: Accepts,
    algorithms: WebPkiSupportedAlgorithms,
}

impl std::fmt::Debug for PinnedKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedKeys").finish_non_exhaustive()
    }
}

impl PinnedKeys {
    /// A verifier accepting the keys `accepts` does.
    #[must_use]
    pub fn accepting(accepts: Accepts) -> Self {
        Self {
            accepts,
            algorithms: default_provider().signature_verification_algorithms,
        }
    }

    fn check(&self, end_entity: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        if (self.accepts)(&PublicKey::from_der(end_entity.as_ref())) {
            Ok(())
        } else {
            Err(rustls::Error::General(
                "the other side's key is not listed".into(),
            ))
        }
    }

    fn signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature_with_raw_key(
            message,
            &SubjectPublicKeyInfoDer::from(cert.as_ref()),
            dss,
            &self.algorithms,
        )
    }
}

impl ServerCertVerifier for PinnedKeys {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|()| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

impl ClientCertVerifier for PinnedKeys {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|()| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

/// TLS for a node's remote admin API: TLS 1.3 with raw public keys,
/// presenting `certified` (the node's admin key) and accepting a client
/// only if `accepts` its key (the operators file).
///
/// # Errors
///
/// TLS 1.3 is not available from the provider (never, with ring).
pub fn admin_server_tls(
    certified: Arc<CertifiedKey>,
    accepts: Accepts,
) -> Result<rustls::ServerConfig, KeyError> {
    let pinned = Arc::new(PinnedKeys::accepting(accepts));
    Ok(
        rustls::ServerConfig::builder_with_provider(Arc::new(default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| KeyError(e.to_string()))?
            .with_client_cert_verifier(pinned)
            .with_cert_resolver(Arc::new(
                rustls::server::AlwaysResolvesServerRawPublicKeys::new(certified),
            )),
    )
}

/// TLS for an operator reaching a node's remote admin API: presents the key
/// in `key`, and accepts the node only if it presents `node`.
///
/// # Errors
///
/// `key` cannot be read or used ([`load_key`]).
pub fn admin_client_tls(key: &Path, node: PublicKey) -> Result<rustls::ClientConfig, KeyError> {
    let (certified, _) = load_key(key)?;
    let pinned = Arc::new(PinnedKeys::accepting(Arc::new(move |k: &PublicKey| {
        *k == node
    })));
    Ok(
        rustls::ClientConfig::builder_with_provider(Arc::new(default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| KeyError(e.to_string()))?
            .dangerous()
            .with_custom_certificate_verifier(pinned)
            .with_client_cert_resolver(Arc::new(
                rustls::client::AlwaysResolvesClientRawPublicKeys::new(certified),
            )),
    )
}

/// The raw public key the other side of a TLS connection presented, from
/// its peer certificates.
#[must_use]
pub fn presented_key(certificates: Option<&[CertificateDer<'_>]>) -> Option<PublicKey> {
    certificates?
        .first()
        .map(|k| PublicKey::from_der(k.as_ref()))
}
