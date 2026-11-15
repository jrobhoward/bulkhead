//! Apps' calls to the `bulkhead-modbus:modbus` package (ROADMAP DA5): the
//! node checks them and forwards them to the Modbus drivers, which parse
//! every Modbus byte; the node parses none.
//!
//! The checks are the extension's of before, so apps and
//! `bulkhead-modbus-client` see no change: the grant, the manifest's
//! `read` and `write` device lists (`write` also reads) for `client#…`
//! functions, `serve` for `server#…` ones, the device known to the node,
//! the server running. A refusal is answered by the node in the shape the
//! function's result has (`AccessDenied`, `NoSuchDevice`, `NotServing`).
//! An allowed call goes to `bulkhead-modbus` (the devices),
//! `bulkhead-modbus-primary` (`primary_only` devices, a singleton) or
//! `bulkhead-modbus-server`, as a call with schema [`MODBUS_SCHEMA`] whose
//! body is the CBOR pair `(function, args)`; the driver's reply body is the
//! function's result, CBOR, given to the app as it is.

#[cfg(test)]
#[path = "modbus_tests.rs"]
mod tests;

use std::collections::BTreeSet;
use std::time::Duration;

use bulkhead_native::protocol::ExtensionError;
use serde::{Deserialize, Serialize};

use crate::config::ModbusDrivers;

/// The package apps declare and operators grant.
pub const MODBUS_PACKAGE: &str = "bulkhead-modbus:modbus";
/// The schema of the node's calls to a Modbus driver and their replies.
pub const MODBUS_SCHEMA: &str = "bulkhead/modbus";
/// The driver polling the devices.
pub const MODBUS_DEVICES_APP: &str = "bulkhead-modbus";
/// The driver polling `primary_only` devices, run as a singleton.
pub const MODBUS_PRIMARY_APP: &str = "bulkhead-modbus-primary";
/// The driver serving masters.
pub const MODBUS_SERVER_APP: &str = "bulkhead-modbus-server";

/// The manifest's scope for the package.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Scope {
    read: BTreeSet<String>,
    write: BTreeSet<String>,
    serve: bool,
}

/// The `client` functions' errors the node answers itself, as
/// `bulkhead-modbus-client` reads them.
#[derive(Debug, Serialize)]
enum ClientError {
    AccessDenied,
    NoSuchDevice,
    Offline(String),
}

/// The `server` functions' errors the node answers itself.
#[derive(Debug, Serialize)]
enum ServedError {
    AccessDenied,
    NotServing,
}

const CLIENT_READS: [&str; 4] = [
    "client#read-holding",
    "client#read-input",
    "client#read-coils",
    "client#read-discrete",
];
const CLIENT_WRITES: [&str; 2] = ["client#write-holding", "client#write-coils"];
const SERVER: [&str; 6] = [
    "server#set-holding",
    "server#set-input",
    "server#set-coils",
    "server#set-discrete",
    "server#get-holding",
    "server#get-coils",
];

/// Where a call goes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Route {
    /// To this driver app.
    Driver(&'static str),
    /// Answered by the node: the function's result, CBOR.
    Answer(Vec<u8>),
}

/// An error result as the function's result, CBOR.
fn refused<E: Serialize>(error: E) -> Vec<u8> {
    let mut out = Vec::new();
    // A unit-variant enum in a `Result` always encodes.
    let _ = ciborium::into_writer(&Err::<(), E>(error), &mut out);
    out
}

/// Where `function` with `args`, called by an app with this scope and
/// grant, goes on a node running `drivers`.
///
/// # Errors
///
/// `NoSuchFunction` for a function the package does not have,
/// `BadArguments` for a `client#…` call without a device name first.
pub(crate) fn route(
    drivers: &ModbusDrivers,
    scope: Option<&serde_json::Value>,
    granted: bool,
    function: &str,
    args: &[u8],
) -> Result<Route, ExtensionError> {
    let scope: Scope = scope
        .filter(|s| !s.is_null())
        .and_then(|s| serde_json::from_value(s.clone()).ok())
        .unwrap_or_default();
    let write = CLIENT_WRITES.contains(&function);
    if write || CLIENT_READS.contains(&function) {
        let device = first_text(args)
            .ok_or_else(|| ExtensionError::BadArguments(format!("{function}: a device first")))?;
        let listed = scope.write.contains(&device) || (!write && scope.read.contains(&device));
        if !granted || !listed {
            return Ok(Route::Answer(refused(ClientError::AccessDenied)));
        }
        return Ok(if drivers.devices.contains(&device) {
            Route::Driver(MODBUS_DEVICES_APP)
        } else if drivers.primary.contains(&device) {
            Route::Driver(MODBUS_PRIMARY_APP)
        } else {
            Route::Answer(refused(ClientError::NoSuchDevice))
        });
    }
    if SERVER.contains(&function) {
        if !granted || !scope.serve {
            return Ok(Route::Answer(refused(ServedError::AccessDenied)));
        }
        return Ok(if drivers.server {
            Route::Driver(MODBUS_SERVER_APP)
        } else {
            Route::Answer(refused(ServedError::NotServing))
        });
    }
    Err(ExtensionError::NoSuchFunction(function.to_string()))
}

/// The first element of a CBOR array, if it is text.
fn first_text(args: &[u8]) -> Option<String> {
    match ciborium::from_reader::<ciborium::Value, _>(args).ok()? {
        ciborium::Value::Array(items) => match items.into_iter().next()? {
            ciborium::Value::Text(t) => Some(t),
            _ => None,
        },
        _ => None,
    }
}

/// The result for `function` when its driver could not answer: the
/// device offline for a `client#…` function, not serving for a
/// `server#…` one.
pub(crate) fn unavailable(function: &str, why: String) -> Vec<u8> {
    if function.starts_with("server#") {
        refused(ServedError::NotServing)
    } else {
        refused(ClientError::Offline(why))
    }
}

/// How often a call to a driver that is not running is tried again.
const RETRY_EVERY: Duration = Duration::from_millis(100);

/// Call `driver` with `function` and `args` as the node, and return its
/// result: tried again while the driver is not running, up to the
/// drivers' timeout.
///
/// # Errors
///
/// The driver did not answer in time, or answered something else; the
/// instance's error.
pub(crate) async fn forward(
    rt: &crate::runtime::RuntimeInner,
    drivers: &ModbusDrivers,
    driver: &str,
    function: &str,
    args: Vec<u8>,
) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    ciborium::into_writer(&(function, ciborium::Value::Bytes(args)), &mut body)
        .map_err(|e| format!("{driver}: {e}"))?;
    let mut msg = crate::message::Envelope::new("application/cbor", body);
    msg.schema = Some(MODBUS_SCHEMA.to_string());
    let deadline = tokio::time::Instant::now() + drivers.timeout;
    loop {
        if let Some(target) = rt.route_app(driver, &msg) {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let reply = target
                .call(msg, left.max(Duration::from_millis(1)))
                .await
                .map_err(|e| format!("{driver}: {e}"))?;
            if reply.schema.as_deref() != Some(MODBUS_SCHEMA) {
                return Err(format!("{driver}: not an answer"));
            }
            return Ok(reply.body);
        }
        if tokio::time::Instant::now() + RETRY_EVERY >= deadline {
            return Err(format!("{driver} is not running"));
        }
        tokio::time::sleep(RETRY_EVERY).await;
    }
}
