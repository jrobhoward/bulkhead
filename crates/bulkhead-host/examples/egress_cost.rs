//! What an outgoing HTTP request costs an app (ROADMAP DA4), for
//! `docs/BASELINE.md`: the SDK fixture's `fetch` command, a call into
//! the app whose request goes to the node and, since DA4, on to the app's
//! egress driver (`bulkhead-egress`), against a local server in this
//! process (hyper; HTTPS with a test CA), kept alive.
//!
//! - `GET` of a 2-byte body over HTTP, then over HTTPS, one at a time;
//! - `GET` of a 1 MiB body over HTTP, one at a time;
//! - 16 instances of one app, each fetching over HTTP, then HTTPS, one at
//!   a time.
//!
//! p50 and p99 per request, CPU per request of this process and its
//! children (the node's side, the app, the driver, and the server), and
//! requests per second; then the driver's resident memory, idle after
//! its first requests and after the rows. Before DA4 the same example ran
//! against the node's own pool: drop the block marked `DA4` to run it on
//! a tree from before (commit 8c6f9e2).
//!
//! ```sh
//! cargo run --release -p bulkhead-host --example egress_cost -- [requests]
//! ```
//!
//! Run on an idle machine (default 5,000 requests a row).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    missing_docs
)]

#[path = "../tests/common/mod.rs"]
mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bulkhead_bundle::manifest::{Capability, NativeProgram};
use bulkhead_host::{
    Egress, Envelope, InstanceHandle, InstanceSpec, NativeCode, NativeConfig, Runtime,
    RuntimeConfig,
};
use bytes::Bytes;
use http_body_util::Full;

const LONG: Duration = Duration::from_secs(60);

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .map_or(5_000, |a| a.parse().expect("requests: a number"));
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
        .block_on(run(n));
}

/// Release builds of `bulkhead-runner`, the SDK fixture and
/// `bulkhead-egress` (where the tree has it).
fn programs() -> (PathBuf, PathBuf, PathBuf) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut build = Command::new(&cargo);
    build
        .current_dir(&root)
        .args(["build", "--release", "-p", "bulkhead-runner"]);
    if root.join("crates/bulkhead-egress").is_dir() {
        build.args(["-p", "bulkhead-egress"]);
    }
    assert!(build.status().unwrap().success());
    let sdk = Command::new(&cargo)
        .current_dir(&root)
        .args(["build", "--release", "-p", "bulkhead-fixture-sdk"])
        .args(["--bin", "bulkhead-fixture-sdk"])
        .status()
        .unwrap();
    assert!(sdk.success());
    let release = root.join("target/release");
    (
        release.join("bulkhead-runner"),
        release.join("bulkhead-fixture-sdk"),
        release.join("bulkhead-egress"),
    )
}

/// `(parent pid, comm, user and system seconds, resident KiB)` of `pid`.
fn stat(pid: &str) -> Option<(String, String, f64, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let comm = stat[stat.find('(')? + 1..stat.rfind(')')?].to_string();
    let f: Vec<&str> = stat[stat.rfind(')')? + 2..].split_whitespace().collect();
    let cpu = (f[11].parse::<f64>().ok()? + f[12].parse::<f64>().ok()?) / 100.0;
    let pages: u64 = f[21].parse().ok()?;
    Some((f[1].to_string(), comm, cpu, pages * 4))
}

/// This process's children.
fn children() -> Vec<(String, String, f64, u64)> {
    let me = std::process::id().to_string();
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|p| stat(&p))
        .filter(|(parent, ..)| *parent == me)
        .collect()
}

/// User and system seconds of this process and its children.
fn cpu_seconds() -> f64 {
    let me = stat(&std::process::id().to_string()).map_or(0.0, |s| s.2);
    me + children().iter().map(|c| c.2).sum::<f64>()
}

/// The egress driver's resident KiB, if one runs.
fn driver_rss() -> Option<u64> {
    children()
        .into_iter()
        .find(|c| c.1 == "bulkhead-egress")
        .map(|c| c.3)
}

/// A server answering `/small` with 2 bytes and `/large` with 1 MiB, over
/// TLS with `tls`.
async fn server(tls: Option<Arc<rustls::ServerConfig>>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let large = Bytes::from(vec![b'x'; 1 << 20]);
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let tls = tls.clone();
            let large = large.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |req: hyper::Request<_>| {
                    let body = if req.uri().path() == "/large" {
                        large.clone()
                    } else {
                        Bytes::from_static(b"hi")
                    };
                    async move { Ok::<_, Infallible>(hyper::Response::new(Full::new(body))) }
                });
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
    address
}

/// A test CA's certificate (PEM) and a server config for `127.0.0.1`
/// signed by it.
fn test_tls() -> (String, Arc<rustls::ServerConfig>) {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = rcgen::CertifiedIssuer::self_signed(ca_params, ca_key).unwrap();
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()])
        .unwrap()
        .signed_by(&leaf_key, &ca)
        .unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
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
    (ca.pem(), Arc::new(config))
}

async fn ask(instance: &InstanceHandle, command: &str) -> String {
    let reply = instance
        .call(Envelope::new("text/plain", command), LONG)
        .await
        .unwrap();
    String::from_utf8(reply.body).unwrap()
}

