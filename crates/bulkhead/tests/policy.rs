//! Quarantine (ROADMAP PS5): an app whose instances the misbehaviour
//! policy keeps killing is stopped and stays stopped, across a node
//! restart, until `bulkhead start`.

// The native fixture runs on Linux and FreeBSD.
#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::time::Duration;

use common::{TestNode, app_bundle, fast, native_app};
use serde_json::json;

/// A node killing an instance past 2 refusals, quarantining an app at
/// `after` kills.
async fn node(after: u32) -> TestNode {
    TestNode::start_native(&format!(
        "[limits]\nrefusals_max = 2\nquarantine_after = {after}\n"
    ))
    .await
}

async fn deploy_rogue(n: &TestNode) {
    n.client
        .deploy(app_bundle(
            &n.key,
            "rogue",
            "1.0.0",
            &native_app(),
            fast(json!({ "supervision": { "intensity": 10 } })),
        ))
        .await
        .unwrap();
}

/// The live instance of `app`, waiting up to 5 s for one.
async fn running(n: &TestNode, app: &str) -> Option<String> {
    for _ in 0..250 {
        let ps = common::ps(&n.client).await;
        if let Some(i) = ps.iter().find(|i| i.app == app && i.state == "active") {
            return Some(i.pid.clone());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

/// Have the app's live instance meet three refusals, which the policy
/// kills it for, and wait until it has gone.
async fn misbehave(n: &TestNode) {
    let pid = running(n, "rogue").await.expect("a rogue instance");
    let _ = n
        .client
        .call("rogue", "text/plain", b"refuse 3".to_vec(), 5_000)
        .await;
    for _ in 0..250 {
        if !common::ps(&n.client).await.iter().any(|i| i.pid == pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{pid} was not killed");
}

async fn status(n: &TestNode) -> (String, Option<String>) {
    let s = n.client.status("rogue").await.unwrap().summary;
    (s.status, s.reason)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quarantine____three_policy_kills____stopped_across_a_restart_until_started() {
    let mut n = node(3).await;
    deploy_rogue(&n).await;

    for _ in 0..3 {
        misbehave(&n).await;
    }
    // The status turns first, the audit record follows: wait for both.
    let mut quarantined = status(&n).await;
    let mut events = n.events();
    for _ in 0..100 {
        if quarantined.0 == "quarantined" && events.iter().any(|e| e == "quarantine") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        quarantined = status(&n).await;
        events = n.events();
    }
    n.restart().await;
    let after_restart = status(&n).await.0;
    let none_running = running(&n, "rogue").await.is_none();
    n.client.start("rogue").await.unwrap();
    let started = (status(&n).await.0, running(&n, "rogue").await.is_some());
    let start_record = n.audit().into_iter().rev().find(|r| r["event"] == "start");

    assert_eq!(quarantined.0, "quarantined");
    let reason = quarantined.1.unwrap_or_default();
    assert!(reason.contains("policy(refusals)"), "{reason}");
    assert_eq!(events.iter().filter(|e| *e == "misbehaved").count(), 3);
    assert!(events.iter().any(|e| e == "quarantine"), "{events:?}");
    assert_eq!(after_restart, "quarantined");
    assert!(none_running);
    assert_eq!(started, ("running".to_string(), true));
    assert!(
        start_record.is_some_and(|r| r["was_quarantined"].is_string()),
        "the start records the quarantine it cleared"
    );
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quarantine____after_zero____never_quarantined() {
    let mut n = node(0).await;
    deploy_rogue(&n).await;

    for _ in 0..4 {
        misbehave(&n).await;
    }
    let pid = running(&n, "rogue").await;
    let (state, _) = status(&n).await;

    assert!(pid.is_some(), "restarted each time");
    assert_eq!(state, "running");
    assert!(!n.events().iter().any(|e| e == "quarantine"));
    n.shutdown().await;
}
