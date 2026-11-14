//! The driver run as the node runs it, on a runtime of the test's own:
//! `bulkhead-ingress` built and started as the app of that name with a
//! driver profile (listening sockets bound here, its certificate files,
//! `calls_routed`, the routes event, forwarding), the SDK fixture as the
//! apps it calls, the route table set through `RouteTable`.

#![allow(dead_code)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bulkhead_bundle::manifest::{
    AppInfo, AppManifest, AppSpec, NativeProgram, ProgramRef, Provenance,
};
use bulkhead_host::routes::{HttpRoute, RouteTable};
use bulkhead_host::{
    AppDeployment, Apps, DriverListener, DriverProfile, Elsewhere, LoadedApp, NativeCode, Runtime,
    RuntimeConfig,
};
use serde_json::{Value, json};

/// The driver's app name, as the node gives it.
pub const INGRESS: &str = "bulkhead-ingress";

fn workspace_root() -> PathBuf {
    // Canonical: a program's path names no crate (`restart.rs` finds the
    // driver by its path).
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn cargo_build(target: &Path, args: &[&str]) {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(workspace_root())
        .arg("build")
        .args(args)
        .arg("--target-dir")
        .arg(target)
        .status()
        .unwrap();
    assert!(status.success(), "cargo build {args:?} failed");
}

/// `bulkhead-runner` and `bulkhead-ingress`, built on first use where the
/// node's tests build them, and the SDK fixture where `bulkhead-host`'s
/// tests do.
pub fn programs() -> &'static (PathBuf, PathBuf, PathBuf) {
    static BUILT: OnceLock<(PathBuf, PathBuf, PathBuf)> = OnceLock::new();
    BUILT.get_or_init(|| {
        let native = workspace_root().join("target/fixtures/native");
        cargo_build(
            &native,
            &["-p", "bulkhead-native", "-p", "bulkhead-ingress"],
        );
        let sdk = workspace_root().join("target/fixtures/native-sdk");
        cargo_build(
            &sdk,
            &[
                "-p",
                "bulkhead-fixture-sdk",
                "--bin",
                "bulkhead-fixture-sdk",
            ],
        );
        (
            native.join("debug/bulkhead-runner"),
            native.join("debug/bulkhead-ingress"),
            sdk.join("debug/bulkhead-fixture-sdk"),
        )
    })
}

/// A manifest for `name` with `appserver` as its spec.
pub fn manifest(name: &str, appserver: &Value) -> AppManifest {
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
        spec: AppSpec::from_json_value(appserver).unwrap(),
    }
}

/// One listener: its name and its init arguments beyond the name (limits,
/// `standby`, `tls`, `tls_reload_s`, as the node writes them).
pub struct Listen {
    pub name: &'static str,
    pub args: Value,
    /// Certificate and key files to give the driver under `/driver/`.
    pub files: Vec<PathBuf>,
}

impl Listen {
    pub fn plain(name: &'static str) -> Self {
        Self {
            name,
            args: json!({}),
            files: Vec::new(),
        }
    }

    pub fn with(name: &'static str, args: Value) -> Self {
        Self {
            name,
            args,
            files: Vec::new(),
        }
    }
}

/// A runtime running the driver, and the apps it serves.
pub struct Served {
    pub rt: Runtime,
    pub apps: Apps,
    pub routes: RouteTable,
    /// Where each listener listens, by name.
    pub addresses: BTreeMap<String, SocketAddr>,
    /// The SDK fixture, to start apps from.
    pub sdk: LoadedApp,
    /// Kept: the runtime holds it weakly.
    _elsewhere: Elsewhere,
}

impl Served {
    pub fn address(&self, listener: &str) -> SocketAddr {
        self.addresses[listener]
    }

    /// Start `name` from the SDK fixture with `appserver` as its spec.
    pub async fn start_app(&self, name: &str, appserver: &Value) {
        self.apps
            .add(AppDeployment::new(
                manifest(name, appserver),
                self.sdk.clone(),
            ))
            .await
            .unwrap();
    }

