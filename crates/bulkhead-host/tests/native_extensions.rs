//! Packages a native app calls by name (`Request::Extension`): since
//! ROADMAP DA8 only the built-in Modbus package is served (its routing is
//! tested in `drivers.rs`); a call of any other answers `NoSuchExtension`,
//! declared and granted or not, and the app lives on.

// The native fixture runs on Linux and FreeBSD.
#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::sync::OnceLock;
use std::time::Duration;

use bulkhead_bundle::manifest::NativeProgram;
use bulkhead_host::services::ExtensionSpec;
use bulkhead_host::{
    InstanceHandle, InstanceSpec, LoadedApp, NativeCode, NativeConfig, Runtime, RuntimeConfig,
};
use common::{ask, native_fixtures};
use rstest::rstest;
use serde_json::json;

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

/// An instance declaring `example:greeting`, granted it when `granted`.
fn up(app: &str, granted: bool) -> InstanceHandle {
    let mut spec = InstanceSpec {
        callback_deadline: Duration::from_secs(5),
        init_timeout: Duration::from_secs(5),
        ..InstanceSpec::new(app, "1.0.0")
    };
    spec.services.extensions.insert(
        "example:greeting".into(),
        ExtensionSpec::new(json!({ "salutation": "Howdy" })),
    );
    let grant = if granted {
        BTreeSet::from(["example:greeting".to_string()])
    } else {
        BTreeSet::new()
    };
    let (rt, loaded) = shared();
    rt.spawn(loaded, spec.with_granted_extensions(&grant))
        .unwrap()
}

#[rstest]
#[case::declared_and_granted("granted", true, "example:greeting greet#hello")]
#[case::declared_not_granted("ungranted", false, "example:greeting greet#hello")]
#[case::undeclared("undeclared", true, "test:absent probe#seen")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ext____package_no_one_serves____no_such_extension_and_the_app_lives_on(
    #[case] app: &str,
    #[case] granted: bool,
    #[case] target: &str,
) {
    let instance = up(app, granted);

    let result = ask(&instance, &format!("ext {target} null")).await;
    let after = ask(&instance, "echo alive").await;

    assert_eq!(result, "err:NoSuchExtension");
    assert_eq!(after, "alive");
}
