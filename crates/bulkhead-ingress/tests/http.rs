//! The driver serving HTTP as the node runs it: requests to an app's
//! routes become calls to its pool, through `bulkhead-sdk`'s `http`
//! module and the SDK fixture. Moved from `bulkhead-host`'s in-node
//! ingress tests (ROADMAP DA2), asserting the same.

#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bulkhead_host::routes::HttpRoute;
use common::{Got, Listen, Served, request, route, until};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Shared {
    /// The driver and apps run here, outliving every test's own runtime.
    _tokio: tokio::runtime::Runtime,
    s: Served,
    /// Listeners: `main`; `tight` (300 ms for headers and body,
    /// 1,000-byte replies); `capped` (one connection at a time);
    /// `partnered` (redirecting to the partner, where `ghost` and `web`
    /// run; `phantom` runs nowhere); `forwarding` (to the partner, node 9,
    /// which cannot be reached); `tls` (certificates for `web.example`,
    /// the default, `*.shop.example`, and `rot.example`, which a test
    /// replaces; its files checked each second).
    web_cert: Vec<u8>,
    shop_cert: Vec<u8>,
    certs: tempfile::TempDir,
}

/// One runtime, the driver, `web` (a pool of three routed by `key`,
/// accepting messages from no app) and `late` (no routes until a test
/// gives it some), shared by every test.
fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(|| {
        // Built on its own thread: a test's runtime cannot block on another.
        std::thread::spawn(start).join().unwrap()
    })
}

fn start() -> Shared {
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let certs = tempfile::tempdir().unwrap();
    let (web_cert, web_entry) = cert(certs.path(), "web", &["web.example"]);
    let (shop_cert, shop_entry) = cert(certs.path(), "shop", &["*.shop.example"]);
    let (_, rot_entry) = cert(certs.path(), "rot", &["rot.example"]);
    let web = json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "instances": { "count": 3, "routing": "hash:key" },
        "supervision": { "intensity": 100, "backoff": { "initial_ms": 1, "max_ms": 5 } },
        "limits": { "callback_deadline_ms": 2000 },
        "messaging_acl": { "accept_from": ["nobody"] },
        "capabilities": { "bulkhead:http-routes": {
            "routes": [
                { "path_prefix": "/api", "hash_header": "x-tenant" },
                { "host": "shop.example", "path_prefix": "/shop" }
            ],
            "max_body": 64,
            "timeout_ms": 300
        } }
    });
    let late = json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "limits": { "callback_deadline_ms": 2000 },
    });
    let listeners = vec![
        Listen::plain("main"),
        Listen::with(
            "tight",
            json!({ "header_timeout_ms": 300, "body_timeout_ms": 300, "max_response": 1000 }),
        ),
        Listen::with("capped", json!({ "max_connections": 1 })),
        Listen::with(
            "partnered",
            json!({ "standby": { "redirect": { "partner_url": "https://partner.example:8443" } } }),
        ),
        Listen::with("forwarding", json!({ "standby": "forward" })),
        Listen {
            name: "tls",
            // Two HTTP/2 streams at once (ROADMAP DA2b).
            args: json!({ "tls": [web_entry.0, shop_entry.0, rot_entry.0], "tls_reload_s": 1, "max_streams": 2 }),
            files: [web_entry.1, shop_entry.1, rot_entry.1].concat(),
        },
    ];
    let s = tokio.block_on(async {
        // `web` too: an app that runs here is served here.
        let s = common::serve(listeners, |app| match app {
            "ghost" | "web" => Some(9),
            _ => None,
        })
        .await;
        s.start_app("web", &web).await;
        s.start_app("late", &late).await;
        let manifest = common::manifest("web", &web);
        s.routes
            .set("web", HttpRoute::from_manifest(&manifest))
            .unwrap();
        s.routes.set("ghost", vec![route("/ghost")]).unwrap();
        s.routes.set("phantom", vec![route("/phantom")]).unwrap();
        // The driver learns the table from an event: wait until it has
        // the last route, or a test that runs first finds none (404).
        let main = s.address("main");
        until("the driver has the routes", || async {
            request(main, "GET", "/phantom", &[], "").await.status != 404
        })
        .await;
        s
    });
    Shared {
        _tokio: tokio,
        s,
        web_cert,
        shop_cert,
        certs,
    }
}

