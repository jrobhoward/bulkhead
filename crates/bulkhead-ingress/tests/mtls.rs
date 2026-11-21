//! Client certificates on an HTTPS listener (ROADMAP DA2b): the driver
//! asks for one from the listener's CAs, verifies it, and gives the app
//! its fingerprint. A harness of its own, the driver taking at most six
//! listeners: `mtls` requires a certificate, `mtls-opt` asks for one.

#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::{Arc, OnceLock};

use bulkhead_host::routes::HttpRoute;
use common::{Listen, Served};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A certificate and its key, DER.
type Pair = (Vec<u8>, Vec<u8>);

struct Shared {
    _tokio: tokio::runtime::Runtime,
    s: Served,
    /// The server's certificate, for `web.example`.
    web_cert: Vec<u8>,
    /// A client certificate the CA signed, and a self-signed one.
    client: Pair,
    stranger: Pair,
    _certs: tempfile::TempDir,
}

fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(|| std::thread::spawn(start).join().unwrap())
}

fn start() -> Shared {
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let certs = tempfile::tempdir().unwrap();
    let dir = certs.path();
    let made = rcgen::generate_simple_self_signed(vec!["web.example".to_string()]).unwrap();
    std::fs::write(dir.join("web.pem"), made.cert.pem()).unwrap();
    std::fs::write(dir.join("web.key"), made.signing_key.serialize_pem()).unwrap();
    let (client, stranger) = client_certs(dir);
    let files = vec![dir.join("web.pem"), dir.join("web.key"), dir.join("ca.pem")];
    let entry =
        json!({ "cert": "/driver/web.pem", "key": "/driver/web.key", "hosts": ["web.example"] });
    let listeners = vec![
        Listen {
            name: "mtls",
            args: json!({ "tls": [entry], "client_auth": { "ca": "/driver/ca.pem" } }),
            files: files.clone(),
        },
        Listen {
            name: "mtls-opt",
            args: json!({
                "tls": [entry],
                "client_auth": { "ca": "/driver/ca.pem", "required": false },
            }),
            files,
        },
    ];
    let web = json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "limits": { "callback_deadline_ms": 2000 },
        "capabilities": { "bulkhead:http-routes": { "routes": [{ "path_prefix": "/api" }] } },
    });
    let s = tokio.block_on(async {
        let s = common::serve(listeners, |_| None).await;
        s.start_app("web", &web).await;
        let manifest = common::manifest("web", &web);
        s.routes
            .set("web", HttpRoute::from_manifest(&manifest))
            .unwrap();
        s
    });
    Shared {
        _tokio: tokio,
        s,
        web_cert: made.cert.der().to_vec(),
        client,
        stranger,
        _certs: certs,
    }
}

/// A CA written to `ca.pem` in `dir`, a client certificate it signed, and
/// a self-signed one.
fn client_certs(dir: &Path) -> (Pair, Pair) {
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca =
        rcgen::CertifiedIssuer::self_signed(params, rcgen::KeyPair::generate().unwrap()).unwrap();
    std::fs::write(dir.join("ca.pem"), ca.pem()).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let signed = rcgen::CertificateParams::new(vec!["client.example".to_string()])
        .unwrap()
        .signed_by(&key, &ca)
        .unwrap();
    let stranger = rcgen::generate_simple_self_signed(vec!["client.example".to_string()]).unwrap();
    (
        (signed.der().to_vec(), key.serialize_der()),
        (
            stranger.cert.der().to_vec(),
            stranger.signing_key.serialize_der(),
        ),
    )
}

/// One HTTPS GET of `/api/client` to `listener`, offering `client` if
/// given: the status line and body, or `None` if the connection failed or
/// ended without an answer.
async fn https_as(listener: &str, client: Option<&Pair>) -> Option<(String, String)> {
    let sh = shared();
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls_pki_types::CertificateDer::from(sh.web_cert.clone()))
        .unwrap();
    let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots);
    let config = match client {
        None => builder.with_no_client_auth(),
        Some((cert, key)) => builder
            .with_client_auth_cert(
                vec![rustls_pki_types::CertificateDer::from(cert.clone())],
                rustls_pki_types::PrivateKeyDer::Pkcs8(key.clone().into()),
            )
            .unwrap(),
    };
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(sh.s.address(listener))
        .await
        .unwrap();
    let name = rustls_pki_types::ServerName::try_from("web.example").unwrap();
    // Under TLS 1.3 a refused client certificate shows after the
    // handshake, at the first read.
    let mut stream = connector.connect(name, tcp).await.ok()?;
    stream
        .write_all(b"GET /api/client HTTP/1.1\r\nhost: web.example\r\nconnection: close\r\n\r\n")
        .await
        .ok()?;
    let mut raw = Vec::new();
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        stream.read_to_end(&mut raw),
    )
    .await;
    read.ok()?.ok()?;
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = raw.split_once("\r\n\r\n")?;
    Some((head.lines().next()?.to_string(), body.to_string()))
}

fn sha256_hex(der: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, der)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mtls____certificate_from_the_ca____app_gets_its_fingerprint() {
    let sh = shared();

    let got = https_as("mtls", Some(&sh.client)).await;

    let (status, body) = got.expect("no answer");
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert_eq!(body, sha256_hex(&sh.client.0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mtls____required_and_none_or_another_ca____not_served() {
    let sh = shared();

    let none = https_as("mtls", None).await;
    let stranger = https_as("mtls", Some(&sh.stranger)).await;

    assert_eq!(none, None);
    assert_eq!(stranger, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mtls____optional____served_with_or_without_one() {
    let sh = shared();

    let without = https_as("mtls-opt", None).await;
    let with = https_as("mtls-opt", Some(&sh.client)).await;

    assert_eq!(without.map(|(_, b)| b).as_deref(), Some("-"));
    assert_eq!(with.map(|(_, b)| b), Some(sha256_hex(&sh.client.0)));
}
