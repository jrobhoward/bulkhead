//! The driver run as the node runs it, on a runtime of the test's own:
//! `bulkhead-resolve` built and started as the app of that name with the
//! node's manifest for it (TCP to the nameservers alone, messages from
//! itself), the runtime's names resolved through it; and a DNS server
//! over TCP answering what each test sets.

#![allow(dead_code)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bulkhead_bundle::manifest::{
    AppInfo, AppManifest, AppSpec, NativeProgram, ProgramRef, Provenance,
};
use bulkhead_host::{AppDeployment, Apps, NativeCode, Resolver, Runtime, RuntimeConfig};
use hickory_resolver::proto::op::{Message, MessageType, OpCode, ResponseCode};
use hickory_resolver::proto::rr::rdata::{A, AAAA};
use hickory_resolver::proto::rr::{RData, Record, RecordType};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The driver's app name, as the node gives it.
pub const RESOLVE: &str = "bulkhead-resolve";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// `bulkhead-runner` and `bulkhead-resolve`, built on first use where the
/// node's tests build them.
pub fn programs() -> &'static (PathBuf, PathBuf) {
    static BUILT: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    BUILT.get_or_init(|| {
        let native = workspace_root().join("target/fixtures/native");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .current_dir(workspace_root())
            .args(["build", "-p", "bulkhead-native", "-p", "bulkhead-resolve"])
            .arg("--target-dir")
            .arg(&native)
            .status()
            .unwrap();
        assert!(status.success(), "building the runner and resolver failed");
        (
            native.join("debug/bulkhead-runner"),
            native.join("debug/bulkhead-resolve"),
        )
    })
}

/// A DNS server over TCP: each name's addresses, NXDOMAIN for the rest,
/// every query counted, answers held back by `delay`.
#[derive(Clone)]
pub struct Dns {
    pub addr: SocketAddr,
    records: Arc<Mutex<HashMap<String, Vec<IpAddr>>>>,
    queries: Arc<AtomicUsize>,
    delay: Arc<Mutex<Duration>>,
    /// Close a connection idle this long; none if zero.
    idle: Arc<Mutex<Duration>>,
}

impl Dns {
    /// Serve `records` (`name`, addresses) on a free loopback port.
    pub async fn start(records: &[(&str, &[&str])]) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dns = Self {
            addr: listener.local_addr().unwrap(),
            records: Arc::new(Mutex::new(
                records
                    .iter()
                    .map(|(n, ips)| {
                        (
                            n.to_ascii_lowercase(),
                            ips.iter().map(|ip| ip.parse().unwrap()).collect(),
                        )
                    })
                    .collect(),
            )),
            queries: Arc::default(),
            delay: Arc::default(),
            idle: Arc::default(),
        };
        let served = dns.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(served.clone().connection(stream));
            }
        });
        dns
    }

    /// Queries answered or held so far.
    pub fn queries(&self) -> usize {
        self.queries.load(Ordering::SeqCst)
    }

    /// Close a connection idle for `after` from now on, as servers end
    /// idle TCP connections (RFC 7766).
    pub fn close_when_idle(&self, after: Duration) {
        *self.idle.lock().unwrap() = after;
    }

    /// Hold each answer back this long from now on.
    pub fn delay(&self, by: Duration) {
        *self.delay.lock().unwrap() = by;
    }

    async fn connection(self, mut stream: tokio::net::TcpStream) {
        loop {
            let mut len = [0u8; 2];
            let idle = *self.idle.lock().unwrap();
            if idle.is_zero() {
                if stream.read_exact(&mut len).await.is_err() {
                    return;
                }
            } else if !matches!(
                tokio::time::timeout(idle, stream.read_exact(&mut len)).await,
                Ok(Ok(_))
            ) {
                return;
            }
            let mut buf = vec![0u8; usize::from(u16::from_be_bytes(len))];
            if stream.read_exact(&mut buf).await.is_err() {
                return;
            }
            self.queries.fetch_add(1, Ordering::SeqCst);
            let Ok(request) = Message::from_vec(&buf) else {
                return;
            };
            let delay = *self.delay.lock().unwrap();
            tokio::time::sleep(delay).await;
            let answer = self.answer(&request).to_vec().unwrap();
            let len = u16::try_from(answer.len()).unwrap().to_be_bytes();
            if stream.write_all(&len).await.is_err() || stream.write_all(&answer).await.is_err() {
                return;
            }
        }
    }

    fn answer(&self, request: &Message) -> Message {
        let mut response = Message::new(request.metadata.id, MessageType::Response, OpCode::Query);
        response.metadata.recursion_desired = request.metadata.recursion_desired;
        response.metadata.recursion_available = true;
        let records = self.records.lock().unwrap();
        for query in &request.queries {
            response.add_query(query.clone());
            let name = query.name().to_ascii().to_ascii_lowercase();
            let Some(ips) = records.get(name.trim_end_matches('.')) else {
                response.metadata.response_code = ResponseCode::NXDomain;
                continue;
            };
            for ip in ips {
                let rdata = match (ip, query.query_type()) {
                    (IpAddr::V4(v4), RecordType::A) => RData::A(A(*v4)),
                    (IpAddr::V6(v6), RecordType::AAAA) => RData::AAAA(AAAA(*v6)),
                    _ => continue,
                };
                response.add_answer(Record::from_rdata(query.name().clone(), 300, rdata));
            }
        }
        response
    }
}

