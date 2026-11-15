//! Driver apps (ROADMAP DA1): the profile node config gives an app by
//! name, beyond its manifest. Listening sockets the node binds and keeps,
//! on which the driver accepts itself; files it may read under /driver;
//! the apps it may call. The native fixture through `bulkhead-runner`.

#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bulkhead_bundle::manifest::NativeProgram;
use bulkhead_host::routes::{HttpRoute, RouteTable};
use bulkhead_host::{
    DriverListener, DriverProfile, InstanceHandle, InstanceSpec, LoadedApp, NativeCode, Runtime,
    RuntimeConfig,
};
use common::{ask, native_config, native_fixtures};

struct Node {
    rt: Runtime,
    code: LoadedApp,
    /// Where the driver's listener `main` listens.
    address: SocketAddr,
    /// The driver's file, and one beside it it was not given.
    file: PathBuf,
    other_file: PathBuf,
}

/// A runtime whose app `drv` is a driver: listener `main` on a port of
/// its own, one file, and `drv-target` the only app it may call.
fn node(test: &str) -> Node {
    node_with(test, false)
}

/// [`node`], with the driver asking for the HTTP route table or not.
fn node_with(test: &str, routes: bool) -> Node {
    node_profile(test, routes, false)
}

/// [`node_with`], with the driver calling the apps that own routes
/// (`calls_routed`) instead of `drv-target`.
fn node_profile(test: &str, routes: bool, calls_routed: bool) -> Node {
    let dir = std::env::temp_dir().join(format!("bulkhead-drivers-{test}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("shop.pem");
    let other_file = dir.join("other.pem");
    std::fs::write(&file, "the driver's certificate").unwrap();
    std::fs::write(&other_file, "not given").unwrap();
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        native: native_config(),
        drivers: BTreeMap::from([(
            "drv".to_string(),
            DriverProfile {
                listeners: vec![DriverListener {
                    name: "main".into(),
                    socket: Arc::new(socket),
                }],
                files: vec![file.clone()],
                calls: vec!["drv-target".into()],
                routes,
                calls_routed,
                ..DriverProfile::default()
            },
        )]),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let code = rt.load_native(NativeCode {
        path: native_fixtures().1.clone(),
        kind: NativeProgram::Executable,
    });
    Node {
        rt,
        code,
        address,
        file,
        other_file,
    }
}

fn up(n: &Node, app: &str) -> InstanceHandle {
    up_accepting(n, app, &[])
}

/// [`up`], accepting messages from the apps `accept_from` names only
/// (any app when empty).
fn up_accepting(n: &Node, app: &str, accept_from: &[&str]) -> InstanceHandle {
    let mut spec = InstanceSpec {
        memory_max: 64 << 20,
        callback_deadline: Duration::from_secs(5),
        init_timeout: Duration::from_secs(5),
        ..InstanceSpec::new(app, "1.0.0")
    };
    spec.acl.accept_from = accept_from.iter().map(ToString::to_string).collect();
    n.rt.spawn(&n.code, spec).unwrap()
}

/// Connect to `address` and read what the driver writes until it closes.
fn greeted(address: SocketAddr) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut s = TcpStream::connect(address).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut got = String::new();
        s.read_to_string(&mut got).unwrap();
        got
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver____client_connects____the_driver_accepts_on_its_own_listener() {
    let n = node("accept");
    let d = up(&n, "drv");

    let names = ask(&d, "listeners").await;
    let client = greeted(n.address);
    let peer = ask(&d, "accept main").await;
    let got = client.join().unwrap();

    assert_eq!(names, "main");
    assert!(!peer.starts_with("err:"), "{peer}");
    assert_eq!(got, "hello\n");
    d.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver____restarted____the_port_stays_and_a_waiting_client_is_served() {
    let n = node("restart");
    let first = up(&n, "drv");
    ask(&first, "listeners").await;
    first.kill();
    let _ = first.wait().await;

    let client = greeted(n.address);
    let second = up(&n, "drv");
    let peer = ask(&second, "accept main").await;
    let got = client.join().unwrap();

    assert!(!peer.starts_with("err:"), "{peer}");
    assert_eq!(got, "hello\n");
    second.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listeners____not_a_driver____none() {
    let n = node("plain");
    let plain = up(&n, "plain");

    let names = ask(&plain, "listeners").await;

    assert_eq!(names, "");
    plain.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accept____on_a_descriptor_not_passed____refused_for_drivers_and_apps() {
    let n = node("refused");
    let d = up(&n, "drv");
    let plain = up(&n, "plain");

    // FreeBSD: 6, the descriptor socket, open and without `ACCEPT`.
    let elsewhere = if cfg!(target_os = "freebsd") { 6 } else { 3 };
    let driver_elsewhere = ask(&d, &format!("accept-fd {elsewhere}")).await;
    let app_on_eight = ask(&plain, "accept-fd 8").await;

    // Linux: the seccomp filter; FreeBSD: the descriptor lacks the `ACCEPT`
    // right, and an app has no descriptor 8 at all.
    if cfg!(target_os = "freebsd") {
        assert!(
            driver_elsewhere.contains("Capabilities insufficient"),
            "{driver_elsewhere}"
        );
        assert!(app_on_eight.starts_with("err:"), "{app_on_eight}");
    } else {
        assert!(
            driver_elsewhere.contains("not permitted"),
            "{driver_elsewhere}"
        );
        assert!(app_on_eight.contains("not permitted"), "{app_on_eight}");
    }
    d.kill();
    plain.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver____its_files____readable_under_driver_and_others_beside_them_not() {
    let n = node("files");
    let d = up(&n, "drv");

    let given = ask(&d, "vcat /driver/shop.pem").await;
    let beside = ask(&d, &format!("read {}", n.other_file.display())).await;
    let direct = ask(&d, &format!("read {}", n.file.display())).await;

    assert_eq!(given, "the driver's certificate");
    assert!(beside.starts_with("err:"), "{beside}");
    // Linux: Landlock allows the file by its path; FreeBSD: capability
    // mode refuses every path, the driver's own file's included.
    if cfg!(target_os = "freebsd") {
        assert!(direct.contains("capability mode"), "{direct}");
    } else {
        assert_eq!(direct, "ok");
    }
    d.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver____calls____only_the_apps_its_profile_lists() {
    let n = node("calls");
    let d = up(&n, "drv");
    let target = up(&n, "drv-target");
    let other = up(&n, "drv-other");
    let target_pid = ask(&target, "self").await;
    let other_pid = ask(&other, "self").await;

    let allowed = ask(&d, &format!("call {target_pid} echo x")).await;
    let refused = ask(&d, &format!("call {other_pid} echo x")).await;

    assert_eq!(allowed, "x");
    assert_eq!(refused, "err:AccessDenied");
    d.kill();
    target.kill();
    other.kill();
}

fn route(prefix: &str) -> HttpRoute {
    HttpRoute {
        host: None,
        path_prefix: prefix.into(),
        hash_header: Some("x-customer".into()),
        max_body: 1024,
        timeout: Duration::from_secs(3),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver____asks_for_routes____told_the_table_and_each_change() {
    let n = node_with("routes", true);
    let d = up(&n, "drv");
    let plain = up(&n, "plain");
    ask(&d, "listeners").await;
    let table = RouteTable::new(n.rt.clone());

    table.set("shop", vec![route("/shop")]).unwrap();
    let first = common::ask_until(&d, "infos", |i| i.contains("/shop")).await;
    table.set("orders", vec![route("/orders")]).unwrap();
    let second = common::ask_until(&d, "infos", |i| i.contains("/orders")).await;
    let not_asked = ask(&plain, "infos").await;

    assert!(first.contains("event routes"), "{first}");
    assert!(first.contains(r#""app":"shop""#), "{first}");
    assert!(first.contains(r#""hash_header":"x-customer""#), "{first}");
    assert!(second.contains(r#""app":"orders""#), "{second}");
    assert!(!not_asked.contains("routes"), "{not_asked}");
    d.kill();
    plain.kill();
}

/// The HTTP ingress's rule (ROADMAP DA2): a driver whose profile says
/// `calls_routed` may call exactly the apps owning routes, as the table
/// changes, and they accept it whatever their `accept_from`, as they
/// accepted the node's own ingress.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver____calls_routed____the_route_owners_past_accept_from_and_no_other() {
    let n = node_profile("routed", true, true);
    let d = up(&n, "drv");
    let shop = up_accepting(&n, "shop", &["nobody"]);
    let plain = up(&n, "plain");
    let shop_pid = ask(&shop, "self").await;
    let plain_pid = ask(&plain, "self").await;
    let table = RouteTable::new(n.rt.clone());

    let before = ask(&d, &format!("call {shop_pid} echo x")).await;
    table.set("shop", vec![route("/shop")]).unwrap();
    let routed = ask(&d, &format!("call {shop_pid} echo x")).await;
    let other = ask(&d, &format!("call {plain_pid} echo x")).await;
    let from_plain = ask(&plain, &format!("call {shop_pid} echo x")).await;
    table.remove("shop");
    let after = ask(&d, &format!("call {shop_pid} echo x")).await;

    assert_eq!(before, "err:AccessDenied");
    assert_eq!(routed, "x");
    assert_eq!(other, "err:AccessDenied");
    assert_eq!(
        from_plain, "err:AccessDenied",
        "accept_from holds for others"
    );
    assert_eq!(after, "err:AccessDenied");
    d.kill();
    shop.kill();
    plain.kill();
}

/// A runtime whose app `drv` is a driver with the `admin` and `metrics`
/// flags as given, the node's side of each played here: an admin stream
/// answered `admin:<what it was sent>`, metrics rendered as one line.
/// The hooks are returned to keep: the runtime holds them weakly.
fn node_admin(
    admin: bool,
    metrics: bool,
) -> (
    Runtime,
    LoadedApp,
    bulkhead_host::AdminStreams,
    bulkhead_host::MetricsText,
) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        native: native_config(),
        drivers: BTreeMap::from([(
            "drv".to_string(),
            DriverProfile {
                admin,
                metrics,
                ..DriverProfile::default()
            },
        )]),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let streams: bulkhead_host::AdminStreams = Arc::new(|mut stream| {
        std::thread::spawn(move || {
            use std::io::Write;
            let mut got = String::new();
            stream.read_to_string(&mut got).unwrap();
            stream.write_all(format!("admin:{got}").as_bytes()).unwrap();
        });
    });
    let text: bulkhead_host::MetricsText = Arc::new(|| "node_metric 1\n".to_string());
    rt.set_admin_streams(&streams);
    rt.set_metrics_text(&text);
    let code = rt.load_native(NativeCode {
        path: native_fixtures().1.clone(),
        kind: NativeProgram::Executable,
    });
    (rt, code, streams, text)
}

fn spawn(rt: &Runtime, code: &LoadedApp, app: &str) -> InstanceHandle {
    let spec = InstanceSpec {
        memory_max: 64 << 20,
        callback_deadline: Duration::from_secs(5),
        init_timeout: Duration::from_secs(5),
        ..InstanceSpec::new(app, "1.0.0")
    };
    rt.spawn(code, spec).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver____admin_and_metrics_flags____a_stream_to_the_node_and_its_metrics() {
    let (rt, code, _streams, _text) = node_admin(true, true);
    let d = spawn(&rt, &code, "drv");

    let answered = ask(&d, "admin-stream GET /v1/apps").await;
    let text = ask(&d, "metrics-text").await;
    let from_sender = ask(&d, "sender-metrics-text").await;

    assert_eq!(answered, "admin:GET /v1/apps");
    assert_eq!(text, "node_metric 1\n");
    assert_eq!(from_sender, "node_metric 1\n");
    d.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver____without_the_flags____admin_streams_and_metrics_denied() {
    let (rt, code, _streams, _text) = node_admin(false, false);
    let d = spawn(&rt, &code, "drv");

    let stream = ask(&d, "admin-stream GET /v1/apps").await;
    let text = ask(&d, "metrics-text").await;
    let from_sender = ask(&d, "sender-metrics-text").await;

    assert_eq!(stream, "err:Denied");
    assert_eq!(text, "err:Denied");
    assert_eq!(from_sender, "err:Denied");
    d.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app____not_a_driver____admin_streams_and_metrics_denied() {
    let (rt, code, _streams, _text) = node_admin(true, true);
    let app = spawn(&rt, &code, "shop");

    let stream = ask(&app, "admin-stream GET /v1/apps").await;
    let text = ask(&app, "metrics-text").await;

    assert_eq!(
        (stream.as_str(), text.as_str()),
        ("err:Denied", "err:Denied")
    );
    app.kill();
}

/// A peer that greets each connection with `greeting`, late enough that a
/// read on a non-blocking stream would find nothing yet, and closes it.
fn greeter(listener: TcpListener, greeting: &'static str) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            use std::io::Write;
            std::thread::sleep(Duration::from_millis(50));
            let _ = stream.unwrap().write_all(greeting.as_bytes());
        }
    });
}

/// A runtime whose app `drv` is a driver with `connect` as its endpoint,
/// names resolved from a table (`db.test` is loopback).
fn node_connect(connect: Option<bulkhead_host::DriverEndpoint>) -> (Runtime, LoadedApp) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        native: native_config(),
        drivers: BTreeMap::from([(
            "drv".to_string(),
            DriverProfile {
                connect,
                ..DriverProfile::default()
            },
        )]),
        resolver: bulkhead_host::Resolver::Table(BTreeMap::from([(
            "db.test".to_string(),
            vec!["127.0.0.1".parse().unwrap()],
        )])),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let code = rt.load_native(NativeCode {
        path: native_fixtures().1.clone(),
        kind: NativeProgram::Executable,
    });
    (rt, code)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver____tcp_endpoint_by_name____the_node_connects_it_from_a_callback_or_a_sender() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    greeter(listener, "hello from the database");
    let (rt, code) = node_connect(Some(bulkhead_host::DriverEndpoint::Tcp {
        host: "db.test".into(),
        port,
        timeout: Duration::from_secs(5),
    }));
    let d = spawn(&rt, &code, "drv");

    let from_callback = ask(&d, "driver-connect").await;
    let from_sender = ask(&d, "sender-connect").await;

    assert_eq!(from_callback, "hello from the database");
    assert_eq!(from_sender, "hello from the database");
    d.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver____unix_endpoint____the_node_connects_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".s.PGSQL.5432");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            use std::io::Write;
            std::thread::sleep(Duration::from_millis(50));
            let _ = stream.unwrap().write_all(b"hello over a socket");
        }
    });
    let (rt, code) = node_connect(Some(bulkhead_host::DriverEndpoint::Unix(path)));
    let d = spawn(&rt, &code, "drv");

    let got = ask(&d, "driver-connect").await;

    assert_eq!(got, "hello over a socket");
    d.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver____no_endpoint_or_an_unknown_name____denied_or_failed() {
    let (rt, code) = node_connect(None);
    let none = spawn(&rt, &code, "drv");
    let (rt2, code2) = node_connect(Some(bulkhead_host::DriverEndpoint::Tcp {
        host: "nowhere.test".into(),
        port: 5432,
        timeout: Duration::from_secs(1),
    }));
    let unknown = spawn(&rt2, &code2, "drv");

    let denied = ask(&none, "driver-connect").await;
    let failed = ask(&unknown, "driver-connect").await;

    assert_eq!(denied, "err:Denied");
    assert!(failed.contains("nowhere.test"), "{failed}");
    none.kill();
    unknown.kill();
}
