//! Each app's egress driver (ROADMAP DA4): `bulkhead-egress`, started as
//! the app `bulkhead-egress-<app>` for every app granted
//! `bulkhead:http-egress` with an `allow` list, before the app, and
//! removed after it. No HTTP response or server TLS is parsed in the
//! node; a driver, compromised, holds one app's requests and reaches that
//! app's destinations.
//!
//! The runtime gives each driver its profile by its name
//! (`DriverProfile.egress`): the node connects it
//! (`Request::ConnectTo`) only where its app's running instances may send
//! requests. Its arguments are node config's egress settings and the
//! trusted roots (`Runtime::egress_driver_args`). Upgrades keep it; an
//! upgrade that adds egress starts it.

use std::path::PathBuf;

use bulkhead_bundle::manifest::{AppManifest, Capability, MAX_NAME_LEN, NativeProgram};
use bulkhead_host::{AppDeployment, NativeCode};

use super::{Node, builtin};
use crate::node::config::{EGRESS_PROGRAM, NodeConfig};
use crate::node::error::NodeError;

/// A driver's manifest: one instance; messages from itself alone, so no
/// app reaches it but through the node's checks; restarts that never give
/// up; its counters as `bulkhead_egress_*`.
fn manifest(driver: &str, args: serde_json::Value) -> Result<AppManifest, NodeError> {
    let spec = serde_json::json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "instances": { "count": 1 },
        "lifecycle": { "start_args": args },
        "supervision": builtin::supervision(),
        "limits": { "memory_max": "256MiB" },
        "messaging_acl": { "accept_from": [driver], "may_call": [driver] },
        "capabilities": {
            "bulkhead:metrics": { "prefix": "bulkhead_egress" },
        },
    });
    builtin::manifest(driver, "An app's outgoing HTTP", &spec)
}

/// Where the program is: `[native] egress`, or beside the runner.
fn program(config: &NodeConfig) -> Result<PathBuf, NodeError> {
    let program = builtin::program(config, config.native.egress.as_ref(), EGRESS_PROGRAM);
    if !program.is_file() {
        return Err(NodeError::Invalid(format!(
            "apps granted bulkhead:http-egress need {EGRESS_PROGRAM}, and {} is not it \
             (install it beside bulkhead-runner, or set [native] egress)",
            program.display()
        )));
    }
    Ok(program)
}

/// Whether `deployment` sends HTTP requests, and so needs a driver.
pub(super) fn needs_driver(deployment: &AppDeployment) -> bool {
    deployment.spec.granted.contains(&Capability::HttpOutgoing)
        && deployment.spec.services.egress.sends()
}

/// Why `name`, declaring `bulkhead:http-egress`, cannot have a driver, if
/// it cannot: the driver's name must be a valid app name.
pub(super) fn check_name(name: &str, manifest: &AppManifest) -> Result<(), NodeError> {
    let declares = manifest.spec.capabilities.http_outgoing.is_some();
    let longest = MAX_NAME_LEN - bulkhead_host::EGRESS_DRIVER_PREFIX.len();
    if declares && name.len() > longest {
        return Err(NodeError::Refused(format!(
            "{name} declares bulkhead:http-egress, so its name may have at most {longest} \
             characters: its egress driver runs as {}{name}",
            bulkhead_host::EGRESS_DRIVER_PREFIX
        )));
    }
    Ok(())
}

impl Node {
    /// Start `name`'s egress driver if `deployment` needs one and it does
    /// not run yet.
    ///
    /// # Errors
    ///
    /// The program is missing, or the driver did not start.
    pub(super) async fn ensure_egress_driver(
        &self,
        name: &str,
        deployment: &AppDeployment,
    ) -> Result<(), NodeError> {
        let driver = bulkhead_host::egress_driver_app(name);
        if !needs_driver(deployment) || self.apps.app(&driver).is_some() {
            return Ok(());
        }
        let Some(args) = self.rt.egress_driver_args(name) else {
            // Built without egress: requests answer that, no driver needed.
            return Ok(());
        };
        let code = self.rt.load_native(NativeCode {
            path: program(&self.config)?,
            kind: NativeProgram::Executable,
        });
        builtin::start(
            &self.rt,
            &self.apps,
            AppDeployment::new(manifest(&driver, args)?, code),
        )
        .await?;
        tracing::info!(app = %name, "egress driver started");
        Ok(())
    }

    /// After egress's roots were read again (ROADMAP DA4b): restart each
    /// running egress driver whose arguments changed (the roots), or that
    /// holds an identity, whose files it reads at its start. Requests in
    /// flight in a restarted driver are lost. The apps whose drivers
    /// restarted.
    ///
    /// # Errors
    ///
    /// The roots could not be read (the drivers keep theirs), or a driver
    /// did not start again.
    pub(super) async fn reload_egress(&self) -> Result<Vec<String>, String> {
        self.rt.reload_egress_roots()?;
        let mut restarted = Vec::new();
        for name in self.apps.names() {
            let driver = bulkhead_host::egress_driver_app(&name);
            let (Some(running), Some(app)) = (self.apps.app(&driver), self.apps.app(&name)) else {
                continue;
            };
            let Some(args) = self.rt.egress_driver_args(&name) else {
                continue;
            };
            let changed = running.deployment().manifest.spec.lifecycle.start_args != Some(args);
            if changed || self.config.egress.identities.contains_key(&name) {
                self.stop_egress_driver(&name).await;
                self.ensure_egress_driver(&name, &app.deployment())
                    .await
                    .map_err(|e| format!("{driver}: {e}"))?;
                restarted.push(name);
            }
        }
        Ok(restarted)
    }

    /// Remove `name`'s egress driver, if it runs.
    pub(super) async fn stop_egress_driver(&self, name: &str) {
        let driver = bulkhead_host::egress_driver_app(name);
        if self.apps.app(&driver).is_some()
            && let Err(e) = self.apps.remove(&driver).await
        {
            tracing::warn!(app = %name, error = %e, "egress driver left running");
        }
    }
}
