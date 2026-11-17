//! The misbehaviour policy (ROADMAP PS5): a native instance that meets
//! too many refusals (`access-denied` answers and messaging-ACL
//! refusals, from its callbacks or its sender channels) within the
//! period, or makes more requests a second than its rate, is killed as
//! `crashed: policy(refusals)` or `crashed: policy(rate)`.

// The native fixture runs on Linux and FreeBSD.
#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::sync::OnceLock;
use std::time::Duration;

use bulkhead_bundle::manifest::NativeProgram;
use bulkhead_host::acl::Acl;
use bulkhead_host::{
    ExitReason, HostObjectRequest, InstanceHandle, InstanceSpec, LoadedApp, NativeCode,
    NativeConfig, Runtime, RuntimeConfig,
};
use common::{ask, native_fixtures, text};

fn shared() -> &'static (Runtime, LoadedApp) {
    static SHARED: OnceLock<(Runtime, LoadedApp)> = OnceLock::new();
    SHARED.get_or_init(|| {
        let mut native = NativeConfig::default();
        native.runner = Some(native_fixtures().0.clone());
        let rt = Runtime::new(RuntimeConfig {
            max_instances: 32,
            native,
            ..RuntimeConfig::default()
        })
        .unwrap();
        let app = rt.load_native(NativeCode {
            path: native_fixtures().1.clone(),
            kind: NativeProgram::Executable,
        });
        (rt, app)
    })
}

/// An instance of `app`, granted nothing, with `limits` asked of the node.
fn up(app: &str, limits: HostObjectRequest) -> InstanceHandle {
    let spec = InstanceSpec {
        memory_max: 64 << 20,
        callback_deadline: Duration::from_secs(10),
        init_timeout: Duration::from_secs(5),
        host_objects: limits,
        ..InstanceSpec::new(app, "1.0.0")
    };
    let (rt, loaded) = shared();
    rt.spawn(loaded, spec).unwrap()
}

fn refusals(n: u32) -> HostObjectRequest {
    HostObjectRequest {
        refusals: Some(n),
        ..HostObjectRequest::default()
    }
}

/// Ask `command`, which the policy is expected to end the instance in;
/// its exit reason.
async fn killed_by(instance: &InstanceHandle, command: &str) -> ExitReason {
    let _ = instance.call(text(command), Duration::from_secs(20)).await;
    tokio::time::timeout(Duration::from_secs(10), instance.wait())
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusals____up_to_the_nodes_100____the_instance_lives() {
    let i = up("pol-100", HostObjectRequest::default());

    let refused = ask(&i, "refuse 100").await;
    let after = ask(&i, "echo alive").await;

    assert_eq!(refused, "refused 100");
    assert_eq!(after, "alive");
    i.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusals____one_past_the_nodes_100____killed_policy_refusals() {
    let i = up("pol-101", HostObjectRequest::default());

    let reason = killed_by(&i, "refuse 101").await;

    assert_eq!(reason, ExitReason::Crashed("policy(refusals)".into()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusals____past_a_lower_manifest_limit____killed() {
    let i = up("pol-low", refusals(5));

    let five = ask(&i, "refuse 5").await;
    let reason = killed_by(&i, "refuse 1").await;

    assert_eq!(five, "refused 5");
    assert_eq!(reason, ExitReason::Crashed("policy(refusals)".into()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusals____calls_the_acl_refuses____counted() {
    let (rt, loaded) = shared();
    let mut closed = InstanceSpec {
        memory_max: 64 << 20,
        init_timeout: Duration::from_secs(5),
        ..InstanceSpec::new("pol-closed", "1.0.0")
    };
    closed.acl = Acl {
        accept_from: vec!["someone-else".into()],
        may_call: Vec::new(),
    };
    let target = rt.spawn(loaded, closed).unwrap();
    let caller = up("pol-acl", refusals(3));

    let to = ask(&target, "self").await;

    let three = ask(&caller, &format!("refuse-acl 3 {to}")).await;
    let reason = killed_by(&caller, &format!("refuse-acl 1 {to}")).await;

    assert_eq!(three, "refused 3");
    assert_eq!(reason, ExitReason::Crashed("policy(refusals)".into()));
    target.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusals____from_a_sender_channel____counted_with_the_callbacks() {
    let i = up("pol-sender", refusals(4));

    let opened = ask(&i, "sender open").await;
    let two = ask(&i, "refuse 2").await;
    let sender_two = ask(&i, "sender 0 refuse 2").await;
    let reason = killed_by(&i, "sender 0 refuse 1").await;

    assert_eq!(opened, "0");
    assert_eq!(
        (two.as_str(), sender_two.as_str()),
        ("refused 2", "refused 2")
    );
    assert_eq!(reason, ExitReason::Crashed("policy(refusals)".into()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate____a_burst_over_requests_per_sec____killed_policy_rate() {
    let i = up(
        "pol-rate",
        HostObjectRequest {
            requests_per_sec: Some(1_000),
            ..HostObjectRequest::default()
        },
    );

    let under = ask(&i, "requests 500").await;
    let reason = killed_by(&i, "requests 5000").await;

    assert_eq!(under, "asked 500");
    assert_eq!(reason, ExitReason::Crashed("policy(rate)".into()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate____unset____a_burst_lives() {
    let i = up("pol-norate", HostObjectRequest::default());

    let asked = ask(&i, "requests 10000").await;
    let after = ask(&i, "echo alive").await;

    assert_eq!((asked.as_str(), after.as_str()), ("asked 10000", "alive"));
    i.kill();
}
