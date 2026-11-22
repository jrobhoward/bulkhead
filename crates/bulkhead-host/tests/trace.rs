//! Trace context across calls (ROADMAP O1): what a traced callback sends
//! joins its trace by itself, through native instances of the fixture.

// The native fixture runs on Linux and FreeBSD.
#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::sync::OnceLock;
use std::time::Duration;

use bulkhead_bundle::manifest::NativeProgram;
use bulkhead_host::{
    InstanceHandle, InstanceSpec, LoadedApp, NativeCode, NativeConfig, Runtime, RuntimeConfig,
    Trace,
};
use common::{LONG, ask, body, native_fixtures, text};

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
    let (rt, loaded) = shared();
    let spec = InstanceSpec {
        callback_deadline: Duration::from_secs(5),
        init_timeout: Duration::from_secs(5),
        ..InstanceSpec::new(app, "1.0.0")
    };
    rt.spawn(loaded, spec).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace____a_traced_call_whose_callback_calls_another____the_same_trace_a_new_span() {
    let a = up("trace-a");
    let b = up("trace-b");
    let b_pid = ask(&b, "self").await;
    let root = Trace::root();
    let traced = |command: &str| {
        let mut msg = text(command);
        msg.trace = Some(Box::new(root));
        msg
    };

    let own = body(&a.call(traced("trace"), LONG).await.unwrap());
    let onward = body(
        &a.call(traced(&format!("call {b_pid} trace")), LONG)
            .await
            .unwrap(),
    );
    let untraced = ask(&a, &format!("call {b_pid} trace")).await;

    assert_eq!(own, root.traceparent(), "the app reads its call's trace");
    let (next, sampled) = Trace::from_traceparent(&onward).unwrap();
    assert!(sampled);
    assert_eq!(next.trace_id, root.trace_id, "the same trace");
    assert_ne!(next.span_id, root.span_id, "a span of its own");
    assert_eq!(untraced, "none", "an untraced callback starts none");
}
