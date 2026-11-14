//! The HTTP ingress as a driver app (ROADMAP DA2): hyper and rustls in a
//! process of their own, outside the node that holds every secret.
//!
//! The node starts it from node config's `[http.<name>]` blocks, binds
//! and passes its listening sockets, gives it its certificate files under
//! `/driver/` and sends it the route table (the host event `routes`). It
//! accepts and reads its connections itself, and makes each request a
//! call to the app owning its route, by name, so the node picks the
//! instance by the app's `instances.routing` and checks messaging rules
//! as for any call. The request and response formats are
//! `bulkhead_sdk::http`'s.
//!
//! - [`config`]: the init arguments the node writes.
//! - [`serve`]: one request, its limits, its statuses.
//! - [`tls`]: certificates by SNI, reloaded from their files.
//! - [`driver`]: the app, its worker threads and host events.

pub mod config;
pub mod driver;
pub mod serve;
pub mod tls;

pub use driver::Ingress;