/// The node's manifest for the driver (`node/daemon/resolve.rs`), with
/// `args` as its init arguments and TCP to `nameservers`.
pub fn manifest(args: &Value, nameservers: &[SocketAddr]) -> AppManifest {
    let connect: Vec<Value> = nameservers
        .iter()
        .map(|a| {
            let bits = if a.is_ipv4() { 32 } else { 128 };
            json!({ "cidr": format!("{}/{bits}", a.ip()), "ports": a.port().to_string() })
        })
        .collect();
    let spec = json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "instances": { "count": 1 },
        "lifecycle": { "start_args": args },
        "supervision": { "intensity": 100, "backoff": { "initial_ms": 1, "max_ms": 50 } },
        "limits": { "memory_max": "64MiB" },
        "messaging_acl": { "accept_from": [RESOLVE], "may_call": [RESOLVE] },
        "capabilities": {
            "bulkhead:sockets": { "connect": connect, "max_sockets": 16 },
            "bulkhead:metrics": { "prefix": "bulkhead_resolve" },
        },
    });
    AppManifest {
        app: AppInfo {
            name: RESOLVE.into(),
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
        spec: AppSpec::from_json_value(&spec).unwrap(),
    }
}

/// A runtime whose names go to the driver, and the driver running.
pub struct Served {
    pub rt: Runtime,
    pub apps: Apps,
}

/// The driver's init arguments.
pub fn args(nameservers: &[SocketAddr], resolv_conf: &str, hosts: &str) -> Value {
    json!({
        "nameservers": nameservers.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "resolv_conf": resolv_conf,
        "hosts": hosts,
    })
}

/// Start a runtime resolving through the driver, the driver given
/// `nameservers`, `resolv_conf` and `hosts`. Call from a multi-threaded
/// tokio runtime.
pub async fn serve(nameservers: &[SocketAddr], resolv_conf: &str, hosts: &str) -> Served {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let (runner, resolve) = programs();
    let mut native = bulkhead_host::NativeConfig::default();
    native.runner = Some(runner.clone());
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        native,
        resolver: Resolver::App(RESOLVE.into()),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let apps = rt.start_apps(Vec::new()).await.unwrap();
    start(&rt, &apps, resolve, nameservers, resolv_conf, hosts).await;
    Served { rt, apps }
}

/// Start the driver on `rt`.
pub async fn start(
    rt: &Runtime,
    apps: &Apps,
    program: &Path,
    nameservers: &[SocketAddr],
    resolv_conf: &str,
    hosts: &str,
) {
    let code = rt.load_native(NativeCode {
        path: program.to_path_buf(),
        kind: NativeProgram::Executable,
    });
    let manifest = manifest(&args(nameservers, resolv_conf, hosts), nameservers);
    apps.add(AppDeployment::new(manifest, code)).await.unwrap();
}

/// The driver's counter `bulkhead_resolve_<name>`, once reported, else 0.
pub fn counter(rt: &Runtime, name: &str) -> u64 {
    let wanted = format!("bulkhead_resolve_{name}{{");
    rt.render_metrics()
        .lines()
        .filter(|l| l.starts_with(&wanted))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .map(|v| v as u64)
        .sum()
}