/// One request, answered `200`.
async fn op(instance: &InstanceHandle, command: &str) -> Duration {
    let t = Instant::now();
    let got = ask(instance, command).await;
    assert!(got.starts_with("200 "), "{command}: {got:.80}");
    t.elapsed()
}

fn print_row(label: &str, mut took: Vec<Duration>, cpu: f64, wall: Duration) {
    let n = took.len();
    took.sort();
    let at = |q: f64| took[((n - 1) as f64 * q) as usize];
    println!(
        "| {label} | {:.0} | {:.0} | {:.0} | {:.0} |",
        at(0.5).as_secs_f64() * 1e6,
        at(0.99).as_secs_f64() * 1e6,
        cpu / n as f64 * 1e6,
        n as f64 / wall.as_secs_f64(),
    );
}

/// `n` requests of `command`, one at a time.
async fn row(label: &str, n: usize, instance: &InstanceHandle, command: &str) {
    let cpu = cpu_seconds();
    let start = Instant::now();
    let mut took = Vec::with_capacity(n);
    for _ in 0..n {
        took.push(op(instance, command).await);
    }
    print_row(label, took, cpu_seconds() - cpu, start.elapsed());
}

/// `n` requests of `command` from each of `instances` at once.
async fn row_together(label: &str, n: usize, instances: &[InstanceHandle], command: &str) {
    let cpu = cpu_seconds();
    let start = Instant::now();
    let tasks: Vec<_> = instances
        .iter()
        .cloned()
        .map(|i| {
            let command = command.to_string();
            tokio::spawn(async move {
                let mut took = Vec::with_capacity(n);
                for _ in 0..n {
                    took.push(op(&i, &command).await);
                }
                took
            })
        })
        .collect();
    let mut took = Vec::new();
    for t in tasks {
        took.extend(t.await.unwrap());
    }
    print_row(label, took, cpu_seconds() - cpu, start.elapsed());
}

fn spec(allow: &[String]) -> InstanceSpec {
    let mut s = InstanceSpec {
        memory_max: 256 << 20,
        callback_deadline: LONG,
        init_timeout: Duration::from_secs(5),
        ..InstanceSpec::new("bench", "1.0.0")
    };
    s.granted.insert(Capability::HttpOutgoing);
    s.services.egress.allow = allow.to_vec();
    s
}

async fn run(n: usize) {
    let (runner, fixture, egress) = programs();
    let (ca_pem, tls) = test_tls();
    let plain = server(None).await;
    let secure = server(Some(tls)).await;
    let dir = tempfile::tempdir().unwrap();
    let ca_file = dir.path().join("ca.pem");
    std::fs::write(&ca_file, ca_pem).unwrap();
    let mut config = Egress::default();
    config.system_roots = false;
    config.ca_file = Some(ca_file);
    let mut native = NativeConfig::default();
    native.runner = Some(runner);
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 64,
        native,
        egress: config,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let apps = rt.start_apps(Vec::new()).await.unwrap();
    let _ = &egress;
    // DA4: the app's egress driver.
    let code = rt.load_native(NativeCode {
        path: egress.clone(),
        kind: NativeProgram::Executable,
    });
    apps.add(common::egress_driver(&rt, "bench", code))
        .await
        .unwrap();
    // End of DA4.

    let app = rt.load_native(NativeCode {
        path: fixture,
        kind: NativeProgram::Executable,
    });
    let allow = [format!("http://{plain}"), format!("https://{secure}")];
    let one = rt.spawn(&app, spec(&allow)).unwrap();
    let many: Vec<InstanceHandle> = (0..16)
        .map(|_| rt.spawn(&app, spec(&allow)).unwrap())
        .collect();
    let http = format!("fetch GET http://{plain}/small");
    let https = format!("fetch GET https://{secure}/small");
    let large = format!("fetch GET http://{plain}/large");
    // Warm: each instance started, connections pooled.
    for i in std::iter::once(&one).chain(&many) {
        op(i, &http).await;
        op(i, &https).await;
    }
    let idle = driver_rss();

    println!(
        "requests: {n} a row (16 instances: {n} each); CPU of this process and its children \
         (with the server); {} cores",
        std::thread::available_parallelism().map_or(0, std::num::NonZero::get)
    );
    println!();
    println!("| request | p50 µs | p99 µs | CPU µs/req | req/s |");
    println!("| --- | --- | --- | --- | --- |");
    row("HTTP `GET`, 2 bytes", n, &one, &http).await;
    row("HTTPS `GET`, 2 bytes", n, &one, &https).await;
    row("HTTP `GET`, 1 MiB", n / 10, &one, &large).await;
    row_together("16 instances at once, HTTP", n, &many, &http).await;
    row_together("16 instances at once, HTTPS", n, &many, &https).await;
    println!();
    match (idle, driver_rss()) {
        (Some(idle), Some(after)) => {
            println!("driver resident: {idle} KiB warm and idle, {after} KiB after the rows");
        }
        _ => println!("no egress driver"),
    }
    one.kill();
    for i in many {
        i.kill();
    }
    apps.stop().await;
}
