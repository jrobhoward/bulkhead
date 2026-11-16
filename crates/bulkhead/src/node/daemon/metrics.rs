//! The node's metrics endpoint (`[metrics]`): since ROADMAP DA7 the
//! driver `bulkhead-metrics`, a native app of the node's own
//! (`METRICS_APP`), serves Prometheus's `GET /metrics`, so no
//! unauthenticated HTTP request is parsed in the node. The node binds the
//! address before its apps start and passes the listener; the driver asks
//! for the text on each scrape (`Request::MetricsText`), which the node
//! renders as `bulkhead metrics` does, the cluster's and replication's
//! gauges included.

use std::sync::Arc;

use bulkhead_bundle::manifest::NativeProgram;
use bulkhead_host::{AppDeployment, Apps, DriverListener, DriverProfile, NativeCode, Runtime};

use super::builtin;
use crate::node::config::{METRICS_APP, NodeConfig};
use crate::node::error::NodeError;

/// `[metrics]` bound for the driver: its profile, the listener and the
/// right to read the metrics; `None` without `[metrics]`.
///
/// # Errors
///
/// The address cannot be bound.
pub(super) fn bind(
    config: &NodeConfig,
    native: &bulkhead_host::NativeConfig,
) -> Result<Option<DriverProfile>, NodeError> {
    let Some(binding) = &config.metrics else {
        return Ok(None);
    };
    let socket = bulkhead_host::bind_tcp(binding.address, native)
        .map_err(|e| NodeError::Invalid(format!("metrics: {}: {e}", binding.address)))?;
    Ok(Some(DriverProfile {
        listeners: vec![DriverListener {
            name: "metrics".into(),
            socket: Arc::new(socket),
        }],
        metrics: true,
        ..DriverProfile::default()
    }))
}

/// The driver's manifest: one instance, small, restarts that never give
/// up, nothing granted but what its profile gives.
fn manifest() -> Result<bulkhead_bundle::manifest::AppManifest, NodeError> {
    let spec = serde_json::json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "instances": { "count": 1 },
        "supervision": builtin::supervision(),
        "limits": { "memory_max": "64MiB" },
        "messaging_acl": { "accept_from": [METRICS_APP], "may_call": [METRICS_APP] },
    });
    builtin::manifest(METRICS_APP, "The node's metrics endpoint", &spec)
}

/// Start the driver, with `[metrics]`.
///
/// # Errors
///
/// Its program is missing, or it did not start.
pub(super) async fn start(config: &NodeConfig, rt: &Runtime, apps: &Apps) -> Result<(), NodeError> {
    if config.metrics.is_none() {
        return Ok(());
    }
    let program = builtin::program(config, config.native.metrics.as_ref(), METRICS_APP);
    if !program.is_file() {
        return Err(NodeError::Invalid(format!(
            "[metrics] needs {METRICS_APP}, and {} is not it (install it beside \
             bulkhead-runner, or set [native] metrics)",
            program.display()
        )));
    }
    let code = rt.load_native(NativeCode {
        path: program,
        kind: NativeProgram::Executable,
    });
    builtin::start(rt, apps, AppDeployment::new(manifest()?, code)).await?;
    tracing::info!("metrics endpoint driver started");
    Ok(())
}
