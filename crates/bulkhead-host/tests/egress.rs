//! HTTP egress (`bulkhead:http-egress`) through the SDK fixture's
//! `fetch` command, against a local server: the allow list, methods, the
//! address guard, the per-app limit, timeouts, the pool and TLS, on the
//! fixture built as a native program. The node checks each request and
//! its app's egress driver (`bulkhead-egress`, ROADMAP DA4) sends it, over
//! connections the node opens.

#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use bulkhead_bundle::manifest::{Capability, NativeProgram};
use bulkhead_host::{
    Apps, Egress, InstanceHandle, InstanceSpec, LoadedApp, NativeCode, NativeConfig, Runtime,
    RuntimeConfig,
};
use bytes::Bytes;
use common::ask;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame, Incoming};
use hyper::{Request, Response};

/// A body that sends one chunk and then nothing, ever.
struct Stall(bool);

impl Body for Stall {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let this = self.get_mut();
        if this.0 {
            Poll::Pending
        } else {
            this.0 = true;
            Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"first")))))
        }
    }
}

type Reply = Response<http_body_util::combinators::BoxBody<Bytes, Infallible>>;

async fn answer(req: Request<Incoming>) -> Result<Reply, Infallible> {
    let path = req.uri().path().to_string();
    let method = req.method().to_string();
    let full = |s: String| Full::new(Bytes::from(s)).boxed();
    Ok(match path.as_str() {
        "/hello" => Response::new(full("hi".into())),
        "/echo" => {
            let body = req.into_body().collect().await.unwrap().to_bytes();
            Response::new(full(format!("{method} {}", String::from_utf8_lossy(&body))))
        }
        "/slow" => {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            Response::new(full("late".into()))
        }
        "/stall" => Response::new(Stall(false).boxed()),
        _ => {
            let mut r = Response::new(full("missing".into()));
            *r.status_mut() = hyper::StatusCode::NOT_FOUND;
            r
        }
    })
}

/// A local HTTP server (HTTPS with `tls`); its address and how many
/// connections it accepted.
async fn server(tls: Option<Arc<rustls::ServerConfig>>) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = accepted.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            count.fetch_add(1, Ordering::Relaxed);
            let tls = tls.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(answer);
                let http = hyper::server::conn::http1::Builder::new();
                match tls {
                    Some(config) => {
                        let acceptor = tokio_rustls::TlsAcceptor::from(config);
                        if let Ok(stream) = acceptor.accept(stream).await {
                            let io = hyper_util::rt::TokioIo::new(stream);
                            let _ = http.serve_connection(io, service).await;
                        }
                    }
                    None => {
                        let io = hyper_util::rt::TokioIo::new(stream);
                        let _ = http.serve_connection(io, service).await;
                    }
                }
            });
        }
    });
    (address, accepted)
}

/// A runtime, the SDK fixture, the apps (where drivers run) and the
/// egress driver's program.
struct Node(Runtime, LoadedApp, Apps, LoadedApp);

async fn runtime(egress: Egress) -> Node {
    let mut native = NativeConfig::default();
    native.runner = Some(common::native_fixtures().0.clone());
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        egress,
        native,
        // A node resolves through its driver; here, a table.
        resolver: bulkhead_host::Resolver::Table(
            [
                ("localhost".to_string(), vec!["127.0.0.1".parse().unwrap()]),
                // A name on the internal network (ROADMAP DA4b's deny list).
                (
                    "intranet".to_string(),
                    vec!["10.255.255.1".parse().unwrap()],
                ),
            ]
            .into(),
        ),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load_native(NativeCode {
        path: common::native_sdk().clone(),
        kind: NativeProgram::Executable,
    });
    let driver = rt.load_native(NativeCode {
        path: common::egress_program().clone(),
        kind: NativeProgram::Executable,
    });
    let apps = rt.start_apps(Vec::new()).await.unwrap();
    Node(rt, app, apps, driver)
}

fn spec(app: &str, allow: &[String], granted: bool) -> InstanceSpec {
    let mut s = InstanceSpec {
        callback_deadline: Duration::from_secs(5),
        granted: if granted {
            [Capability::HttpOutgoing].into_iter().collect()
        } else {
            Default::default()
        },
        ..InstanceSpec::new(app, "1.0.0")
    };
    s.services.egress.allow = allow.to_vec();
    s
}

