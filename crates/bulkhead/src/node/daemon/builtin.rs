//! What the node's own driver apps share: the HTTP ingress
//! (`bulkhead-ingress`, ROADMAP DA2) and the resolver of host names
//! (`bulkhead-resolve`, DA9). Each is a program installed with the node,
//! started from a manifest built here rather than deployed: not signed,
//! not upgraded on its own, restarted by a supervisor that never gives
//! up.

use std::path::PathBuf;

use bulkhead_bundle::manifest::AppManifest;

use crate::node::config::NodeConfig;
use crate::node::error::NodeError;

/// A built-in driver's restarts: at most `INTENSITY` within `PERIOD_MS`,
/// each after a delay from `BACKOFF_MS` doubling to its cap. The cap
/// keeps a driver crashing at once to about 13 restarts in the period,
/// under the intensity, so its supervisor never gives up: whoever can
/// crash it (a client exhausting the ingress's memory, say) costs the
/// node that driver for a second at a time, not until the node restarts.
pub(super) const INTENSITY: u32 = 20;
pub(super) const PERIOD_MS: u64 = 10_000;
pub(super) const BACKOFF_MS: (u64, u64) = (100, 1_000);

/// Where the program `name` is: `explicit` (its `[native]` key), or
/// beside the runner.
pub(super) fn program(config: &NodeConfig, explicit: Option<&PathBuf>, name: &str) -> PathBuf {
    if let Some(path) = explicit {
        return path.clone();
    }
    let runner = config.native.runner.clone().unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|d| d.join("bulkhead-runner")))
            .unwrap_or_else(|| PathBuf::from("bulkhead-runner"))
    });
    runner
        .parent()
        .map_or_else(|| PathBuf::from(name), |d| d.join(name))
}

/// The supervision every built-in driver has, as manifest JSON.
pub(super) fn supervision() -> serde_json::Value {
    serde_json::json!({
        "intensity": INTENSITY,
        "period_ms": PERIOD_MS,
        "backoff": {
            "initial_ms": BACKOFF_MS.0,
            "max_ms": BACKOFF_MS.1,
            "jitter": false,
        },
    })
}

/// A manifest for the built-in app `name`, at the node's version, with
/// `spec` (manifest JSON) as its spec.
///
/// # Errors
///
/// `spec` is not a valid spec: a bug here, not the operator's.
pub(super) fn manifest(
    name: &str,
    description: &str,
    spec: &serde_json::Value,
) -> Result<AppManifest, NodeError> {
    use bulkhead_bundle::manifest::{AppInfo, AppSpec, ProgramRef, Provenance};
    Ok(AppManifest {
        app: AppInfo {
            name: name.into(),
            version: env!("CARGO_PKG_VERSION")
                .parse()
                .map_err(|e| NodeError::Internal(format!("{name} version: {e}")))?,
            description: Some(description.into()),
            authors: Vec::new(),
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
        spec: AppSpec::from_json_value(spec)
            .map_err(|e| NodeError::Internal(format!("{name} manifest: {e}")))?,
    })
}

#[cfg(test)]
#[path = "builtin_tests.rs"]
mod tests;
