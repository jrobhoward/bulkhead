//! Each datasource's driver (ROADMAP DA3): `bulkhead-sql`, started as the
//! app `bulkhead-sql-<name>` for every `[datasources.<name>]`, so no
//! database's reply is parsed in the node and no datasource's password is
//! read by it (one from the node's environment excepted: the node passes
//! its value).
//!
//! The runtime gives each driver its profile from the datasource
//! (`bulkhead_host::SqlDatasource::driver_profile`): the one address the
//! node connects it to, and its password and CA files under `/driver/`.
//! This module starts the drivers, one instance each, before the node's
//! apps, which may use a datasource as they start.

use std::path::PathBuf;

use bulkhead_bundle::manifest::{AppManifest, NativeProgram};
use bulkhead_host::{AppDeployment, Apps, NativeCode, Runtime, SqlDatasource};

use super::builtin;
use crate::node::config::{ClusterRole, NodeConfig, SQL_PROGRAM};
use crate::node::error::NodeError;

/// A datasource driver's manifest: one instance; messages from itself
/// alone, so no app can reach its database but through the node's checks
/// and caps; restarts that never give up; its counters as
/// `bulkhead_sql_*`, labelled with the datasource.
fn manifest(app: &str, args: serde_json::Value) -> Result<AppManifest, NodeError> {
    let spec = serde_json::json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "instances": { "count": 1 },
        "lifecycle": { "start_args": args },
        "supervision": builtin::supervision(),
        "limits": { "memory_max": "256MiB" },
        "messaging_acl": { "accept_from": [app], "may_call": [app] },
        "capabilities": {
            "bulkhead:metrics": { "prefix": "bulkhead_sql" },
        },
    });
    builtin::manifest(app, "A datasource's Postgres client", &spec)
}

/// Where the program is: `[native] sql`, or beside the runner.
fn program(config: &NodeConfig) -> Result<PathBuf, NodeError> {
    let program = builtin::program(config, config.native.sql.as_ref(), SQL_PROGRAM);
    if !program.is_file() {
        return Err(NodeError::Invalid(format!(
            "[datasources] need {SQL_PROGRAM}, and {} is not it (install it beside \
             bulkhead-runner, or set [native] sql)",
            program.display()
        )));
    }
    Ok(program)
}

/// Start each datasource's driver; none on a witness, which runs no apps.
///
/// # Errors
///
/// The program is missing, a datasource's arguments cannot be made (its
/// password variable unset), or a driver did not start.
pub(super) async fn start(
    config: &NodeConfig,
    datasources: &std::collections::BTreeMap<String, SqlDatasource>,
    rt: &Runtime,
    apps: &Apps,
) -> Result<(), NodeError> {
    if datasources.is_empty() || config.cluster.role == ClusterRole::Witness {
        return Ok(());
    }
    let program = program(config)?;
    let code = rt.load_native(NativeCode {
        path: program,
        kind: NativeProgram::Executable,
    });
    for (name, ds) in datasources {
        let app = SqlDatasource::driver_app(name);
        let args = ds.driver_args(name).map_err(NodeError::Invalid)?;
        builtin::start(
            rt,
            apps,
            AppDeployment::new(manifest(&app, args)?, code.clone()),
        )
        .await?;
        tracing::info!(datasource = %name, "datasource driver started");
    }
    Ok(())
}
