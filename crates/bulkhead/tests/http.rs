//! HTTP ingress through a node: `examples/http-counter` as a native
//! program (ROADMAP Z9d), bundled with the CLI, deployed under operator policy, and driven over
//! HTTP; route clashes, ungranted routes, stop and start.

// Native apps run on Linux and FreeBSD (`bulkhead-runner`).
#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::Duration;

use bulkhead::node::{LogRing, Node};

mod common;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The counter built as a native program (its `native` feature).
fn build() -> PathBuf {
    let target = root().join("target/fixtures/examples-http-native");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let ok = StdCommand::new(cargo)
        .current_dir(root())
        .args(["build", "--release", "-p", "example-http-counter"])
        .args(["--bin", "example-http-counter", "--target-dir"])
        .arg(&target)
        .status()
        .unwrap()
        .success();
    assert!(ok, "building example-http-counter natively failed");
    target.join("release/example-http-counter")
}

struct Run {
    ok: bool,
    stdout: String,
    stderr: String,
}

async fn bulkhead(socket: &Path, args: &[&str]) -> Run {
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_bulkhead"))
        .arg("--socket")
        .arg(socket)
        .args(args)
        .output()
        .await
        .unwrap();
    Run {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn free_port() -> u16 {
    common::ports::free_port().unwrap()
}

/// One request on its own connection: (status, body).
async fn http(port: u16, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    (
        head.split(' ').nth(1).unwrap().parse().unwrap(),
        body.to_string(),
    )
}

struct Setup {
    dir: TempDir,
    key: PathBuf,
    public: String,
    program: PathBuf,
}

async fn setup() -> Setup {
    let program = build();
    let dir = TempDir::new().unwrap();
    let keys = bulkhead(
        &dir.path().join("x.sock"),
        &["keygen", "--no-password", s(&dir.path().join("k"))],
    )
    .await;
    assert!(keys.ok, "{}", keys.stderr);
    Setup {
        key: dir.path().join("k.key"),
        public: keys.stdout,
        dir,
        program,
    }
}

/// Bundle the counter's program as `name` with `routes` (TOML).
async fn bundle(setup: &Setup, name: &str, routes: &str) -> PathBuf {
    let manifest = setup.dir.path().join(format!("{name}.toml"));
    std::fs::write(
        &manifest,
        format!(
            "[app]\nname = \"{name}\"\nversion = \"1.0.0\"\n\
             [appserver]\nkind = \"native-process\"\nnative = {{ program = \"executable\" }}\n\
             [appserver.capabilities.\"bulkhead:http-routes\"]\nroutes = {routes}\n"
        ),
    )
    .unwrap();
    let out = setup.dir.path().join(format!("{name}.rbp"));
    let r = bulkhead(
        &setup.dir.path().join("x.sock"),
        &[
            "bundle",
            s(&manifest),
            s(&setup.program),
            "--key",
            s(&setup.key),
            "-o",
            s(&out),
        ],
    )
    .await;
    assert!(r.ok, "{}", r.stderr);
    out
}

async fn node(setup: &Setup, tag: &str, extra: &str) -> (Arc<Node>, PathBuf) {
    let socket = setup.dir.path().join(format!("{tag}.sock"));
    let policy = setup.dir.path().join("policy.toml");
    std::fs::write(
        &policy,
        "[apps.http-counter]\ngrant = [\"bulkhead:http-routes\"]\n\
         [apps.thief]\ngrant = [\"bulkhead:http-routes\"]\n",
    )
    .unwrap();
    let toml = format!(
        "data_dir = {:?}\nadmin_socket = {socket:?}\ntrusted_keys = [{:?}]\npolicy = {policy:?}\n{extra}\n\
         [native]\nrunner = {:?}\n",
        setup.dir.path().join(tag),
        setup.public,
        {
            common::ingress_built();
            common::native_fixtures().0.clone()
        },
    );
    let node = Arc::new(
        Node::open(common::node_config(&toml), LogRing::new(100))
            .await
            .unwrap(),
    );
    let server = node.clone();
    tokio::spawn(async move {
        bulkhead::node::serve(server, std::future::pending::<()>())
            .await
            .unwrap();
    });
    while !socket.exists() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    (node, socket)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_counter____deployed_with_policy____served_and_routes_follow_the_app() {
    let setup = setup().await;
    let port = free_port();
    let (node, socket) = node(
        &setup,
        "a",
        &format!("[http.main]\naddress = \"127.0.0.1:{port}\"\n"),
    )
    .await;
    let counter = bundle(&setup, "http-counter", "[{ path_prefix = \"/counter\" }]").await;
    let thief = bundle(&setup, "thief", "[{ path_prefix = \"/counter\" }]").await;
    let ungranted = bundle(&setup, "ungranted", "[{ path_prefix = \"/free\" }]").await;

    let deployed = bulkhead(&socket, &["deploy", s(&counter)]).await;
    let first = http(port, "GET", "/counter", "").await;
    let posted = http(port, "POST", "/counter", "5").await;
    let wrong = http(port, "PUT", "/counter", "").await;
    let clash = bulkhead(&socket, &["deploy", s(&thief)]).await;
    let free = bulkhead(&socket, &["--json", "deploy", s(&ungranted)]).await;
    let not_routed = http(port, "GET", "/free", "").await;
    assert!(bulkhead(&socket, &["stop", "http-counter"]).await.ok);
    let stopped = http(port, "GET", "/counter", "").await;
    assert!(bulkhead(&socket, &["start", "http-counter"]).await.ok);
    let started = http(port, "GET", "/counter", "").await;

    assert!(deployed.ok, "{}", deployed.stderr);
    assert_eq!(first, (200, r#"{"count":0}"#.to_string()));
    assert_eq!(posted, (200, r#"{"count":5}"#.to_string()));
    assert_eq!(wrong.0, 405);
    assert!(!clash.ok);
    assert!(
        clash.stderr.contains("belongs to http-counter"),
        "{}",
        clash.stderr
    );
    assert!(free.ok, "{}", free.stderr);
    assert!(
        free.stdout.contains("bulkhead:http-routes"),
        "{}",
        free.stdout
    );
    assert_eq!(not_routed.0, 404);
    assert_eq!(stopped.0, 404);
    assert_eq!(started, (200, r#"{"count":0}"#.to_string()));
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn routes____node_without_ingress____deploy_refused() {
    let setup = setup().await;
    let (node, socket) = node(&setup, "b", "").await;
    let counter = bundle(&setup, "http-counter", "[{ path_prefix = \"/counter\" }]").await;

    let refused = bulkhead(&socket, &["deploy", s(&counter)]).await;

    assert!(!refused.ok);
    assert!(
        refused.stderr.contains("no HTTP ingress"),
        "{}",
        refused.stderr
    );
    node.shutdown().await;
}

/// A self-signed certificate for `host` written to `cert` and `key`; its
/// DER.
fn write_cert(cert: &Path, key: &Path, host: &str) -> Vec<u8> {
    let made = rcgen::generate_simple_self_signed(vec![host.to_string()]).unwrap();
    std::fs::write(cert, made.cert.pem()).unwrap();
    std::fs::write(key, made.signing_key.serialize_pem()).unwrap();
    made.cert.der().to_vec()
}

/// One HTTPS GET trusting `trusted`: (the certificate served, status).
async fn https_get(port: u16, trusted: &[&Vec<u8>], path: &str) -> (Vec<u8>, u16) {
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
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let name = rustls_pki_types::ServerName::try_from("shop.example").unwrap();
    let mut stream = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .unwrap();
    let served = stream.get_ref().1.peer_certificates().unwrap()[0].to_vec();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nhost: shop.example\r\nconnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw).await;
    let raw = String::from_utf8_lossy(&raw).into_owned();
    (served, raw.split(' ').nth(1).unwrap().parse().unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn https____reload_tls____new_certificate_and_a_broken_one_refused() {
    let setup = setup().await;
    let port = free_port();
    let cert = setup.dir.path().join("shop.pem");
    let key = setup.dir.path().join("shop.key");
    let first = write_cert(&cert, &key, "shop.example");
    let (node, socket) = node(
        &setup,
        "tls",
        &format!(
            "[http.main]\naddress = \"127.0.0.1:{port}\"\ntls_reload_s = 0\n\
             tls = [{{ cert = {cert:?}, key = {key:?}, hosts = [\"shop.example\"] }}]\n"
        ),
    )
    .await;
    let counter = bundle(&setup, "http-counter", "[{ path_prefix = \"/counter\" }]").await;
    assert!(bulkhead(&socket, &["deploy", s(&counter)]).await.ok);

    let (served_first, status) = https_get(port, &[&first], "/counter").await;
    let second = write_cert(&cert, &key, "shop.example");
    let reloaded = bulkhead(&socket, &["reload-tls"]).await;
    let (served_second, _) = https_get(port, &[&first, &second], "/counter").await;
    std::fs::write(&cert, "broken").unwrap();
    let refused = bulkhead(&socket, &["reload-tls"]).await;
    let (still, _) = https_get(port, &[&first, &second], "/counter").await;

    assert_eq!((served_first, status), (first.clone(), 200));
    assert!(reloaded.ok, "{}", reloaded.stderr);
    // Rewritten in place: the driver reloads, it does not restart.
    assert!(
        !reloaded.stdout.contains("restarted"),
        "{}",
        reloaded.stdout
    );
    assert_eq!(served_second, second);
    assert!(!refused.ok);
    assert!(refused.stderr.contains("http.main"), "{}", refused.stderr);
    assert_eq!(still, second);
    node.shutdown().await;
}

/// A certificate renewed as certbot does (ROADMAP DA2b): new files in an
/// archive directory, the paths the node was given symlinks re-pointed by
/// rename. The driver cannot read the new files through the rules it got
/// at start, so `reload-tls` restarts it, and it serves the new
/// certificate; so it does after a plain rename over the path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn https____certificate_renewed_by_rename____driver_restarted_and_serves_it() {
    let setup = setup().await;
    let port = free_port();
    let archive = setup.dir.path().join("archive");
    let live = setup.dir.path().join("live");
    std::fs::create_dir_all(&archive).unwrap();
    std::fs::create_dir_all(&live).unwrap();
    let (cert, key) = (live.join("shop.pem"), live.join("shop.key"));
    let renew = |n: u32| {
        let (c, k) = (
            archive.join(format!("cert{n}.pem")),
            archive.join(format!("key{n}.pem")),
        );
        let der = write_cert(&c, &k, "shop.example");
        for (target, link) in [(&c, &cert), (&k, &key)] {
            let tmp = live.join("tmp-link");
            let _ = std::fs::remove_file(&tmp);
            std::os::unix::fs::symlink(target, &tmp).unwrap();
            std::fs::rename(&tmp, link).unwrap();
        }
        der
    };
    let first = renew(1);
    let (node, socket) = node(
        &setup,
        "renew",
        &format!(
            "[http.main]\naddress = \"127.0.0.1:{port}\"\ntls_reload_s = 0\n\
             tls = [{{ cert = {cert:?}, key = {key:?}, hosts = [\"shop.example\"] }}]\n"
        ),
    )
    .await;
    let counter = bundle(&setup, "http-counter", "[{ path_prefix = \"/counter\" }]").await;
    assert!(bulkhead(&socket, &["deploy", s(&counter)]).await.ok);

    let (served_first, _) = https_get(port, &[&first], "/counter").await;
    let second = renew(2);
    let renewed = bulkhead(&socket, &["reload-tls"]).await;
    let (served_second, status) = https_get(port, &[&first, &second], "/counter").await;
    // A plain rename over the path, the symlinks gone.
    let (c3, k3) = (
        setup.dir.path().join("c3.pem"),
        setup.dir.path().join("k3.pem"),
    );
    let third = write_cert(&c3, &k3, "shop.example");
    std::fs::rename(&c3, &cert).unwrap();
    std::fs::rename(&k3, &key).unwrap();
    let renamed = bulkhead(&socket, &["reload-tls"]).await;
    let (served_third, _) = https_get(port, &[&second, &third], "/counter").await;

    assert_eq!(served_first, first);
    assert!(renewed.ok, "{}", renewed.stderr);
    assert!(renewed.stdout.contains("restarted"), "{}", renewed.stdout);
    assert_eq!((served_second, status), (second, 200));
    assert!(renamed.ok, "{}", renamed.stderr);
    assert_eq!(served_third, third);
    node.shutdown().await;
}

/// Client certificates through the node (ROADMAP DA2b): `client_ca` on
/// an HTTPS listener, required by default. A client with a certificate
/// from the CA is served; one without is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn https____client_ca____served_with_a_certificate_from_it_only() {
    let setup = setup().await;
    let port = free_port();
    let dir = setup.dir.path();
    let (cert, key, ca_file) = (
        dir.join("shop.pem"),
        dir.join("shop.key"),
        dir.join("ca.pem"),
    );
    let served = write_cert(&cert, &key, "shop.example");
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca =
        rcgen::CertifiedIssuer::self_signed(params, rcgen::KeyPair::generate().unwrap()).unwrap();
    std::fs::write(&ca_file, ca.pem()).unwrap();
    let client_key = rcgen::KeyPair::generate().unwrap();
    let client = rcgen::CertificateParams::new(vec!["client.example".to_string()])
        .unwrap()
        .signed_by(&client_key, &ca)
        .unwrap();
    let (node, socket) = node(
        &setup,
        "mtls",
        &format!(
            "[http.main]\naddress = \"127.0.0.1:{port}\"\nclient_ca = {ca_file:?}\n\
             tls = [{{ cert = {cert:?}, key = {key:?}, hosts = [\"shop.example\"] }}]\n"
        ),
    )
    .await;
    let counter = bundle(&setup, "http-counter", "[{ path_prefix = \"/counter\" }]").await;
    assert!(bulkhead(&socket, &["deploy", s(&counter)]).await.ok);

    let with = https_status(
        port,
        &served,
        Some((client.der().to_vec(), client_key.serialize_der())),
    )
    .await;
    let without = https_status(port, &served, None).await;

    assert_eq!(with, Some(200));
    assert_eq!(without, None);
    node.shutdown().await;
}

/// One HTTPS GET of `/counter` trusting `trusted`, offering `client`
/// (certificate, key; DER) if given: the status, or `None` without an
/// answer.
async fn https_status(
    port: u16,
    trusted: &[u8],
    client: Option<(Vec<u8>, Vec<u8>)>,
) -> Option<u16> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls_pki_types::CertificateDer::from(trusted.to_vec()))
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
                vec![rustls_pki_types::CertificateDer::from(cert)],
                rustls_pki_types::PrivateKeyDer::Pkcs8(key.into()),
            )
            .unwrap(),
    };
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let name = rustls_pki_types::ServerName::try_from("shop.example").unwrap();
    let mut stream = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .ok()?;
    stream
        .write_all(b"GET /counter HTTP/1.1\r\nhost: shop.example\r\nconnection: close\r\n\r\n")
        .await
        .ok()?;
    let mut raw = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        stream.read_to_end(&mut raw),
    )
    .await
    .ok()?
    .ok()?;
    String::from_utf8_lossy(&raw)
        .split(' ')
        .nth(1)?
        .parse()
        .ok()
}

/// `bulkhead list` shows the node's own drivers beside the apps, and
/// `bulkhead status` an app's routes and a driver alike (ROADMAP DA2b).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_and_status____ingress_and_routes____shown() {
    let setup = setup().await;
    let port = free_port();
    let (node, socket) = node(
        &setup,
        "list",
        &format!("[http.main]\naddress = \"127.0.0.1:{port}\"\n"),
    )
    .await;
    let counter = bundle(
        &setup,
        "http-counter",
        "[{ path_prefix = \"/counter\" }, { host = \"shop.example\", path_prefix = \"/shop\" }]",
    )
    .await;
    assert!(bulkhead(&socket, &["deploy", s(&counter)]).await.ok);

    let list = bulkhead(&socket, &["list"]).await;
    let status = bulkhead(&socket, &["status", "http-counter"]).await;
    let driver = bulkhead(&socket, &["status", "bulkhead-ingress"]).await;

    assert!(list.ok, "{}", list.stderr);
    let row = |name: &str| {
        list.stdout
            .lines()
            .find(|l| l.starts_with(name))
            .map(str::to_string)
    };
    assert!(
        row("http-counter").is_some_and(|l| l.contains("1.0.0")),
        "{}",
        list.stdout
    );
    for name in ["bulkhead-ingress", "bulkhead-resolve"] {
        assert!(
            row(name).is_some_and(|l| l.contains("built-in") && l.contains("running")),
            "{}",
            list.stdout
        );
    }
    assert!(
        status.stdout.contains("route:     */counter"),
        "{}",
        status.stdout
    );
    assert!(
        status.stdout.contains("route:     shop.example/shop"),
        "{}",
        status.stdout
    );
    assert!(driver.ok, "{}", driver.stderr);
    assert!(
        driver.stdout.contains("built into the node"),
        "{}",
        driver.stdout
    );
    node.shutdown().await;
}
