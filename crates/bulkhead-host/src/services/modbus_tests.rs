#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::time::Duration;

use bulkhead_native::protocol::ExtensionError;
use rstest::rstest;
use serde_json::json;

use super::{MODBUS_DEVICES_APP, MODBUS_PRIMARY_APP, MODBUS_SERVER_APP, Route, route};
use crate::config::ModbusDrivers;

fn drivers(server: bool) -> ModbusDrivers {
    let mut d = ModbusDrivers::default();
    d.devices.insert("pump-1".into());
    d.primary.insert("valve-1".into());
    d.server = server;
    d.timeout = Duration::from_secs(5);
    d
}

fn args(device: &str) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::into_writer(&(device, 0u16, 1u16), &mut out).unwrap();
    out
}

/// A node's own answer, decoded as `Result<(), E>` with `E` a string.
fn refused(r: Route) -> String {
    match r {
        Route::Answer(bytes) => {
            let v: Result<(), ciborium::Value> = ciborium::from_reader(bytes.as_slice()).unwrap();
            format!("{:?}", v.unwrap_err())
        }
        Route::Driver(d) => panic!("forwarded to {d}"),
    }
}

#[rstest]
#[case::read_listed("client#read-holding", "pump-1", json!({ "read": ["pump-1"] }), MODBUS_DEVICES_APP)]
#[case::write_also_reads("client#read-coils", "pump-1", json!({ "write": ["pump-1"] }), MODBUS_DEVICES_APP)]
#[case::write_listed("client#write-holding", "pump-1", json!({ "write": ["pump-1"] }), MODBUS_DEVICES_APP)]
#[case::write_tag_listed("client#write-tag", "pump-1", json!({ "write": ["pump-1"] }), MODBUS_DEVICES_APP)]
#[case::primary_device("client#read-input", "valve-1", json!({ "read": ["valve-1"] }), MODBUS_PRIMARY_APP)]
#[case::server("server#set-holding", "", json!({ "serve": true }), MODBUS_SERVER_APP)]
fn route____allowed____to_its_driver(
    #[case] function: &str,
    #[case] device: &str,
    #[case] scope: serde_json::Value,
    #[case] driver: &str,
) {
    let found = route(&drivers(true), Some(&scope), true, function, &args(device)).unwrap();

    assert_eq!(
        found,
        Route::Driver(match driver {
            MODBUS_DEVICES_APP => MODBUS_DEVICES_APP,
            MODBUS_PRIMARY_APP => MODBUS_PRIMARY_APP,
            _ => MODBUS_SERVER_APP,
        })
    );
}

#[rstest]
#[case::read_only_writes("client#write-coils", json!({ "read": ["pump-1"] }), true, "AccessDenied")]
#[case::read_only_writes_a_tag("client#write-tag", json!({ "read": ["pump-1"] }), true, "AccessDenied")]
#[case::unlisted("client#read-holding", json!({ "read": ["other"] }), true, "AccessDenied")]
#[case::ungranted("client#read-holding", json!({ "read": ["pump-1"] }), false, "AccessDenied")]
#[case::no_scope("client#read-holding", serde_json::Value::Null, true, "AccessDenied")]
fn route____client_refused____access_denied(
    #[case] function: &str,
    #[case] scope: serde_json::Value,
    #[case] granted: bool,
    #[case] error: &str,
) {
    let found = route(
        &drivers(true),
        Some(&scope),
        granted,
        function,
        &args("pump-1"),
    )
    .unwrap();

    assert!(refused(found).contains(error));
}

#[test]
fn route____unknown_device____no_such_device() {
    let scope = json!({ "read": ["ghost"] });

    let found = route(
        &drivers(true),
        Some(&scope),
        true,
        "client#read-holding",
        &args("ghost"),
    )
    .unwrap();

    assert!(refused(found).contains("NoSuchDevice"));
}

#[test]
fn route____server_without_serve_or_server____refused() {
    let no_serve = route(
        &drivers(true),
        Some(&json!({})),
        true,
        "server#get-coils",
        &[],
    )
    .unwrap();
    let no_server = route(
        &drivers(false),
        Some(&json!({ "serve": true })),
        true,
        "server#get-coils",
        &[],
    )
    .unwrap();

    assert!(refused(no_serve).contains("AccessDenied"));
    assert!(refused(no_server).contains("NotServing"));
}

#[test]
fn route____unknown_function_or_no_device____errors() {
    let unknown = route(&drivers(true), None, true, "client#reboot", &args("pump-1"));
    let no_device = route(&drivers(true), None, true, "client#read-holding", &[0x80]);

    assert_eq!(
        unknown,
        Err(ExtensionError::NoSuchFunction("client#reboot".into()))
    );
    assert!(matches!(no_device, Err(ExtensionError::BadArguments(_))));
}
