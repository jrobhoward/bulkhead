//! Modbus point throughput, for `docs/BASELINE.md` (ROADMAP DA5): fake
//! Modbus TCP devices in this process whose registers all change on every
//! read (the worst case for report by exception), polled over loopback,
//! each scan delivered to `examples/scada/points`, a hash-routed pool that
//! writes every update into a shared `memory` bucket in one batch.
//!
//! It reports polls asked a second, updates stored a second (the pool's
//! cast callbacks, from the node's counters), their ratio, updates its full
//! mailboxes dropped a second, points a second, and CPU per update of this process and its
//! children (the node's side, the pool, and since DA5 the driver; the fake
//! devices are in this process too). Before DA5 the node polled through the
//! `bulkhead-modbus:modbus` extension: replace the block marked `DA5` with
//! the one in its comment to run it on a tree from before (commit
//! 3571010).
//!
//! ```sh
//! cargo run --release -p bulkhead-modbus --example modbus_load -- DEVICES REGISTERS POLL_MS POOL SECONDS
//! ```
//!
//! Run on an idle machine.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    missing_docs
)]

use std::collections::BTreeMap;
use std::future::{Ready, ready};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use bulkhead_bundle::manifest::{
    AppInfo, AppManifest, AppSpec, NativeProgram, ProgramRef, Provenance,
};
use bulkhead_host::{
    AppDeployment, NativeCode, NativeConfig, Runtime, RuntimeConfig, SharedBucket,
};
use serde_json::json;
use tokio_modbus::server::tcp::{Server, accept_tcp_connection};
use tokio_modbus::{ExceptionCode, Request, Response};

fn arg(i: usize, default: u64) -> u64 {
    std::env::args()
        .nth(i)
        .map_or(default, |a| a.parse().expect("a number"))
}

fn main() {
    let devices = arg(1, 16) as usize;
    let registers = arg(2, 100) as u16;
    let poll_ms = arg(3, 10);
    let pool = arg(4, 4) as u32;
    let seconds = arg(5, 10);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
        .block_on(run(devices, registers, poll_ms, pool, seconds));
}

/// Release builds of the runner, the points app and (where the tree has
/// them) the Modbus drivers.
fn programs() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut build = Command::new(&cargo);
    build.current_dir(&root).args([
        "build",
        "--release",
        "-p",
        "bulkhead-runner",
        "-p",
        "example-scada-points",
    ]);
    assert!(build.status().unwrap().success());
    if root.join("crates/bulkhead-modbus/src/bin").is_dir() {
        let drivers = Command::new(&cargo)
            .current_dir(&root)
            .args(["build", "--release", "-p", "bulkhead-modbus", "--bins"])
            .status()
            .unwrap();
        assert!(drivers.success());
    }
    root.join("target/release")
}

