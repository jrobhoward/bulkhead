//! Modbus TCP for Bulkhead, outside the node (ROADMAP DA5): the drivers
//! `bulkhead-modbus` (polling devices) and `bulkhead-modbus-server`
//! (serving masters), which the node starts from its configuration:
//!
//! ```toml
//! [extensions."bulkhead-modbus:modbus".devices.pump-1]
//! address = "10.0.0.5:502"
//! unit = 1
//! poll_ms = 1000
//! timeout_ms = 1000
//! full_every_ms = 60000         # an integrity scan; optional
//! deliver_to = "scada"          # the app updates are cast to
//! primary_only = false          # on an HA pair, poll on the primary only
//! holding = [{ start = 0, count = 10 }]
//! coils = [{ start = 0, count = 16 }]
//!
//! [extensions."bulkhead-modbus:modbus".server]
//! listen = "0.0.0.0:5020"
//! deliver_to = "scada"          # masters' writes
//! holding = 100                 # table sizes
//! coils = 64
//! ```
//!
//! Every Modbus byte is parsed here, never in the node. The node opens
//! each device's connection (to its configured address alone) and binds
//! the server's port, and passes them; it runs `primary_only` devices in a
//! second devices driver, `bulkhead-modbus-primary`, placed as a singleton
//! so it polls only while its node holds the failover lease.
//!
//! Updates are CBOR casts described in [`points`]; give the receiving app
//! `instances.routing = "hash:device"` so each device stays on one
//! instance, in order. Apps read and write devices, and set the served
//! registers, through `bulkhead-modbus-client`'s `client` and `server`
//! functions, declared in the manifest and granted by the operator:
//!
//! ```toml
//! [appserver.capabilities.extensions."bulkhead-modbus:modbus"]
//! read = ["pump-1"]
//! write = ["pump-1"]
//! serve = true
//! ```
//!
//! The node checks each call against the grant and these lists and
//! forwards it to the driver (`bulkhead_host::services::modbus`).

mod calls;
mod cidr;
mod config;
mod device;
mod driver;
pub mod points;
mod server;

/// The server's request handling, for the fuzz targets in `fuzz/` only.
#[cfg(fuzzing)]
#[doc(hidden)]
pub use server::fuzz_request;

pub use config::{Config, DeviceConfig, Range, ServerConfig};
pub use driver::{Devices, LISTENER, MODBUS_SCHEMA, Server};

/// The package apps declare and operators grant.
pub const PACKAGE: &str = "bulkhead-modbus:modbus";
