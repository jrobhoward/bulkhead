//! The driver's process killed: the runtime's supervisor starts it again,
//! the node keeps its listening sockets, so the port stays open and
//! connections made meanwhile are served. A test binary of its own: the
//! driver it kills is the only one this process runs.

#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::process::Command;
use std::time::Duration;

use common::{Listen, request, route, until};
use serde_json::json;

/// This process's children running `bulkhead-ingress`: its path on Linux,
/// its name after the run-time linker's arguments on FreeBSD
/// (`ld-elf.so.1 -f FD -- bulkhead-ingress`).
fn drivers() -> Vec<u32> {
    let out = Command::new("pgrep")
        .args([
            "-P",
            &std::process::id().to_string(),
            "-f",
            "/bulkhead-ingress$|-- bulkhead-ingress$",
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|p| p.parse().ok())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn driver____killed____restarted_on_the_same_port_and_a_waiting_client_served() {
    let s = common::serve(vec![Listen::plain("main")], |_| None).await;
    s.start_app(
        "web",
        &json!({ "kind": "native-process", "native": { "program": "executable" } }),
    )
    .await;
    s.routes.set("web", vec![route("/api")]).unwrap();
    let address = s.address("main");
    until("the driver serves", || async {
        request(address, "GET", "/api/plain", &[], "").await.status == 200
    })
    .await;
    let first = drivers();
    assert_eq!(first.len(), 1, "{first:?}");

    let killed = Command::new("kill")
        .args(["-KILL", &first[0].to_string()])
        .status()
        .unwrap();
    // Connected while no driver runs: the node's socket queues it.
    let waiting = tokio::spawn(request(address, "GET", "/api/plain", &[], ""));
    until("another driver runs", || async {
        drivers().iter().any(|p| *p != first[0])
    })
    .await;
    let waited = tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .unwrap()
        .unwrap();
    let after = request(address, "GET", "/api/plain", &[], "").await;

    assert!(killed.success());
    assert_eq!((waited.status, waited.body.as_str()), (200, "plain"));
    assert_eq!((after.status, after.body.as_str()), (200, "plain"));
}
