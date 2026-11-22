//! The Modbus drivers run as the node runs them (ROADMAP DA5), on a runtime
//! of their own: `bulkhead-modbus` polling a fake device over connections
//! the node opens, `bulkhead-modbus-server` serving masters on a listener
//! the node bound, and an app (`bulkhead-host`'s native fixture) receiving
//! their updates and calling their functions through the node.

#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bulkhead_bundle::manifest::{
    AppInfo, AppManifest, AppSpec, NativeProgram, ProgramRef, Provenance,
};
use bulkhead_host::services::modbus::{MODBUS_DEVICES_APP, MODBUS_SERVER_APP};
use bulkhead_host::{
    AppDeployment, Apps, DriverEndpoint, DriverListener, DriverProfile, Envelope, InstanceHandle,
    ModbusDrivers, NativeCode, NativeConfig, Runtime, RuntimeConfig,
};
use bulkhead_modbus::PACKAGE;
use serde_json::json;
use tokio_modbus::client::{Reader, Writer};
use tokio_modbus::prelude::Slave;

/// Built into `target/fixtures/native`: the runner, the native fixture
/// and the two drivers.
struct Programs {
    runner: PathBuf,
    fixture: PathBuf,
    devices: PathBuf,
    server: PathBuf,
}

fn programs() -> Programs {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target = root.join("target/fixtures/native");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(&root)
        .args([
            "build",
            "-p",
            "bulkhead-runner",
            "-p",
            "bulkhead-fixture-native",
        ])
        .args(["-p", "bulkhead-modbus", "--target-dir"])
        .arg(&target)
        .status()
        .unwrap();
    assert!(status.success(), "building the programs failed");
    let debug = target.join("debug");
    Programs {
        runner: debug.join("bulkhead-runner"),
        fixture: debug.join("bulkhead-fixture-native"),
        devices: debug.join("bulkhead-modbus"),
        server: debug.join("bulkhead-modbus-server"),
    }
}