/// A fake device whose `registers` holding registers all change on every
/// read.
async fn device(registers: u16) -> SocketAddr {
    #[derive(Clone)]
    struct Changing(Arc<AtomicU16>, u16);
    impl tokio_modbus::server::Service for Changing {
        type Request = Request<'static>;
        type Response = Response;
        type Exception = ExceptionCode;
        type Future = Ready<Result<Response, ExceptionCode>>;
        fn call(&self, request: Self::Request) -> Self::Future {
            ready(match request {
                Request::ReadHoldingRegisters(_, n) if n <= self.1 => {
                    let base = self.0.fetch_add(1, Ordering::Relaxed);
                    Ok(Response::ReadHoldingRegisters(
                        (0..n).map(|i| base.wrapping_add(i)).collect(),
                    ))
                }
                _ => Err(ExceptionCode::IllegalDataAddress),
            })
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let service = Changing(Arc::new(AtomicU16::new(0)), registers);
    tokio::spawn(async move {
        let on_connected = move |stream, addr| {
            let service = service.clone();
            async move { accept_tcp_connection(stream, addr, move |_| Ok(Some(service.clone()))) }
        };
        let _ = Server::new(listener).serve(&on_connected, |_e| {}).await;
    });
    address
}

fn manifest(name: &str, spec: &serde_json::Value) -> AppManifest {
    AppManifest {
        app: AppInfo {
            name: name.into(),
            version: "1.0.0".parse().unwrap(),
            description: None,
            authors: vec![],
            license: None,
            repository: None,
        },
        program: ProgramRef {
            path: String::new(),
            checksum: String::new(),
        },
        provenance: Provenance {
            signer_key: String::new(),
            build_info: None,
            sbom: None,
        },
        spec: AppSpec::from_json_value(spec).unwrap(),
    }
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

/// Updates the pool handled (its cast callbacks, each storing one update)
/// and casts its full mailboxes dropped, from the node's counters: asking
/// the pool itself would queue behind the updates.
fn handled(rt: &Runtime) -> (u64, u64) {
    let text = rt.render_metrics();
    let value = |prefix: &str| -> u64 {
        text.lines()
            .filter(|l| l.starts_with(prefix))
            .filter_map(|l| l.rsplit(' ').next()?.parse::<u64>().ok())
            .sum()
    };
    (
        value("bulkhead_callbacks_total{app=\"points\",kind=\"cast\"}"),
        value("bulkhead_mailbox_dropped_total{app=\"points\"}"),
    )
}

#[allow(clippy::too_many_lines)]
async fn run(devices: usize, registers: u16, poll_ms: u64, pool: u32, seconds: u64) {
    let release = programs();
    let mut addresses = Vec::new();
    for _ in 0..devices {
        addresses.push(device(registers).await);
    }
    let config: serde_json::Map<String, serde_json::Value> = addresses
        .iter()
        .enumerate()
        .map(|(i, a)| {
            (
                format!("dev-{i}"),
                json!({
                    "address": a.to_string(), "poll_ms": poll_ms, "timeout_ms": 1000,
                    "deliver_to": "points", "holding": [{ "start": 0, "count": registers }],
                }),
            )
        })
        .collect();
    let mut native = NativeConfig::default();
    native.runner = Some(release.join("bulkhead-runner"));
    let mut runtime = RuntimeConfig {
        max_instances: 64,
        native,
        buckets: BTreeMap::from([(
            "points".to_string(),
            SharedBucket {
                durability: bulkhead_bundle::manifest::Durability::Memory,
                quota: None,
                read_concurrency: false,
                publish: None,
            },
        )]),
        ..RuntimeConfig::default()
    };
    let points = json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "instances": { "count": pool, "routing": "hash:device" },
        "capabilities": {
            "bulkhead:logging": {},
            "bulkhead:keyvalue": { "shared": { "points": "rw" } },
        },
        "limits": { "memory_max": "128MiB" },
    });
    let load = |rt: &Runtime, name: &str| {
        rt.load_native(NativeCode {
            path: release.join(name),
            kind: NativeProgram::Executable,
        })
    };

    // DA5: the devices' driver, its connections opened by the node.
    // Before DA5, in place of this block:
    //   let rt = Runtime::with_extensions(runtime, vec![std::sync::Arc::new(
    //       bulkhead_modbus::Modbus::default())]).unwrap();
    //   let apps = rt.start_apps(vec![AppDeployment::new(
    //       manifest("points", &points), load(&rt, "example-scada-points"))]).await.unwrap();
    //   let _started = rt.start_extensions(&apps, &BTreeMap::from([(
    //       bulkhead_modbus::PACKAGE.to_string(), json!({ "devices": config }))])).unwrap();
    let mut modbus = bulkhead_host::ModbusDrivers::default();
    modbus.devices.extend(config.keys().cloned());
    modbus.timeout = Duration::from_secs(5);
    runtime.modbus = Some(modbus);
    runtime.drivers = BTreeMap::from([(
        "bulkhead-modbus".to_string(),
        bulkhead_host::DriverProfile {
            endpoints: addresses
                .iter()
                .map(|a| bulkhead_host::DriverEndpoint::Tcp {
                    host: a.ip().to_string(),
                    port: a.port(),
                    timeout: Duration::from_secs(1),
                })
                .collect(),
            calls: vec!["points".into()],
            delivers: true,
            ..bulkhead_host::DriverProfile::default()
        },
    )]);
    let rt = Runtime::new(runtime).unwrap();
    let apps = rt
        .start_apps(vec![AppDeployment::new(
            manifest("points", &points),
            load(&rt, "example-scada-points"),
        )])
        .await
        .unwrap();
    let driver = manifest(
        "bulkhead-modbus",
        &json!({
            "kind": "native-process",
            "native": { "program": "executable" },
            "lifecycle": { "start_args": { "devices": config } },
            "limits": { "memory_max": "256MiB" },
            "messaging_acl": { "accept_from": ["bulkhead-modbus"], "may_call": ["bulkhead-modbus"] },
        }),
    );
    apps.add(AppDeployment::new(driver, load(&rt, "bulkhead-modbus")))
        .await
        .unwrap();
    // End of DA5.

    // Warm: every device connected and delivering.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let ((n0, d0), cpu0, t0) = (handled(&rt), cpu_seconds(), Instant::now());
    tokio::time::sleep(Duration::from_secs(seconds)).await;
    let ((n1, d1), cpu1, wall) = (handled(&rt), cpu_seconds(), t0.elapsed());
    let updates = n1.saturating_sub(n0) as f64;
    let dropped = d1.saturating_sub(d0) as f64 / wall.as_secs_f64();
    let asked = devices as f64 * 1000.0 / poll_ms as f64;
    let per_s = updates / wall.as_secs_f64();
    println!(
        "| {devices} | {registers} | {poll_ms} | {pool} | {asked:.0} | {per_s:.0} | {:.0}% | {dropped:.0} | {:.0} | {:.0} |",
        per_s / asked * 100.0,
        per_s * f64::from(registers),
        (cpu1 - cpu0) / updates.max(1.0) * 1e6,
    );
}