/// A self-signed certificate for `hosts`, written to `dir`; its DER, and
/// its init arguments for the driver with the files to give it.
fn cert(
    dir: &Path,
    name: &str,
    hosts: &[&str],
) -> (Vec<u8>, (serde_json::Value, Vec<std::path::PathBuf>)) {
    let der = write_cert(dir, name, hosts);
    let args = json!({
        "cert": format!("/driver/{name}.pem"),
        "key": format!("/driver/{name}.key"),
        "hosts": hosts,
    });
    let files = vec![
        dir.join(format!("{name}.pem")),
        dir.join(format!("{name}.key")),
    ];
    (der, (args, files))
}

/// Write a new self-signed certificate for `hosts` to `<name>.pem` and
/// `<name>.key` in `dir`; its DER.
fn write_cert(dir: &Path, name: &str, hosts: &[&str]) -> Vec<u8> {
    let made = rcgen::generate_simple_self_signed(
        hosts.iter().map(ToString::to_string).collect::<Vec<_>>(),
    )
    .unwrap();
    std::fs::write(dir.join(format!("{name}.pem")), made.cert.pem()).unwrap();
    std::fs::write(
        dir.join(format!("{name}.key")),
        made.signing_key.serialize_pem(),
    )
    .unwrap();
    made.cert.der().to_vec()
}

fn at(listener: &str) -> SocketAddr {
    shared().s.address(listener)
}