/// An instance of `spec`'s app, and its egress driver, as the node starts
/// it, unless it runs already.
async fn up(rt: &Node, spec: InstanceSpec) -> InstanceHandle {
    let driver = bulkhead_host::egress_driver_app(&spec.app);
    if rt.2.app(&driver).is_none() {
        rt.2.add(common::egress_driver(&rt.0, &spec.app, rt.3.clone()))
            .await
            .unwrap();
    }
    rt.0.spawn(&rt.1, spec).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch____allowed_address____responses_over_one_pooled_connection() {
    let (address, accepted) = server(None).await;
    let rt = runtime(Egress::default()).await;
    let h = up(&rt, spec("egress-ok", &[format!("http://{address}")], true)).await;

    let first = ask(&h, &format!("fetch GET http://{address}/hello")).await;
    let second = ask(&h, &format!("fetch POST http://{address}/echo payload")).await;

    assert_eq!(first, "200 hi");
    assert_eq!(second, "200 POST payload");
    assert_eq!(accepted.load(Ordering::Relaxed), 1, "pooled");
    let metrics = rt.0.render_metrics();
    assert!(
        metrics.contains("bulkhead_egress_requests_total{app=\"egress-ok\",outcome=\"sent\"} 2"),
        "{metrics}"
    );
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch____not_granted_other_port_or_method____denied() {
    let (address, accepted) = server(None).await;
    let rt = runtime(Egress::default()).await;
    let allow = [format!("http://{address}")];
    let ungranted = up(&rt, spec("egress-ungranted", &allow, false)).await;
    let other_port = up(
        &rt,
        spec(
            "egress-port",
            &[format!("http://127.0.0.1:{}", address.port() + 1)],
            true,
        ),
    )
    .await;
    let mut get_only = spec("egress-methods", &allow, true);
    get_only.services.egress.methods = vec!["GET".into()];
    let get_only = up(&rt, get_only).await;

    let a = ask(&ungranted, &format!("fetch GET http://{address}/hello")).await;
    let b = ask(&other_port, &format!("fetch GET http://{address}/hello")).await;
    let c = ask(&get_only, &format!("fetch POST http://{address}/echo x")).await;
    let d = ask(&get_only, &format!("fetch GET http://{address}/hello")).await;

    assert_eq!(
        (a.as_str(), b.as_str(), c.as_str()),
        ("err:denied", "err:denied", "err:denied")
    );
    assert_eq!(d, "200 hi");
    assert_eq!(accepted.load(Ordering::Relaxed), 1, "only the GET went out");
    for h in [ungranted, other_port, get_only] {
        h.kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch____name_resolving_to_loopback____prohibited_unless_node_allows() {
    let (address, _) = server(None).await;
    let url = format!("fetch GET http://localhost:{}/hello", address.port());
    let allow = [format!("http://localhost:{}", address.port())];
    let guarded = runtime(Egress::default()).await;
    let mut open = Egress::default();
    open.allow_loopback = true;
    let open = runtime(open).await;
    let a = up(&guarded, spec("egress-guarded", &allow, true)).await;
    let b = up(&open, spec("egress-open", &allow, true)).await;

    let refused = ask(&a, &url).await;
    let allowed = ask(&b, &url).await;

    assert_eq!(refused, "err:prohibited");
    assert_eq!(allowed, "200 hi");
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch____app_at_max_concurrent____limited_until_one_finishes() {
    let (address, _) = server(None).await;
    let rt = runtime(Egress::default()).await;
    let allow = [format!("http://{address}")];
    let with_limit = |app| {
        let mut s = spec(app, &allow, true);
        s.services.egress.max_concurrent = Some(1);
        s
    };
    let a = up(&rt, with_limit("egress-limit")).await;
    let b = up(&rt, with_limit("egress-limit")).await;

    let slow = tokio::spawn({
        let a = a.clone();
        let url = format!("fetch GET http://{address}/slow");
        async move { ask(&a, &url).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let while_busy = ask(&b, &format!("fetch GET http://{address}/hello")).await;
    let late = slow.await.unwrap();
    let after = ask(&b, &format!("fetch GET http://{address}/hello")).await;

    assert_eq!(while_busy, "err:limited");
    assert_eq!(late, "200 late");
    assert_eq!(after, "200 hi");
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch____slow_headers_or_stalled_body____timeout_and_instance_lives() {
    let (address, _) = server(None).await;
    let mut egress = Egress::default();
    egress.first_byte_timeout = Duration::from_millis(300);
    egress.between_bytes_timeout = Duration::from_millis(300);
    let rt = runtime(egress).await;
    let h = up(
        &rt,
        spec("egress-timeout", &[format!("http://{address}")], true),
    )
    .await;

    let slow = ask(&h, &format!("fetch GET http://{address}/slow")).await;
    let stalled = ask(&h, &format!("fetch GET http://{address}/stall")).await;
    let fine = ask(&h, &format!("fetch GET http://{address}/hello")).await;

    assert_eq!(slow, "err:timeout");
    assert_eq!(stalled, "err:timeout");
    assert_eq!(fine, "200 hi");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch____https____trusted_with_the_ca_file_only() {
    let dir = tempfile::tempdir().unwrap();
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = rcgen::CertifiedIssuer::self_signed(ca_params, ca_key).unwrap();
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()])
        .unwrap()
        .signed_by(&leaf_key, &ca)
        .unwrap();
    let ca_file = dir.path().join("ca.pem");
    std::fs::write(&ca_file, ca.pem()).unwrap();
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![leaf.der().clone()],
        rustls_pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
    )
    .unwrap();
    let (address, _) = server(Some(Arc::new(tls))).await;
    let mut trusting = Egress::default();
    trusting.system_roots = false;
    trusting.ca_file = Some(ca_file);
    let mut system_only = Egress::default();
    system_only.system_roots = false;
    let trusting = runtime(trusting).await;
    let untrusting = runtime(system_only).await;
    let allow = [format!("https://{address}")];
    let a = up(&trusting, spec("egress-tls", &allow, true)).await;
    let b = up(&untrusting, spec("egress-tls-no-ca", &allow, true)).await;

    let good = ask(&a, &format!("fetch GET https://{address}/hello")).await;
    let bad = ask(&b, &format!("fetch GET https://{address}/hello")).await;

    assert_eq!(good, "200 hi");
    assert!(bad.starts_with("err:tls"), "{bad}");
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch____instance_killed_mid_request____its_slot_freed() {
    let (address, _) = server(None).await;
    let rt = runtime(Egress::default()).await;
    let allow = [format!("http://{address}")];
    let with_limit = |app| {
        let mut s = spec(app, &allow, true);
        s.services.egress.max_concurrent = Some(1);
        s
    };
    let a = up(&rt, with_limit("egress-killed")).await;
    let b = up(&rt, with_limit("egress-killed")).await;
    let slow = tokio::spawn({
        let a = a.clone();
        let url = format!("fetch GET http://{address}/slow");
        async move { ask(&a, &url).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let before = ask(&b, &format!("fetch GET http://{address}/hello")).await;
    a.kill();
    let _ = slow.await;
    let mut after = String::new();
    for _ in 0..50 {
        after = ask(&b, &format!("fetch GET http://{address}/hello")).await;
        if after != "err:limited" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(before, "err:limited");
    assert_eq!(after, "200 hi");
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch____forbidden_header____invalid_and_nothing_sent() {
    let (address, accepted) = server(None).await;
    let rt = runtime(Egress::default()).await;
    let h = up(
        &rt,
        spec("egress-headers", &[format!("http://{address}")], true),
    )
    .await;

    let forbidden = ask(&h, &format!("fetch-with connection http://{address}/hello")).await;
    let plain = ask(&h, &format!("fetch-with x-trace http://{address}/hello")).await;

    assert!(forbidden.starts_with("err:invalid"), "{forbidden}");
    assert_eq!(plain, "200 hi");
    assert_eq!(
        accepted.load(Ordering::Relaxed),
        1,
        "only the plain one went out"
    );
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch____driver_killed_mid_request____answered_its_slot_freed_and_served_again() {
    let (address, _) = server(None).await;
    let rt = runtime(Egress::default()).await;
    let mut limited = spec("egress-driver-killed", &[format!("http://{address}")], true);
    limited.services.egress.max_concurrent = Some(1);
    let h = up(&rt, limited).await;
    let slow = tokio::spawn({
        let h = h.clone();
        let url = format!("fetch GET http://{address}/slow");
        async move { ask(&h, &url).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let driver = bulkhead_host::egress_driver_app("egress-driver-killed");
    for i in rt.0.instances().into_iter().filter(|i| i.app() == driver) {
        i.kill();
    }
    let killed = slow.await.unwrap();
    let mut after = String::new();
    for _ in 0..50 {
        after = ask(&h, &format!("fetch GET http://{address}/hello")).await;
        if after == "200 hi" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(killed.starts_with("err:"), "{killed}");
    assert_ne!(killed, "err:limited");
    assert_eq!(after, "200 hi");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch____two_apps____each_through_its_own_driver_to_its_own_allow_list() {
    let (first, first_accepted) = server(None).await;
    let (second, second_accepted) = server(None).await;
    let rt = runtime(Egress::default()).await;
    let a = up(
        &rt,
        spec("egress-first", &[format!("http://{first}")], true),
    )
    .await;
    let b = up(
        &rt,
        spec("egress-second", &[format!("http://{second}")], true),
    )
    .await;

    let a_own = ask(&a, &format!("fetch GET http://{first}/hello")).await;
    let a_other = ask(&a, &format!("fetch GET http://{second}/hello")).await;
    let b_own = ask(&b, &format!("fetch GET http://{second}/hello")).await;

    assert_eq!((a_own.as_str(), a_other.as_str()), ("200 hi", "err:denied"));
    assert_eq!(b_own, "200 hi");
    assert_eq!(first_accepted.load(Ordering::Relaxed), 1);
    assert_eq!(second_accepted.load(Ordering::Relaxed), 1);
    let drivers: Vec<String> =
        rt.0.instances()
            .iter()
            .map(|i| i.app().to_string())
            .filter(|a| a.starts_with(bulkhead_host::EGRESS_DRIVER_PREFIX))
            .collect();
    assert_eq!(drivers.len(), 2, "{drivers:?}");
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch____name_in_a_denied_range____prohibited_and_reached_with_no_deny_list() {
    let (address, _) = server(None).await;
    let local = format!("fetch GET http://localhost:{}/hello", address.port());
    let allow_local = [format!("http://localhost:{}", address.port())];
    let mut loopback_denied = Egress::default();
    loopback_denied.allow_loopback = true;
    loopback_denied.deny = vec!["127.0.0.0/8".into()];
    let mut nothing_denied = Egress::default();
    nothing_denied.allow_loopback = true;
    nothing_denied.deny = Vec::new();
    let default = runtime(Egress::default()).await;
    let denied = runtime(loopback_denied).await;
    let open = runtime(nothing_denied).await;
    let a = up(
        &default,
        spec("egress-intranet", &["http://intranet".to_string()], true),
    )
    .await;
    let b = up(&denied, spec("egress-denied", &allow_local, true)).await;
    let c = up(&open, spec("egress-undenied", &allow_local, true)).await;

    let private = ask(&a, "fetch GET http://intranet/").await;
    let refused = ask(&b, &local).await;
    let allowed = ask(&c, &local).await;

    assert_eq!(private, "err:prohibited", "10/8 by default");
    assert_eq!(refused, "err:prohibited");
    assert_eq!(allowed, "200 hi");
    for h in [a, b, c] {
        h.kill();
    }
}

/// A CONNECT proxy: it records each request line, refuses with 403 when
/// `refuse`, and otherwise tunnels to the target.
async fn proxy(refuse: bool) -> (SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut client, _) = listener.accept().await.unwrap();
            let log = log.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if client.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let line = String::from_utf8_lossy(&head)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                log.lock().unwrap().push(line.clone());
                if refuse {
                    let _ = client.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n").await;
                    return;
                }
                let target = line.split(' ').nth(1).unwrap_or_default().to_string();
                let Ok(mut upstream) = tokio::net::TcpStream::connect(target).await else {
                    return;
                };
                let _ = client
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await;
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    (address, seen)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch____through_a_proxy____tunneled_by_the_node_refusals_answered() {
    let (address, accepted) = server(None).await;
    let (open_proxy, seen) = proxy(false).await;
    let (closed_proxy, refused_seen) = proxy(true).await;
    let mut via_open = Egress::default();
    via_open.proxy = Some(format!("http://{open_proxy}"));
    let mut via_closed = Egress::default();
    via_closed.proxy = Some(format!("http://{closed_proxy}"));
    let open = runtime(via_open).await;
    let closed = runtime(via_closed).await;
    let allow = [format!("http://{address}")];
    let a = up(&open, spec("egress-proxied", &allow, true)).await;
    let b = up(&closed, spec("egress-proxy-refused", &allow, true)).await;
    let elsewhere = up(
        &open,
        spec(
            "egress-proxy-elsewhere",
            &["http://127.0.0.1:9".to_string()],
            true,
        ),
    )
    .await;

    let through = ask(&a, &format!("fetch GET http://{address}/hello")).await;
    let refused = ask(&b, &format!("fetch GET http://{address}/hello")).await;
    let not_allowed = ask(&elsewhere, &format!("fetch GET http://{address}/hello")).await;
    let after = ask(&b, "self").await;

    assert_eq!(through, "200 hi");
    assert_eq!(
        seen.lock().unwrap().clone(),
        [format!("CONNECT {address} HTTP/1.1")],
        "the node asked for the allowed destination alone"
    );
    assert!(refused.starts_with("err:"), "{refused}");
    assert_eq!(refused_seen.lock().unwrap().len(), 1);
    assert_eq!(not_allowed, "err:denied");
    assert!(!after.is_empty(), "the instance lives");
    assert_eq!(
        accepted.load(Ordering::Relaxed),
        1,
        "only the tunnel reached the server"
    );
    for h in [a, b, elsewhere] {
        h.kill();
    }
}

/// A CA, a server certificate for 127.0.0.1 and a client certificate it
/// signed, as PEM files in `dir`; and a server config that requires a
/// client certificate from that CA, when `require_client`.
fn mutual_tls(
    dir: &std::path::Path,
    require_client: bool,
) -> (
    std::path::PathBuf,
    bulkhead_host::EgressIdentity,
    Arc<rustls::ServerConfig>,
) {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = rcgen::CertifiedIssuer::self_signed(ca_params, ca_key).unwrap();
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()])
        .unwrap()
        .signed_by(&leaf_key, &ca)
        .unwrap();
    let client_key = rcgen::KeyPair::generate().unwrap();
    let client = rcgen::CertificateParams::new(vec!["client".to_string()])
        .unwrap()
        .signed_by(&client_key, &ca)
        .unwrap();
    let ca_file = dir.join("ca.pem");
    std::fs::write(&ca_file, ca.pem()).unwrap();
    let identity = bulkhead_host::EgressIdentity {
        cert: dir.join("client.pem"),
        key: dir.join("client.key"),
    };
    std::fs::write(&identity.cert, client.pem()).unwrap();
    std::fs::write(&identity.key, client_key.serialize_pem()).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.der().clone()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots),
        provider.clone(),
    )
    .build()
    .unwrap();
    let builder = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap();
    let builder = if require_client {
        builder.with_client_cert_verifier(verifier)
    } else {
        builder.with_no_client_auth()
    };
    let tls = builder
        .with_single_cert(
            vec![leaf.der().clone()],
            rustls_pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
        )
        .unwrap();
    (ca_file, identity, Arc::new(tls))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch____server_wanting_a_client_certificate____served_with_the_apps_identity() {
    let dir = tempfile::tempdir().unwrap();
    let (ca_file, identity, tls) = mutual_tls(dir.path(), true);
    let (address, _) = server(Some(tls)).await;
    let mut with_identity = Egress::default();
    with_identity.system_roots = false;
    with_identity.ca_file = Some(ca_file.clone());
    with_identity
        .identities
        .insert("egress-mtls".into(), identity);
    let mut without = Egress::default();
    without.system_roots = false;
    without.ca_file = Some(ca_file);
    let with_identity = runtime(with_identity).await;
    let without = runtime(without).await;
    let allow = [format!("https://{address}")];
    let a = up(&with_identity, spec("egress-mtls", &allow, true)).await;
    let b = up(&without, spec("egress-no-identity", &allow, true)).await;

    let served = ask(&a, &format!("fetch GET https://{address}/hello")).await;
    let refused = ask(&b, &format!("fetch GET https://{address}/hello")).await;

    assert_eq!(served, "200 hi");
    assert!(refused.starts_with("err:"), "{refused}");
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_egress_roots____ca_file_replaced____changed_and_a_new_driver_trusts_it() {
    let dir = tempfile::tempdir().unwrap();
    // A server of the new CA's, not asking for a client certificate.
    let (new_ca, _, tls) = mutual_tls(dir.path(), false);
    let old_dir = tempfile::tempdir().unwrap();
    let (old_ca, _, _) = mutual_tls(old_dir.path(), false);
    let ca_file = dir.path().join("trusted.pem");
    std::fs::copy(&old_ca, &ca_file).unwrap();
    let (address, _) = server(Some(tls)).await;
    let mut egress = Egress::default();
    egress.system_roots = false;
    egress.ca_file = Some(ca_file.clone());
    let rt = runtime(egress).await;
    let allow = [format!("https://{address}")];
    let h = up(&rt, spec("egress-reload", &allow, true)).await;
    let before = ask(&h, &format!("fetch GET https://{address}/hello")).await;

    std::fs::copy(&new_ca, &ca_file).unwrap();
    let changed = rt.0.reload_egress_roots().unwrap();
    let again = rt.0.reload_egress_roots().unwrap();
    let driver = bulkhead_host::egress_driver_app("egress-reload");
    rt.2.remove(&driver).await.unwrap();
    rt.2.add(common::egress_driver(&rt.0, "egress-reload", rt.3.clone()))
        .await
        .unwrap();
    let after = ask(&h, &format!("fetch GET https://{address}/hello")).await;

    assert!(before.starts_with("err:tls"), "{before}");
    assert!(changed);
    assert!(!again, "nothing changed the second time");
    assert_eq!(after, "200 hi");
    h.kill();
}