    /// The driver's counter `bulkhead_http_<name>`, once it has reported
    /// one (each second), else 0.
    pub fn counter(&self, name: &str) -> u64 {
        let wanted = format!("bulkhead_http_{name}{{");
        self.rt
            .render_metrics()
            .lines()
            .filter(|l| l.starts_with(&wanted) && l.contains(&format!("app=\"{INGRESS}\"")))
            .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
            .map(|v| v as u64)
            .sum()
    }
}

/// Bind `listeners`, start a runtime whose app [`INGRESS`] is a routed,
/// forwarding driver, and start the driver. `elsewhere` says where apps
/// that do not run here run. Call from a multi-threaded tokio runtime.
pub async fn serve(
    listeners: Vec<Listen>,
    elsewhere: impl Fn(&str) -> Option<u32> + Send + Sync + 'static,
) -> Served {
    // `RUST_LOG=debug` shows the runtime's records, the driver's among
    // them.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let (runner, ingress, sdk) = programs();
    let mut sockets = Vec::new();
    let mut files = Vec::new();
    let mut args = Vec::new();
    let mut addresses = BTreeMap::new();
    for l in listeners {
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        addresses.insert(l.name.to_string(), socket.local_addr().unwrap());
        sockets.push(DriverListener {
            name: l.name.into(),
            socket: Arc::new(socket),
        });
        files.extend(l.files);
        let mut a = l.args;
        a["name"] = json!(l.name);
        args.push(a);
    }
    let mut native = bulkhead_host::NativeConfig::default();
    native.runner = Some(runner.clone());
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 32,
        native,
        drivers: BTreeMap::from([(
            INGRESS.to_string(),
            DriverProfile {
                listeners: sockets,
                files,
                calls: Vec::new(),
                routes: true,
                calls_routed: true,
                forward: true,
            },
        )]),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let elsewhere: Elsewhere = Arc::new(elsewhere);
    rt.set_elsewhere(&elsewhere);
    let routes = RouteTable::new(rt.clone());
    let apps = rt.start_apps(Vec::new()).await.unwrap();
    let driver = rt.load_native(NativeCode {
        path: ingress.clone(),
        kind: NativeProgram::Executable,
    });
    // As the node's manifest for it (`ingress_manifest`).
    let spec = json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "instances": { "count": 1 },
        "lifecycle": { "start_args": { "threads": 2, "listeners": args } },
        "supervision": { "intensity": 100, "backoff": { "initial_ms": 1, "max_ms": 50 } },
        "limits": { "memory_max": "512MiB", "message_max_bytes": "16MiB" },
        "capabilities": { "bulkhead:metrics": { "prefix": "bulkhead_http" } },
    });
    apps.add(AppDeployment::new(manifest(INGRESS, &spec), driver))
        .await
        .unwrap();
    let sdk = rt.load_native(NativeCode {
        path: sdk.clone(),
        kind: NativeProgram::Executable,
    });
    Served {
        rt,
        apps,
        routes,
        addresses,
        sdk,
        _elsewhere: elsewhere,
    }
}

/// A route on `prefix`, any host, no hash header.
pub fn route(prefix: &str) -> HttpRoute {
    HttpRoute {
        host: None,
        path_prefix: prefix.into(),
        hash_header: None,
        max_body: 1024,
        timeout: Duration::from_secs(1),
    }
}

/// What a request got back.
pub struct Got {
    pub status: u16,
    /// The status line and headers, lower case.
    pub head: String,
    pub body: String,
}

/// One HTTP/1.1 request on its own connection.
pub async fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Got {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nconnection: close\r\ncontent-length: {}\r\n",
        body.len()
    );
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        req.push_str("host: localhost\r\n");
    }
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut raw = String::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_string(&mut raw))
        .await
        .expect("no answer in 10 s")
        .unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    Got {
        status,
        head: head.to_ascii_lowercase(),
        body: body.to_string(),
    }
}

/// Try `check` every 50 ms for up to 10 s; panic with `what` if it never
/// holds.
pub async fn until<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !check().await {
        assert!(tokio::time::Instant::now() < deadline, "never: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