async fn get(path: &str, headers: &[(&str, &str)]) -> Got {
    request(at("main"), "GET", path, headers, "").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn request____on_a_route____answered_by_the_app() {
    let got = request(
        at("main"),
        "POST",
        "/api/orders?y=1",
        &[("x-tenant", "t1")],
        "hi",
    )
    .await;

    assert_eq!(got.status, 200);
    assert!(
        got.body
            .starts_with("POST /api/orders?y=1 key=t1 body=hi scheme=http pid="),
        "{}",
        got.body
    );
    assert!(got.head.contains("x-fixture: sdk"), "{}", got.head);
}

/// `web` accepts messages from no app, and the driver reaches it all the
/// same: an app owning routes accepts HTTP requests (ROADMAP DA2).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accept_from____routed_app_accepting_no_app____served() {
    let got = get("/api/plain", &[]).await;

    assert_eq!((got.status, got.body.as_str()), (200, "plain"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hash_header____same_tenant____same_instance() {
    let pid = |g: Got| g.body.rsplit("pid=").next().unwrap().to_string();

    let mut seen = Vec::new();
    for _ in 0..5 {
        seen.push(pid(get("/api/x", &[("x-tenant", "acme")]).await));
    }

    assert!(seen.windows(2).all(|w| w[0] == w[1]), "{seen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn routes____host_and_prefix____matched_on_segments() {
    let named = get("/shop/cart", &[("host", "Shop.Example:8080")]).await;
    let other_host = get("/shop/cart", &[("host", "elsewhere")]).await;
    let not_segment = get("/apix", &[]).await;

    assert_eq!(named.status, 200);
    assert_eq!(other_host.status, 404);
    assert_eq!(not_segment.status, 404);
}

/// The node changes the table while the driver serves: the driver follows
/// it (the `routes` event) and may call exactly its owners.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn routes____added_then_removed_while_serving____served_then_404() {
    let s = &shared().s;
    let before = get("/late/plain", &[]).await;

    s.routes.set("late", vec![route("/late")]).unwrap();
    until("the route is served", || async {
        get("/late/plain", &[]).await.status == 200
    })
    .await;
    let served = get("/late/plain", &[]).await;
    s.routes.remove("late");
    until("the route is gone", || async {
        get("/late/plain", &[]).await.status == 404
    })
    .await;

    assert_eq!(before.status, 404);
    assert_eq!((served.status, served.body.as_str()), (200, "plain"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replies____plain_status_and_failures____mapped_to_http() {
    let plain = get("/api/plain", &[]).await;
    let teapot = get("/api/status/418", &[]).await;
    let never = get("/api/never", &[]).await;
    let crash = get("/api/crash", &[]).await;
    let big = request(at("main"), "POST", "/api/x", &[], &"x".repeat(65)).await;
    let ghost = get("/ghost", &[]).await;

    assert_eq!((plain.status, plain.body.as_str()), (200, "plain"));
    assert!(
        plain.head.contains("content-type: text/plain"),
        "{}",
        plain.head
    );
    assert_eq!(teapot.status, 418);
    assert_eq!(never.status, 504);
    assert_eq!(crash.status, 502);
    assert_eq!(big.status, 413);
    assert_eq!(ghost.status, 503);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn standby_redirect____app_runs_on_the_partner____307_to_the_same_path() {
    let s = &shared().s;

    let moved = request(at("partnered"), "POST", "/ghost/a?b=1", &[], "hi").await;
    let refused = get("/ghost/a", &[]).await;
    let nowhere = request(at("partnered"), "GET", "/phantom", &[], "").await;
    let here = request(at("partnered"), "GET", "/api/plain", &[], "").await;

    assert_eq!(moved.status, 307);
    assert!(
        moved
            .head
            .contains("location: https://partner.example:8443/ghost/a?b=1"),
        "{}",
        moved.head
    );
    assert_eq!(refused.status, 503);
    assert_eq!(nowhere.status, 503);
    assert_eq!((here.status, here.body.as_str()), (200, "plain"));
    until("the redirect counted", || async {
        s.counter("redirected_total") >= 1
    })
    .await;
}

/// The forwarded reply itself needs a partner node: `board.rs` in the
/// node's tests forwards between a pair.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn standby_forward____partner_unreachable____503() {
    let unreachable = request(at("forwarding"), "GET", "/ghost", &[], "").await;
    let nowhere = request(at("forwarding"), "GET", "/phantom", &[], "").await;
    let here = request(at("forwarding"), "GET", "/api/plain", &[], "").await;

    assert_eq!(unreachable.status, 503);
    assert_eq!(nowhere.status, 503);
    assert_eq!((here.status, here.body.as_str()), (200, "plain"));
}

/// Read until the server closes or `limit` passes; what arrived.
async fn read_until_closed(stream: &mut tokio::net::TcpStream, limit: Duration) -> Option<String> {
    let mut raw = Vec::new();
    match tokio::time::timeout(limit, stream.read_to_end(&mut raw)).await {
        Ok(_) => Some(String::from_utf8_lossy(&raw).into_owned()),
        Err(_) => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_client____headers_or_body_late____dropped_or_408() {
    let mut silent = tokio::net::TcpStream::connect(at("tight")).await.unwrap();
    let mut slow_body = tokio::net::TcpStream::connect(at("tight")).await.unwrap();
    slow_body
        .write_all(b"POST /api/x HTTP/1.1\r\nhost: localhost\r\ncontent-length: 10\r\n\r\nab")
        .await
        .unwrap();
    let started = Instant::now();

    let silent_end = read_until_closed(&mut silent, Duration::from_secs(5)).await;
    let body_end = read_until_closed(&mut slow_body, Duration::from_secs(5)).await;

    assert!(silent_end.is_some(), "an idle connection stayed open");
    assert!(started.elapsed() < Duration::from_secs(5));
    let body_end = body_end.expect("a slow body held the connection");
    assert!(body_end.starts_with("HTTP/1.1 408"), "{body_end}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_response____reply_over_it____502() {
    let small = request(at("tight"), "GET", "/api/big/10", &[], "").await;
    let big = request(at("tight"), "GET", "/api/big/2000", &[], "").await;

    assert_eq!((small.status, small.body.len()), (200, 10));
    assert_eq!(big.status, 502);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_connections____over_it____closed_on_accept() {
    let s = &shared().s;
    let _held = tokio::net::TcpStream::connect(at("capped")).await.unwrap();
    // Let the driver take the first connection.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut second = tokio::net::TcpStream::connect(at("capped")).await.unwrap();

    let ended = read_until_closed(&mut second, Duration::from_secs(2)).await;

    assert_eq!(ended.as_deref(), Some(""));
    until("the refusal counted", || async {
        s.counter("refused_connections_total") >= 1
    })
    .await;
}

/// One HTTPS request to `listener` with SNI `server_name`, trusting
/// `trusted`: (the certificate served, status line, body).
async fn https(
    listener: &str,
    trusted: &[&Vec<u8>],
    server_name: &str,
    path: &str,
) -> (Vec<u8>, String, String) {
    let mut roots = rustls::RootCertStore::empty();
    for der in trusted {
        roots
            .add(rustls_pki_types::CertificateDer::from((*der).clone()))
            .unwrap();
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(at(listener)).await.unwrap();
    let name = rustls_pki_types::ServerName::try_from(server_name.to_string()).unwrap();
    let mut stream = connector.connect(name, tcp).await.unwrap();
    let served_cert = stream.get_ref().1.peer_certificates().unwrap()[0].to_vec();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nhost: {server_name}\r\nconnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw).await;
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    (
        served_cert,
        head.lines().next().unwrap().to_string(),
        body.to_string(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tls____sni_names____matching_certificate_and_https_scheme() {
    let sh = shared();
    let both = [&sh.web_cert, &sh.shop_cert];

    let (web, status, body) = https("tls", &both, "web.example", "/api/x").await;
    let (shop, _, _) = https("tls", &both, "eu.shop.example", "/api/x").await;

    assert_eq!(web, sh.web_cert);
    assert_eq!(shop, sh.shop_cert);
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert!(body.contains("scheme=https"), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tls____plain_http_on_the_tls_port____no_response() {
    let mut stream = tokio::net::TcpStream::connect(at("tls")).await.unwrap();
    stream
        .write_all(b"GET /api/x HTTP/1.1\r\nhost: web.example\r\n\r\n")
        .await
        .unwrap();

    let ended = read_until_closed(&mut stream, Duration::from_secs(5)).await;

    let ended = ended.expect("the connection stayed open");
    assert!(!ended.contains("HTTP/1.1 200"), "{ended}");
}

/// `tls_reload_s`: the driver reads changed files on its timer, without
/// the node asking (the node's reload is `bulkhead reload-tls`, in the
/// node's `http.rs`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tls_reload____files_replaced____new_certificate_served_on_the_timer() {
    let sh = shared();
    let first = https_cert("tls").await;

    let replaced = write_cert(sh.certs.path(), "rot", &["rot.example"]);
    until("the new certificate is served", || async {
        https_cert("tls").await == replaced
    })
    .await;

    assert_ne!(first, replaced);
}

/// The certificate `listener` serves for `rot.example`, whatever it is.
async fn https_cert(listener: &str) -> Vec<u8> {
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(AnyCert))
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(at(listener)).await.unwrap();
    let name = rustls_pki_types::ServerName::try_from("rot.example").unwrap();
    let stream = connector.connect(name, tcp).await.unwrap();
    stream.get_ref().1.peer_certificates().unwrap()[0].to_vec()
}

/// Accepts any certificate: the test compares what was served itself.
#[derive(Debug)]
struct AnyCert;

impl rustls::client::danger::ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _: &rustls_pki_types::CertificateDer<'_>,
        _: &[rustls_pki_types::CertificateDer<'_>],
        _: &rustls_pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls_pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// A client's sampled `traceparent` (ROADMAP O1): the app's call carries a
/// new span of its trace; an unsampled one, or none, starts no trace (the
/// sample share is 0 by default).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn traceparent____sampled_from_the_client____the_apps_call_continues_it() {
    let client = "00-0102030405060708090a0b0c0d0e0f10-1112131415161718-01";
    let unsampled = "00-0102030405060708090a0b0c0d0e0f10-1112131415161718-00";

    let sampled = get("/api/trace", &[("traceparent", client)]).await;
    let not = get("/api/trace", &[("traceparent", unsampled)]).await;
    let none = get("/api/trace", &[]).await;

    assert_eq!(sampled.status, 200);
    assert!(
        sampled
            .body
            .starts_with("00-0102030405060708090a0b0c0d0e0f10-"),
        "{}",
        sampled.body
    );
    assert_ne!(sampled.body, client, "a span of the driver's own");
    assert_eq!(not.body, "-");
    assert_eq!(none.body, "-");
}

type H2Sender = hyper::client::conn::http2::SendRequest<http_body_util::Full<bytes::Bytes>>;

/// A TLS connection to `listener` offering `h2` and `http/1.1` by ALPN,
/// trusting `trusted`, with SNI `server_name`: the protocol the driver
/// chose, and with HTTP/2 a sender of requests on that one connection.
async fn h2(
    listener: &str,
    trusted: &[&Vec<u8>],
    server_name: &str,
) -> (Vec<u8>, Option<H2Sender>) {
    let mut roots = rustls::RootCertStore::empty();
    for der in trusted {
        roots
            .add(rustls_pki_types::CertificateDer::from((*der).clone()))
            .unwrap();
    }
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(at(listener)).await.unwrap();
    let name = rustls_pki_types::ServerName::try_from(server_name.to_string()).unwrap();
    let stream = connector.connect(name, tcp).await.unwrap();
    let chosen = stream
        .get_ref()
        .1
        .alpn_protocol()
        .unwrap_or_default()
        .to_vec();
    if chosen != b"h2" {
        return (chosen, None);
    }
    let (sender, conn) = hyper::client::conn::http2::handshake(
        hyper_util::rt::TokioExecutor::new(),
        hyper_util::rt::TokioIo::new(stream),
    )
    .await
    .unwrap();
    tokio::spawn(conn);
    (chosen, Some(sender))
}

/// One request on an HTTP/2 connection: status and body.
async fn h2_request(sender: &mut H2Sender, method: &str, uri: &str, body: &str) -> (u16, String) {
    use http_body_util::BodyExt;
    let req = hyper::Request::builder()
        .method(method)
        .uri(uri)
        .header("x-tenant", "t1")
        .body(http_body_util::Full::new(bytes::Bytes::from(
            body.to_string(),
        )))
        .unwrap();
    sender.ready().await.unwrap();
    let res = sender.send_request(req).await.unwrap();
    let status = res.status().as_u16();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// HTTP/2 by ALPN on a TLS listener (ROADMAP DA2b): many requests on one
/// connection, each a call to the app as over HTTP/1.1, the body limit as
/// before, and the host from `:authority`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn h2____over_tls____requests_on_one_connection_routed_and_limited() {
    let sh = shared();
    let both = [&sh.web_cert, &sh.shop_cert];

    let (chosen, sender) = h2("tls", &both, "web.example").await;
    let mut sender = sender.expect("HTTP/2 chosen");
    let mut answers = Vec::new();
    for i in 0..10 {
        answers.push(
            h2_request(
                &mut sender,
                "GET",
                &format!("https://web.example/api/x?n={i}"),
                "",
            )
            .await,
        );
    }
    let too_big = h2_request(
        &mut sender,
        "POST",
        "https://web.example/api/x",
        &"x".repeat(65),
    )
    .await;
    let by_authority = h2_request(&mut sender, "GET", "https://shop.example/shop/y", "").await;
    let other_host = h2_request(&mut sender, "GET", "https://other.example/shop/y", "").await;

    assert_eq!(chosen, b"h2");
    for (i, (status, body)) in answers.iter().enumerate() {
        assert_eq!(*status, 200, "{body}");
        assert!(
            body.starts_with(&format!("GET /api/x?n={i} key=t1")),
            "{body}"
        );
        assert!(body.contains("scheme=https"), "{body}");
    }
    assert_eq!(too_big.0, 413, "{}", too_big.1);
    assert_eq!(by_authority.0, 200, "{}", by_authority.1);
    assert_eq!(other_host.0, 404);
}

/// More streams at once than `max_streams` wait their turn once the client
/// has the server's settings: all answered, none refused. (Streams a client
/// opens before the settings arrive may be refused with `REFUSED_STREAM`,
/// which HTTP/2 says to retry.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn h2____streams_over_max_streams____held_back_all_answered() {
    let sh = shared();

    let (_, sender) = h2("tls", &[&sh.web_cert], "web.example").await;
    let mut sender = sender.expect("HTTP/2 chosen");
    let first = h2_request(&mut sender, "GET", "https://web.example/api/x", "").await;
    let calls = (0..8).map(|i| {
        let mut sender = sender.clone();
        tokio::spawn(async move {
            h2_request(
                &mut sender,
                "GET",
                &format!("https://web.example/api/x?n={i}"),
                "",
            )
            .await
        })
    });
    let mut statuses = Vec::new();
    for c in calls.collect::<Vec<_>>() {
        statuses.push(c.await.unwrap().0);
    }

    assert_eq!(first.0, 200);
    assert_eq!(statuses, vec![200; 8]);
}
