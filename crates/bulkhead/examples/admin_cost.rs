//! What a remote admin request and a metrics scrape cost against a running
//! `bulkhead node` (ROADMAP DA7): sequential requests through the
//! signed-request client, and `GET /metrics` over plain HTTP, each with
//! p50, p99 and the CPU of the node's process and its children (its
//! drivers) per request.
//!
//! `cargo run --release -p bulkhead --example admin_cost -- ADDRESS
//! NODE_KEY OPERATOR_KEY METRICS_PORT NODE_PID [N]` (N default 1,000),
//! against a node with `[admin]` and `[metrics]`, on an idle machine.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::print_stdout)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use bulkhead::api::Client;

/// User and system seconds of `pid` and its children.
fn cpu_seconds(pid: u32) -> f64 {
    let pid = pid.to_string();
    let ticks = |p: &str| -> Option<(String, f64)> {
        let stat = std::fs::read_to_string(format!("/proc/{p}/stat")).ok()?;
        let f: Vec<&str> = stat[stat.rfind(')')? + 2..].split_whitespace().collect();
        let t = f[11].parse::<f64>().ok()? + f[12].parse::<f64>().ok()?;
        Some((f[1].to_string(), t / 100.0))
    };
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|p| {
            let (parent, t) = ticks(&p)?;
            (p == pid || parent == pid).then_some(t)
        })
        .sum()
}

async fn row<F, Fut>(label: &str, n: usize, pid: u32, f: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut took = Vec::with_capacity(n);
    let cpu = cpu_seconds(pid);
    for i in 0..n {
        let t = Instant::now();
        assert!(f().await, "{label}: request {i} failed");
        took.push(t.elapsed());
    }
    let cpu = cpu_seconds(pid) - cpu;
    took.sort();
    let at = |q: f64| took[((took.len() - 1) as f64 * q) as usize];
    println!(
        "| {label} | {:.0} | {:.0} | {:.0} |",
        at(0.5).as_secs_f64() * 1e6,
        at(0.99).as_secs_f64() * 1e6,
        cpu / n as f64 * 1e6,
    );
}

async fn scrape(port: u16) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Ok(mut s) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await else {
        return false;
    };
    if s.write_all(b"GET /metrics HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
        .await
        .is_err()
    {
        return false;
    }
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.is_ok() && raw.starts_with(b"HTTP/1.1 200")
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (address, node_key, operator, port, pid) = (
        &args[1],
        &args[2],
        &args[3],
        args[4].parse::<u16>().unwrap(),
        args[5].parse::<u32>().unwrap(),
    );
    let n: usize = args.get(6).map_or(1_000, |a| a.parse().unwrap());
    let node_key: bulkhead_keys::PublicKey = node_key.parse().unwrap();
    let operator = std::path::Path::new(operator);
    let tls = bulkhead_keys::admin_client_tls(operator, node_key.clone()).unwrap();
    let signer = bulkhead_keys::signature::Signer::from_file(operator).unwrap();
    let client = Client::remote(address.clone(), Arc::new(tls), Arc::new(signer), node_key);
    // Warm: the node, its drivers and the connections' first handshakes.
    for _ in 0..20 {
        client.list().await.unwrap();
        assert!(scrape(port).await);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    println!("{n} each, one at a time; CPU of the node and its drivers");
    println!();
    println!("| request | p50 µs | p99 µs | node CPU µs/request |");
    println!("| --- | --- | --- | --- |");
    row(
        "remote list (a new TLS connection each)",
        n,
        pid,
        || async { client.list().await.is_ok() },
    )
    .await;
    row("remote status of an absent app", n, pid, || async {
        client.status("absent").await.is_err()
    })
    .await;
    row("GET /metrics", n, pid, || scrape(port)).await;
}