/// A fake device: holding registers anyone may read and write.
async fn fake_device(registers: Vec<u16>) -> SocketAddr {
    use std::future::{Ready, ready};
    use tokio_modbus::server::tcp::{Server, accept_tcp_connection};
    use tokio_modbus::{ExceptionCode, Request, Response};

    #[derive(Clone)]
    struct Device(Arc<Mutex<Vec<u16>>>);
    impl tokio_modbus::server::Service for Device {
        type Request = Request<'static>;
        type Response = Response;
        type Exception = ExceptionCode;
        type Future = Ready<Result<Response, ExceptionCode>>;
        fn call(&self, request: Self::Request) -> Self::Future {
            let mut regs = self.0.lock().unwrap();
            ready(match request {
                Request::ReadHoldingRegisters(a, n) => regs
                    .get(usize::from(a)..usize::from(a) + usize::from(n))
                    .map(|r| Response::ReadHoldingRegisters(r.to_vec()))
                    .ok_or(ExceptionCode::IllegalDataAddress),
                Request::WriteMultipleRegisters(a, values) => {
                    for (i, v) in values.iter().enumerate() {
                        regs[usize::from(a) + i] = *v;
                    }
                    Ok(Response::WriteMultipleRegisters(a, values.len() as u16))
                }
                _ => Err(ExceptionCode::IllegalFunction),
            })
        }
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let device = Device(Arc::new(Mutex::new(registers)));
    tokio::spawn(async move {
        let on_connected = move |stream, addr| {
            let device = device.clone();
            async move { accept_tcp_connection(stream, addr, move |_| Ok(Some(device.clone()))) }
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

/// A driver's manifest, as the node makes it: one instance, messages from
/// itself alone, its part of the configuration as start arguments.
fn driver_manifest(name: &str, args: &serde_json::Value) -> AppManifest {
    manifest(
        name,
        &json!({
            "kind": "native-process",
            "native": { "program": "executable" },
            "instances": { "count": 1 },
            "lifecycle": { "start_args": args },
            "supervision": { "intensity": 100, "backoff": { "initial_ms": 1, "max_ms": 50 } },
            "limits": { "memory_max": "64MiB" },
            "messaging_acl": { "accept_from": [name], "may_call": [name] },
        }),
    )
}

/// A node running both drivers: device `pump-1` at `device` (polled every
/// 50 ms into `scada`) and `ghost` where nothing listens, and a server of 4
/// holding registers whose writes go to `scada`. Its master address.
struct Node {
    apps: Apps,
    master: SocketAddr,
}

async fn node(device: SocketAddr) -> Node {
    node_with(device, json!({ "holding": [{ "start": 0, "count": 2 }] })).await
}

/// [`node`] with `pump-1`'s polled ranges and tags from `pump`.
async fn node_with(device: SocketAddr, pump: serde_json::Value) -> Node {
    let p = programs();
    let ghost: SocketAddr = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let master = socket.local_addr().unwrap();
    let endpoint = |a: SocketAddr| DriverEndpoint::Tcp {
        host: a.ip().to_string(),
        port: a.port(),
        timeout: Duration::from_secs(1),
    };
    let mut drivers = ModbusDrivers::default();
    drivers.devices.insert("pump-1".into());
    drivers.devices.insert("ghost".into());
    drivers.server = true;
    drivers.timeout = Duration::from_secs(5);
    let mut native = NativeConfig::default();
    native.runner = Some(p.runner.clone());
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        native,
        modbus: Some(drivers),
        drivers: BTreeMap::from([
            (
                MODBUS_DEVICES_APP.to_string(),
                DriverProfile {
                    endpoints: vec![endpoint(device), endpoint(ghost)],
                    calls: vec!["scada".into()],
                    delivers: true,
                    ..DriverProfile::default()
                },
            ),
            (
                MODBUS_SERVER_APP.to_string(),
                DriverProfile {
                    listeners: vec![DriverListener {
                        name: bulkhead_modbus::LISTENER.into(),
                        socket: Arc::new(socket),
                    }],
                    calls: vec!["scada".into()],
                    delivers: true,
                    ..DriverProfile::default()
                },
            ),
        ]),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let load = |path: &PathBuf| {
        rt.load_native(NativeCode {
            path: path.clone(),
            kind: NativeProgram::Executable,
        })
    };
    let mut pump_1 = json!({
        "address": device.to_string(), "poll_ms": 50, "deliver_to": "scada",
    });
    for (k, v) in pump.as_object().unwrap() {
        pump_1[k] = v.clone();
    }
    let devices_args = json!({ "devices": {
        "pump-1": pump_1,
        "ghost": { "address": ghost.to_string(), "timeout_ms": 300 },
    } });
    let server_args = json!({ "server": {
        "listen": master.to_string(), "deliver_to": "scada", "holding": 4,
    } });
    let scada = json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "capabilities": { "extensions": { PACKAGE: {
            "read": ["pump-1", "ghost"], "write": ["pump-1"], "serve": true,
        } } },
        "limits": { "callback_deadline_ms": 5000, "memory_max": "64MiB" },
    });
    let apps = rt
        .start_apps(vec![
            AppDeployment::new(manifest("scada", &scada), load(&p.fixture)),
            AppDeployment::new(
                driver_manifest(MODBUS_DEVICES_APP, &devices_args),
                load(&p.devices),
            ),
            AppDeployment::new(
                driver_manifest(MODBUS_SERVER_APP, &server_args),
                load(&p.server),
            ),
        ])
        .await
        .unwrap();
    Node { apps, master }
}

impl Node {
    fn scada(&self) -> InstanceHandle {
        self.apps.app("scada").unwrap().instances().remove(0)
    }

    async fn ask(&self, command: &str) -> String {
        let reply = self
            .scada()
            .call(
                Envelope::new("text/plain", command),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        String::from_utf8(reply.body).unwrap()
    }

    async fn call(&self, function: &str, args: &str) -> String {
        self.ask(&format!("ext {PACKAGE} {function} {args}")).await
    }

    /// The Modbus updates `scada` received, decoded.
    async fn updates(&self) -> Vec<bulkhead_modbus::points::Update> {
        self.ask("cbor-casts")
            .await
            .lines()
            .filter(|l| !l.is_empty())
            .map(|hex| {
                let bytes: Vec<u8> = (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                    .collect();
                ciborium::from_reader(bytes.as_slice()).unwrap()
            })
            .collect()
    }

    /// The casts `scada` received, once one contains `text`.
    async fn received(&self, text: &str) -> String {
        for _ in 0..100 {
            let infos = self.ask("infos").await;
            if infos.contains(text) {
                return infos;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("scada never received {text}: {}", self.ask("infos").await);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn devices____polled____updates_cast_to_the_deliver_to_app() {
    let device = fake_device(vec![7, 8, 9, 10]).await;
    let n = node(device).await;

    let infos = n.received("pump-1").await;

    assert!(infos.contains("cast"), "{infos}");
    n.apps.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client____reads_and_writes____through_the_node_to_the_device() {
    let device = fake_device(vec![7, 8, 9, 10]).await;
    let n = node(device).await;

    let read = n.call("client#read-holding", r#"["pump-1", 0, 2]"#).await;
    let wrote = n
        .call("client#write-holding", r#"["pump-1", 1, [42]]"#)
        .await;
    let again = n.call("client#read-holding", r#"["pump-1", 0, 2]"#).await;
    let ghost = n.call("client#read-holding", r#"["ghost", 0, 1]"#).await;
    let denied = n.call("client#write-holding", r#"["ghost", 0, [1]]"#).await;

    assert_eq!(read, r#"{"Ok":[7,8]}"#);
    assert_eq!(wrote, r#"{"Ok":null}"#);
    assert_eq!(again, r#"{"Ok":[7,42]}"#);
    assert!(ghost.starts_with(r#"{"Err":{"Offline""#), "{ghost}");
    assert_eq!(denied, r#"{"Err":"AccessDenied"}"#);
    n.apps.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server____master_and_app____share_the_tables_and_writes_are_delivered() {
    let device = fake_device(vec![0; 4]).await;
    let n = node(device).await;
    let set = n.call("server#set-holding", "[0, [5, 6]]").await;
    let mut master = None;
    for _ in 0..100 {
        if let Ok(c) = tokio_modbus::client::tcp::connect_slave(n.master, Slave(1)).await {
            master = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut master = master.expect("the server accepts");

    let read = master.read_holding_registers(0, 2).await.unwrap().unwrap();
    master
        .write_multiple_registers(2, &[77])
        .await
        .unwrap()
        .unwrap();
    let got = n.call("server#get-holding", "[0, 4]").await;
    let infos = n.received("server").await;

    assert_eq!(set, r#"{"Ok":null}"#);
    assert_eq!(read, vec![5, 6]);
    assert_eq!(got, r#"{"Ok":[5,6,77,0]}"#);
    assert!(infos.contains("cast"), "{infos}");
    n.apps.stop().await;
}

/// Tags (ROADMAP DA5b): an f32 across registers 2 and 3, word-swapped, and
/// a scaled, deadbanded u16 at register 0, delivered as `tags`; only the
/// untagged register 1 comes as a raw point. A change inside the deadband
/// sends nothing; one past it sends the tag.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn devices____tags____typed_scaled_and_deadbanded_in_place_of_their_registers() {
    use bulkhead_modbus::points::{Point, Table, TagValue};
    let device = fake_device(vec![7, 8, 0x0000, 0x4148]).await;
    let n = node_with(
        device,
        json!({
            "holding": [{ "start": 0, "count": 4 }],
            "tags": {
                "flow": { "table": "holding", "address": 2, "type": "f32", "order": "cdab" },
                "level": { "table": "holding", "address": 0, "scale": 0.5, "deadband": 10.0 },
            },
        }),
    )
    .await;
    n.received("flow").await;

    let first = n.updates().await.remove(0);
    let inside = n
        .call("client#write-holding", r#"["pump-1", 0, [9]]"#)
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after_inside = n.updates().await.len();
    let past = n
        .call("client#write-holding", r#"["pump-1", 0, [40]]"#)
        .await;
    let mut level = None;
    for _ in 0..100 {
        level = n
            .updates()
            .await
            .iter()
            .skip(after_inside)
            .find_map(|u| u.tags.iter().find(|t| t.name == "level").map(|t| t.value));
        if level.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let tag = |name: &str| first.tags.iter().find(|t| t.name == name).map(|t| t.value);
    assert_eq!(
        first.points,
        [Point {
            table: Table::Holding,
            address: 1,
            value: 8
        }]
    );
    assert_eq!(tag("flow"), Some(TagValue::Float(12.5)));
    assert_eq!(tag("level"), Some(TagValue::Float(3.5)));
    assert_eq!(
        (inside.as_str(), past.as_str()),
        (r#"{"Ok":null}"#, r#"{"Ok":null}"#)
    );
    assert_eq!(
        after_inside, 1,
        "a change inside the deadband sent an update"
    );
    assert_eq!(level, Some(TagValue::Float(20.0)));
    n.apps.stop().await;
}
