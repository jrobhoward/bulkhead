//! What a host-name lookup costs through the resolver driver (ROADMAP DA9)
//! against the ways it could be done in the node: libc's `getaddrinfo`
//! (what the node did before), and hickory in the node's own process (the
//! alternative DA6 set aside). Sequential lookups, p50 and p99, and CPU per
//! lookup of this process and its children (the driver among them).
//!
//! - a name in a hosts file: the driver's own text against `getaddrinfo`
//!   reading `/etc/hosts` (`localhost`);
//! - a cached name: the driver's cache against hickory's in this process,
//!   both filled from a DNS-over-TCP server this process runs, and
//!   `getaddrinfo` of a name the system's resolver has cached, for
//!   reference (skipped without one);
//! - an uncached name, a new one each time, answered by that server: the
//!   driver against hickory in this process, one TCP stream each.
//!
//! Run on an idle machine: `cargo run --release -p bulkhead-resolve
//! --example lookup_cost [lookups]` (default 10,000).

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::print_stdout)]

#[path = "../tests/common/mod.rs"]
mod common;

use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use bulkhead_host::{NativeConfig, Resolver, Runtime, RuntimeConfig};
use common::Dns;
use hickory_resolver::config::{ConnectionConfig, NameServerConfig, ResolveHosts, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;

const WAIT: Duration = Duration::from_secs(10);

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .map_or(10_000, |a| a.parse().expect("lookups: a number"));
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
        .block_on(run(n));
}

/// `bulkhead-runner` and `bulkhead-resolve`, release builds.
fn programs() -> (PathBuf, PathBuf) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(&root)
        .args([
            "build",
            "--release",
            "-p",
            "bulkhead-native",
            "-p",
            "bulkhead-resolve",
        ])
        .status()
        .unwrap();
    assert!(status.success());
    let release = root.join("target/release");
    (
        release.join("bulkhead-runner"),
        release.join("bulkhead-resolve"),
    )
}

/// User and system seconds of this process and its children (the driver).
fn cpu_seconds() -> f64 {
    let me = std::process::id().to_string();
    let ticks = |pid: &str| -> Option<(String, f64)> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let f: Vec<&str> = stat[stat.rfind(')')? + 2..].split_whitespace().collect();
        let t = f[11].parse::<f64>().ok()? + f[12].parse::<f64>().ok()?;
        Some((f[1].to_string(), t / 100.0))
    };
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|p| {
            let (parent, t) = ticks(&p)?;
            (p == me || parent == me).then_some(t)
        })
        .sum()
}

/// A row: `n` lookups of `name(i)` by `lookup`, one at a time.
async fn row<F, Fut>(label: &str, n: usize, name: impl Fn(usize) -> String, lookup: F)
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut took = Vec::with_capacity(n);
    let cpu = cpu_seconds();
    let start = Instant::now();
    for i in 0..n {
        let t = Instant::now();
        assert!(lookup(name(i)).await, "{label}: lookup {i} failed");
        took.push(t.elapsed());
    }
    let wall = start.elapsed();
    let cpu = cpu_seconds() - cpu;
    took.sort();
    let at = |q: f64| took[((took.len() - 1) as f64 * q) as usize];
    println!(
        "| {label} | {:.1} | {:.1} | {:.1} | {:.0} |",
        at(0.5).as_secs_f64() * 1e6,
        at(0.99).as_secs_f64() * 1e6,
        cpu / n as f64 * 1e6,
        n as f64 / wall.as_secs_f64(),
    );
}

async fn run(n: usize) {
    let (runner, resolve) = programs();
    let uncached: Vec<String> = (0..n).map(|i| format!("u{i}.bench")).collect();
    let in_process: Vec<String> = (0..n).map(|i| format!("p{i}.bench")).collect();
    let mut records: Vec<(&str, &[&str])> = vec![("cached.bench", &["192.0.2.1", "2001:db8::1"])];
    let both: &[&str] = &["192.0.2.2", "2001:db8::2"];
    records.extend(
        uncached
            .iter()
            .chain(&in_process)
            .map(|n| (n.as_str(), both)),
    );
    let dns = Dns::start(&records).await;

    let mut native = NativeConfig::default();
    native.runner = Some(runner);
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        native,
        resolver: Resolver::App(common::RESOLVE.into()),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let apps = rt.start_apps(Vec::new()).await.unwrap();
    common::start(
        &rt,
        &apps,
        &resolve,
        &[dns.addr],
        "",
        "127.0.0.1 localhost hosted.bench\n",
    )
    .await;

    let mut config = ResolverConfig::from_parts(None, vec![], vec![]);
    let mut tcp = ConnectionConfig::tcp();
    tcp.port = dns.addr.port();
    let mut server = NameServerConfig::udp_and_tcp(dns.addr.ip());
    server.connections = vec![tcp];
    config.add_name_server(server);
    let mut builder =
        hickory_resolver::Resolver::builder_with_config(config, TokioRuntimeProvider::default());
    builder.options_mut().use_hosts_file = ResolveHosts::Never;
    let hickory = builder.build().unwrap();

    let driver = |name: String| {
        let rt = rt.clone();
        async move {
            rt.resolve(&name, 0, WAIT)
                .await
                .is_ok_and(|a| !a.is_empty())
        }
    };
    let local = |name: String| {
        let hickory = hickory.clone();
        async move { hickory.lookup_ip(name.as_str()).await.is_ok() }
    };
    let gai = |name: String| async move {
        tokio::task::spawn_blocking(move || {
            (name.as_str(), 0)
                .to_socket_addrs()
                .is_ok_and(|mut a| a.next().is_some())
        })
        .await
        .unwrap()
    };
    // Warm: the driver started and connected, both caches filled, the
    // system's resolver asked once.
    assert!(driver("cached.bench".into()).await);
    assert!(local("cached.bench".into()).await);
    let system = gai("example.com".into()).await;

    println!(
        "lookups: {n} each, one at a time; CPU of this process and the driver; {} cores",
        std::thread::available_parallelism().map_or(0, std::num::NonZero::get)
    );
    println!();
    println!("| lookup | p50 µs | p99 µs | CPU µs/lookup | lookups/s |");
    println!("| --- | --- | --- | --- | --- |");
    row("hosts file: driver", n, |_| "hosted.bench".into(), &driver).await;
    row(
        "hosts file: getaddrinfo (localhost)",
        n,
        |_| "localhost".into(),
        &gai,
    )
    .await;
    row("cached: driver", n, |_| "cached.bench".into(), &driver).await;
    row(
        "cached: hickory in this process",
        n,
        |_| "cached.bench".into(),
        &local,
    )
    .await;
    if system {
        row(
            "cached: getaddrinfo via the system's resolver (example.com)",
            n,
            |_| "example.com".into(),
            &gai,
        )
        .await;
    }
    row("uncached: driver", n, |i| uncached[i].clone(), &driver).await;
    row(
        "uncached: hickory in this process",
        n,
        |i| in_process[i].clone(),
        &local,
    )
    .await;
    println!();
    println!(
        "DNS queries the server answered: {} (A and AAAA per uncached lookup)",
        dns.queries()
    );
    apps.stop().await;
}
