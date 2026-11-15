//! What a SQL statement costs an app (ROADMAP DA3), for
//! `docs/BASELINE.md`: the native fixture's `sql-*` commands, each a call
//! into the app, whose request goes to the node and, since DA3, on to the
//! datasource's driver (`bulkhead-sql`) and Postgres. A scratch Postgres
//! on its Unix socket (`tests/common/pg.rs`), as this user.
//!
//! - `SELECT 1` and an `UPDATE` of one row, one at a time;
//! - a transaction: begin, the update, commit (three requests);
//! - 16 instances of one app, each running `SELECT 1` one at a time.
//!
//! p50 and p99 per operation, CPU per operation of this process and its
//! children (the node's side, the app, the driver; not Postgres), and
//! operations per second. Before DA3 the same example ran against the
//! node's own pool: drop the block marked `DA3` to run it on a tree from
//! before (commit 5af7aa0).
//!
//! ```sh
//! cargo run --release -p bulkhead-host --example sql_cost -- [operations]
//! ```
//!
//! Run on an idle machine (default 5,000 operations a row).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    missing_docs
)]

#[path = "../tests/common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use bulkhead_bundle::manifest::{Capability, NativeProgram};
use bulkhead_host::{
    Envelope, InstanceHandle, InstanceSpec, NativeCode, NativeConfig, Runtime, RuntimeConfig,
    SqlDatasource,
};

const LONG: Duration = Duration::from_secs(60);

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .map_or(5_000, |a| a.parse().expect("operations: a number"));
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
        .block_on(run(n));
}

/// Release builds of `bulkhead-runner`, the native fixture and
/// `bulkhead-sql`.
fn programs() -> (PathBuf, PathBuf, PathBuf) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut build = Command::new(cargo);
    build.current_dir(&root).args([
        "build",
        "--release",
        "-p",
        "bulkhead-native",
        "-p",
        "bulkhead-fixture-native",
    ]);
    if root.join("crates/bulkhead-sql").is_dir() {
        build.args(["-p", "bulkhead-sql"]);
    }
    assert!(build.status().unwrap().success());
    let release = root.join("target/release");
    (
        release.join("bulkhead-runner"),
        release.join("bulkhead-fixture-native"),
        release.join("bulkhead-sql"),
    )
}

/// User and system seconds of this process and its children.
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

async fn ask(instance: &InstanceHandle, command: &str) -> String {
    let reply = instance
        .call(Envelope::new("text/plain", command), LONG)
        .await
        .unwrap();
    String::from_utf8(reply.body).unwrap()
}

/// One operation: its commands in turn, each answered without an error.
async fn op(instance: &InstanceHandle, commands: &[&str]) -> Duration {
    let t = Instant::now();
    for c in commands {
        let got = ask(instance, c).await;
        assert!(!got.starts_with("err:"), "{c}: {got}");
    }
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

/// `n` operations of `commands`, one at a time.
async fn row(label: &str, n: usize, instance: &InstanceHandle, commands: &[&str]) {
    let cpu = cpu_seconds();
    let start = Instant::now();
    let mut took = Vec::with_capacity(n);
    for _ in 0..n {
        took.push(op(instance, commands).await);
    }
    print_row(label, took, cpu_seconds() - cpu, start.elapsed());
}

/// `n` operations of `commands` from each of `instances` at once.
async fn row_together(label: &str, n: usize, instances: &[InstanceHandle], commands: &[&str]) {
    let cpu = cpu_seconds();
    let start = Instant::now();
    let tasks: Vec<_> = instances
        .iter()
        .cloned()
        .map(|i| {
            let commands: Vec<String> = commands.iter().map(ToString::to_string).collect();
            tokio::spawn(async move {
                let commands: Vec<&str> = commands.iter().map(String::as_str).collect();
                let mut took = Vec::with_capacity(n);
                for _ in 0..n {
                    took.push(op(&i, &commands).await);
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

fn spec(cap: u32) -> InstanceSpec {
    let mut s = InstanceSpec {
        memory_max: 64 << 20,
        callback_deadline: LONG,
        init_timeout: Duration::from_secs(5),
        ..InstanceSpec::new("bench", "1.0.0")
    };
    s.granted.insert(Capability::Sql);
    s.services.sql.datasources = BTreeMap::from([("main".to_string(), cap)]);
    s
}

async fn run(n: usize) {
    let (runner, fixture, sql) = programs();
    let pg = common::pg::Scratch::start().expect("Postgres (initdb, pg_ctl) installed");
    let mut ds = SqlDatasource::new(
        pg.socket_dir().display().to_string(),
        "postgres",
        "bulkhead",
    );
    ds.port = common::pg::PORT;
    ds.pool_size = 16;
    let datasources = BTreeMap::from([("main".to_string(), ds)]);
    let mut native = NativeConfig::default();
    native.runner = Some(runner);
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 64,
        native,
        datasources: datasources.clone(),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let _ = &sql;
    // DA3: the datasource's driver.
    let _apps = common::sql_drivers_from(&rt, &datasources, &sql).await;

    let app = rt.load_native(NativeCode {
        path: fixture,
        kind: NativeProgram::Executable,
    });
    let one = rt.spawn(&app, spec(16)).unwrap();
    let many: Vec<InstanceHandle> = (0..16).map(|_| rt.spawn(&app, spec(16)).unwrap()).collect();
    for c in [
        "sql-x main CREATE TABLE IF NOT EXISTS bench (id int PRIMARY KEY, v bigint)",
        "sql-x main INSERT INTO bench VALUES (1, 0) ON CONFLICT DO NOTHING",
    ] {
        let got = ask(&one, c).await;
        assert!(!got.starts_with("err:"), "{c}: {got}");
    }
    // Warm: the pool's connections open, each instance started.
    for i in &many {
        op(i, &["sql-q main SELECT 1"]).await;
    }

    println!(
        "operations: {n} a row (16 instances: {n} each); CPU of this process and its children \
         (not Postgres); {} cores",
        std::thread::available_parallelism().map_or(0, std::num::NonZero::get)
    );
    println!();
    println!("| operation | p50 µs | p99 µs | CPU µs/op | ops/s |");
    println!("| --- | --- | --- | --- | --- |");
    let update = "sql-x main UPDATE bench SET v = v + 1 WHERE id = 1";
    row("query `SELECT 1`", n, &one, &["sql-q main SELECT 1"]).await;
    row("execute: `UPDATE` one row", n, &one, &[update]).await;
    row(
        "transaction: begin, `UPDATE`, commit",
        n,
        &one,
        &[
            "sql-begin main",
            "sql-tx UPDATE bench SET v = v + 1 WHERE id = 1",
            "sql-commit",
        ],
    )
    .await;
    row_together(
        "16 instances at once, `SELECT 1`",
        n,
        &many,
        &["sql-q main SELECT 1"],
    )
    .await;
    one.kill();
    for i in many {
        i.kill();
    }
}
