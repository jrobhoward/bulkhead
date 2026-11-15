//! The Modbus drivers (ROADMAP DA5): from node config's
//! `[extensions."bulkhead-modbus:modbus"]`, the node starts
//! `bulkhead-modbus` for the devices, `bulkhead-modbus-primary` (the same
//! program, placed as a singleton) for `primary_only` devices, and
//! `bulkhead-modbus-server` for the server, so no Modbus byte is parsed in
//! the node.
//!
//! The node reads only what it needs of the section: each device's address
//! (the one place its driver may have the node connect it), timeout,
//! `primary_only` and `deliver_to`, and the server's `listen` (bound here,
//! and passed) and `deliver_to`. The drivers get their part of the section
//! as it is and check all of it as they start. Each driver's casts go to
//! the `deliver_to` apps alone, whatever those accept from: the operator
//! named them. Apps' calls are checked by the node and forwarded
//! (`bulkhead_host::services::modbus`).

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::time::Duration;

use bulkhead_bundle::manifest::{AppManifest, NativeProgram};
use bulkhead_host::services::modbus::{
    MODBUS_DEVICES_APP, MODBUS_PACKAGE, MODBUS_PRIMARY_APP, MODBUS_SERVER_APP,
};
use bulkhead_host::{
    AppDeployment, Apps, DriverEndpoint, DriverListener, DriverProfile, ModbusDrivers, NativeCode,
    Runtime,
};
use serde::Deserialize;

use super::builtin;
use crate::node::config::{ClusterRole, MODBUS_PROGRAM, MODBUS_SERVER_PROGRAM, NodeConfig};
use crate::node::error::NodeError;

/// The name of the listening socket the server driver finds its masters
/// on (`bulkhead_modbus::LISTENER`).
const LISTENER: &str = "modbus";

/// What the node reads of a device; the driver reads the rest.
#[derive(Debug, Deserialize)]
struct Device {
    address: SocketAddr,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    #[serde(default)]
    primary_only: bool,
    #[serde(default)]
    deliver_to: Option<String>,
}

/// What the node reads of the server.
#[derive(Debug, Deserialize)]
struct Server {
    listen: SocketAddr,
    #[serde(default)]
    deliver_to: Option<String>,
}

fn default_timeout_ms() -> u64 {
    1_000
}

/// One driver to start: its app, its start arguments, and whether it runs
/// as a singleton.
#[derive(Debug)]
pub(super) struct Start {
    app: &'static str,
    args: serde_json::Value,
    singleton: bool,
}

/// The Modbus drivers a node's config asks for: their profiles (added to
/// the runtime's drivers), what the node's checks need, and what to start.
#[derive(Debug, Default)]
pub(super) struct Bound {
    pub(super) drivers: ModbusDrivers,
    starts: Vec<Start>,
}

fn invalid(what: impl std::fmt::Display) -> NodeError {
    NodeError::Invalid(format!(
        "node config: extensions.\"{MODBUS_PACKAGE}\": {what}"
    ))
}

