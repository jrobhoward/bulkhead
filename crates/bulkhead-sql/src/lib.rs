//! A datasource's Postgres client outside the node (ROADMAP DA3):
//! tokio-postgres in a process of its own, one per `[datasources.<name>]`,
//! so no reply from a database server is parsed in the node that holds
//! every secret, and no datasource's password is read by it.
//!
//! The node starts it as the app `bulkhead-sql-<datasource>`, with the
//! datasource's settings as init arguments and its password file and CA
//! certificates readable under `/driver/`. The driver opens no socket: it
//! asks the node for a stream to the datasource's one address
//! (`Request::Connect`), which the node connects, resolving a host name
//! each time, so a compromised driver reaches its own database and nothing
//! else. The node keeps each app's grant, its connection cap and its
//! transactions; the driver holds the pool and runs statements.
//!
//! - [`config`]: the init arguments.
//! - [`pool`]: the pool over the node's streams, TLS, and the reset.
//! - [`statements`]: running statements, and their failures.
//! - [`driver`]: the app, its worker thread, and the requests.

#![forbid(unsafe_code)]

pub mod config;
pub mod driver;
pub mod pool;
pub mod statements;
mod values;

pub use driver::{SQL_SCHEMA, Sql};
