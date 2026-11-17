//! An instance's processes end with it without the privileged helper
//! (ROADMAP PS7): the node kills the instance's process group, which the
//! runner's seccomp filter keeps a child from leaving on Linux. On FreeBSD
//! capability mode lets a child call `setsid`, which escapes the group: a
//! gap SECURITY.md records, closed under the helper by the jail.

// The native fixture runs on Linux and FreeBSD.
#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use bulkhead_bundle::manifest::NativeProgram;
use bulkhead_host::{
    InstanceHandle, InstanceSpec, LoadedApp, NativeCode, NativeConfig, Runtime, RuntimeConfig,
};
use common::{ask, native_fixtures, text};

fn shared() -> &'static (Runtime, LoadedApp) {
    static SHARED: OnceLock<(Runtime, LoadedApp)> = OnceLock::new();
    SHARED.get_or_init(|| {
        let mut native = NativeConfig::default();
        native.runner = Some(native_fixtures().0.clone());
        let rt = Runtime::new(RuntimeConfig {
            max_instances: 16,
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

fn up(app: &str) -> InstanceHandle {
    let spec = InstanceSpec {
        memory_max: 64 << 20,
        callback_deadline: Duration::from_secs(5),
        init_timeout: Duration::from_secs(5),
        shutdown_timeout: Duration::from_millis(300),
        ..InstanceSpec::new(app, "1.0.0")
    };
    let (rt, loaded) = shared();
    rt.spawn(loaded, spec).unwrap()
}

/// A `key=value` field of the fixture's `forks` answer.
fn field<'a>(answer: &'a str, key: &str) -> &'a str {
    answer
        .split(' ')
        .find_map(|w| w.strip_prefix(key)?.strip_prefix('='))
        .unwrap_or("")
}

/// Whether process `pid` is gone within 3 s.
async fn gone(pid: i32) -> bool {
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if rustix::process::test_kill_process(pid).is_err() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// A child the instance forked, which ignores `SIGTERM`.
async fn forked_child(i: &InstanceHandle) -> i32 {
    let forked = ask(i, "forks 1 0 term-proof").await;
    assert_eq!(field(&forked, "alive"), "1", "{forked}");
    field(&forked, "last").parse().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forked_child____instance_killed____ends_with_it() {
    let i = up("proc-killed");
    let child = forked_child(&i).await;

    i.kill();
    let _ = tokio::time::timeout(Duration::from_secs(5), i.wait()).await;

    assert!(gone(child).await, "child {child} outlived its instance");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forked_child____instance_exits_on_its_own____ends_with_it() {
    let i = up("proc-exits");
    let child = forked_child(&i).await;

    let _ = i.call(text("abort"), Duration::from_secs(5)).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), i.wait()).await;

    assert!(gone(child).await, "child {child} outlived its instance");
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setsid____in_a_forked_child____refused_and_the_instance_lives() {
    let i = up("proc-setsid");

    let tried = ask(&i, "setsid-child").await;
    let after = ask(&i, "echo alive").await;

    assert_eq!(tried, "refused");
    assert_eq!(after, "alive");
    i.kill();
}

/// The documented gap: capability mode allows `setsid`, so a child can
/// leave the group and outlive its instance without the helper's jail.
/// If this starts failing, FreeBSD closed it: update SECURITY.md.
#[cfg(target_os = "freebsd")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setsid____in_a_forked_child____escapes_the_group_without_a_jail() {
    let i = up("proc-setsid");

    let tried = ask(&i, "setsid-child").await;
    i.kill();
    let _ = tokio::time::timeout(Duration::from_secs(5), i.wait()).await;
    let escaped: Option<i32> = tried.strip_prefix("escaped ").and_then(|p| p.parse().ok());
    if let Some(pid) = escaped.and_then(rustix::process::Pid::from_raw) {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
    }

    assert!(escaped.is_some(), "{tried}");
}
