//! Host names through a node's resolver driver (ROADMAP DA9): an app's
//! `connect_host` answered from the hosts file the node read and gave to
//! `bulkhead-resolve`, the file replaced by rename and read again on a
//! reload, and a node without the driver refusing to start. The driver's
//! DNS is tested in `bulkhead-resolve`'s own tests.

// Native apps run on Linux and FreeBSD (`bulkhead-runner`).
#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use common::{TestNode, app_bundle, call, fast, native_net};
use serde_json::json;

/// A peer on loopback that greets each connection.
async fn peer() -> SocketAddr {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let _ = stream.write_all(b"hello").await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
    address
}

/// A port nothing listens on: the nameserver, so only the hosts file
/// answers.
fn closed_port() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// Node config: the resolver's files in `dir`, the app `net` granted
/// sockets to loopback.
fn config(dir: &Path, nameserver: SocketAddr) -> String {
    let policy = dir.join("policy.toml");
    std::fs::write(&policy, "[apps.net]\ngrant = [\"bulkhead:sockets\"]\n").unwrap();
    std::fs::write(dir.join("resolv.conf"), "options timeout:1 attempts:1\n").unwrap();
    format!(
        "policy = {policy:?}\n[resolver]\nresolv_conf = {:?}\nhosts = {:?}\nnameservers = [\"{nameserver}\"]\nreload_s = 0\n",
        dir.join("resolv.conf"),
        dir.join("hosts"),
    )
}

/// Replace the hosts file by rename, as editors and DHCP clients do.
fn replace_hosts(dir: &Path, text: &str) {
    std::fs::write(dir.join("hosts.new"), text).unwrap();
    std::fs::rename(dir.join("hosts.new"), dir.join("hosts")).unwrap();
}

/// Deploy the net fixture as `net`, allowed TCP to `peer`'s port on
/// loopback.
async fn deploy_net(n: &TestNode, peer: SocketAddr) {
    let scope =
        json!({ "connect": [{ "cidr": "127.0.0.1/32", "ports": peer.port().to_string() }] });
    n.client
        .deploy(app_bundle(
            &n.key,
            "net",
            "1.0.0",
            &native_net(),
            fast(json!({ "capabilities": { "bulkhead:sockets": scope } })),
        ))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_host____a_name_in_the_hosts_file____resolved_by_the_driver_and_connected() {
    let peer = peer().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hosts"), "127.0.0.1 peer.test\n").unwrap();
    let mut n = TestNode::start_native(&config(dir.path(), closed_port())).await;
    deploy_net(&n, peer).await;

    let id = call(
        &n,
        "net",
        &format!("connect-host peer.test {}", peer.port()),
    )
    .await;
    call(&n, "net", &format!("watch {id}")).await;
    let mut received = String::new();
    for _ in 0..100 {
        received = call(&n, "net", "received").await;
        if !received.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(received, format!("{id}:hello"));
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload____hosts_replaced_by_rename____the_driver_restarts_with_the_new_file() {
    let peer = peer().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hosts"), "127.0.0.1 old.test\n").unwrap();
    let mut n = TestNode::start_native(&config(dir.path(), closed_port())).await;
    deploy_net(&n, peer).await;
    let before = call(&n, "net", &format!("connect-host new.test {}", peer.port())).await;
    replace_hosts(dir.path(), "127.0.0.1 new.test\n");

    n.node().reload_tls().await.unwrap();
    let new = call(&n, "net", &format!("connect-host new.test {}", peer.port())).await;
    let old = call(&n, "net", &format!("connect-host old.test {}", peer.port())).await;

    // Not in the file and no nameserver answering: a temporary failure.
    assert!(before.starts_with("err:"), "{before}");
    assert!(!new.starts_with("err:"), "{new}");
    assert!(old.starts_with("err:"), "{old}");
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_host____the_name_an_address____connected_without_the_resolver() {
    let peer = peer().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hosts"), "").unwrap();
    let mut n = TestNode::start_native(&config(dir.path(), closed_port())).await;
    deploy_net(&n, peer).await;

    let id = call(
        &n,
        "net",
        &format!("connect-host 127.0.0.1 {}", peer.port()),
    )
    .await;

    assert!(id.parse::<u32>().is_ok(), "{id}");
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open____resolver_program_missing____refused_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let toml = format!(
        "data_dir = {:?}\n[native]\nrunner = {:?}\nresolve = {:?}\n",
        dir.path().join("data"),
        common::native_fixtures().0,
        dir.path().join("absent"),
    );

    let opened = bulkhead::node::Node::open(
        bulkhead::node::NodeConfig::from_toml_str(&toml).unwrap(),
        common::ring(),
    )
    .await;

    let Err(e) = opened else {
        panic!("a node opened without its resolver");
    };
    assert!(e.to_string().contains("[native] resolve"), "{e}");
}