/// The apps `devices` deliver to, as a profile's `calls`.
fn delivered<'a>(apps: impl Iterator<Item = &'a Option<String>>) -> Vec<String> {
    apps.flatten()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Read the section, bind the server's port, and add the drivers'
/// profiles to `profiles`. `None` without the section, and on a witness,
/// which runs no apps.
///
/// # Errors
///
/// The section's devices or server cannot be read, or the server's port
/// cannot be bound.
pub(super) fn bind(
    config: &NodeConfig,
    profiles: &mut BTreeMap<String, DriverProfile>,
) -> Result<Option<Bound>, NodeError> {
    let Some(section) = config.extensions.get(MODBUS_PACKAGE) else {
        return Ok(None);
    };
    if config.cluster.role == ClusterRole::Witness || section.is_null() {
        return Ok(None);
    }
    let raw_devices = section
        .get("devices")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut bound = Bound::default();
    let mut groups: [(BTreeMap<String, Device>, serde_json::Map<_, _>); 2] = Default::default();
    for (name, raw) in &raw_devices {
        let device: Device = serde_json::from_value(raw.clone())
            .map_err(|e| invalid(format!("devices.{name}: {e}")))?;
        let group = usize::from(device.primary_only);
        groups[group].1.insert(name.clone(), raw.clone());
        groups[group].0.insert(name.clone(), device);
    }
    let mut longest = Duration::from_millis(default_timeout_ms());
    for (primary, (devices, raw)) in groups.into_iter().enumerate() {
        if devices.is_empty() {
            continue;
        }
        let app = if primary == 1 {
            MODBUS_PRIMARY_APP
        } else {
            MODBUS_DEVICES_APP
        };
        let mut endpoints = Vec::new();
        for device in devices.values() {
            let timeout = Duration::from_millis(device.timeout_ms.max(1));
            longest = longest.max(timeout);
            endpoints.push(DriverEndpoint::Tcp {
                host: device.address.ip().to_string(),
                port: device.address.port(),
                timeout,
            });
        }
        profiles.insert(
            app.to_string(),
            DriverProfile {
                endpoints,
                calls: delivered(devices.values().map(|d| &d.deliver_to)),
                delivers: true,
                ..DriverProfile::default()
            },
        );
        let names = devices.keys().cloned();
        if primary == 1 {
            bound.drivers.primary.extend(names);
        } else {
            bound.drivers.devices.extend(names);
        }
        bound.starts.push(Start {
            app,
            args: serde_json::json!({ "devices": raw }),
            singleton: primary == 1,
        });
    }
    if let Some(raw) = section.get("server").filter(|s| !s.is_null()) {
        let server: Server =
            serde_json::from_value(raw.clone()).map_err(|e| invalid(format!("server: {e}")))?;
        let socket = std::net::TcpListener::bind(server.listen)
            .map_err(|e| invalid(format!("server: cannot listen on {}: {e}", server.listen)))?;
        profiles.insert(
            MODBUS_SERVER_APP.to_string(),
            DriverProfile {
                listeners: vec![DriverListener {
                    name: LISTENER.into(),
                    socket: std::sync::Arc::new(socket),
                }],
                calls: delivered(std::iter::once(&server.deliver_to)),
                delivers: true,
                ..DriverProfile::default()
            },
        );
        bound.drivers.server = true;
        bound.starts.push(Start {
            app: MODBUS_SERVER_APP,
            args: serde_json::json!({ "server": raw }),
            singleton: false,
        });
    }
    // A forwarded call runs a request (a connect and a read, each within
    // the device's timeout) behind any poll or request in progress.
    bound.drivers.timeout = longest * 4 + Duration::from_secs(1);
    Ok(Some(bound))
}

/// A driver's manifest: one instance (or a singleton); messages from
/// itself alone, so no app reaches it but through the node's checks;
/// restarts that never give up.
fn manifest(start: &Start) -> Result<AppManifest, NodeError> {
    let app = start.app;
    let mut spec = serde_json::json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "instances": { "count": 1 },
        "lifecycle": { "start_args": start.args },
        "supervision": builtin::supervision(),
        "limits": { "memory_max": "256MiB" },
        "messaging_acl": { "accept_from": [app], "may_call": [app] },
    });
    if start.singleton {
        spec["placement"] = serde_json::json!({ "mode": "singleton" });
    }
    builtin::manifest(app, "Modbus TCP outside the node", &spec)
}

/// Where a program is: its `[native]` key, or beside the runner.
fn program(
    config: &NodeConfig,
    explicit: Option<&std::path::PathBuf>,
    name: &str,
) -> Result<std::path::PathBuf, NodeError> {
    let program = builtin::program(config, explicit, name);
    if !program.is_file() {
        return Err(NodeError::Invalid(format!(
            "[extensions.\"{MODBUS_PACKAGE}\"] needs {name}, and {} is not it (install it \
             beside bulkhead-runner, or set [native] modbus or modbus_server)",
            program.display()
        )));
    }
    Ok(program)
}

/// What the runtime's checks of apps' calls need, if the node runs the
/// drivers.
pub(super) fn drivers(bound: Option<&Bound>) -> Option<ModbusDrivers> {
    bound.map(|b| b.drivers.clone())
}

/// Start the drivers `bound` names, if any, before the node's apps. Each
/// is tried once on its own first; the singleton with no devices, so a
/// standby polls nothing even then.
///
/// # Errors
///
/// A program is missing, or a driver did not start.
pub(super) async fn start(
    config: &NodeConfig,
    bound: Option<&Bound>,
    rt: &Runtime,
    apps: &Apps,
) -> Result<(), NodeError> {
    let Some(bound) = bound else {
        return Ok(());
    };
    for start in &bound.starts {
        let path = if start.app == MODBUS_SERVER_APP {
            program(
                config,
                config.native.modbus_server.as_ref(),
                MODBUS_SERVER_PROGRAM,
            )?
        } else {
            program(config, config.native.modbus.as_ref(), MODBUS_PROGRAM)?
        };
        let code = rt.load_native(NativeCode {
            path,
            kind: NativeProgram::Executable,
        });
        let deployment = AppDeployment::new(manifest(start)?, code.clone());
        if start.singleton {
            let trial = Start {
                app: start.app,
                args: serde_json::json!({ "devices": {} }),
                singleton: false,
            };
            builtin::preflight(rt, &AppDeployment::new(manifest(&trial)?, code)).await?;
            builtin::add(apps, deployment).await?;
        } else {
            builtin::start(rt, apps, deployment).await?;
        }
        tracing::info!(driver = start.app, "modbus driver started");
    }
    Ok(())
}
