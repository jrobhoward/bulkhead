//! An app's outgoing HTTP outside the node (ROADMAP DA4): hyper and rustls
//! in a process of their own, one per app granted `bulkhead:http-egress`,
//! so no response or TLS handshake from a server an app may reach is
//! parsed in the node that holds every secret.
//!
//! The node starts it as the app `bulkhead-egress-<app>`, with node
//! config's egress settings and the trusted roots (PEM) as init arguments.
//! The node checks each request (the grant, the manifest's `allow` and
//! `methods`, forbidden headers, the app's requests in flight) before it
//! calls the driver, and opens every connection the driver asks for
//! (`Request::ConnectTo`) only where the app may send a request, so a
//! compromised driver holds one app's requests and responses and reaches
//! that app's destinations, nothing else.
//!
//! - [`config`]: the init arguments.
//! - [`request`]: the node's call and the driver's answer.
//! - [`client`]: the pool over the node's streams, TLS, timeouts, the
//!   body cap, and failures.
//! - [`driver`]: the app, its worker thread, and the requests.

pub mod client;
pub mod config;
pub mod driver;
pub mod request;

pub use driver::Egress;
pub use request::{EGRESS_SCHEMA, EgressRequest};
