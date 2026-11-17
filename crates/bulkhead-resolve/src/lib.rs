//! Host names resolved outside the node (ROADMAP DA9): hickory-resolver in
//! a process of its own, so no DNS reply is parsed in the node that holds
//! every secret.
//!
//! The node starts it on every member, with the nameservers it may reach
//! (connect rules in its manifest) and the contents of `resolv.conf` and
//! `hosts` as init arguments. It asks the node for a TCP stream to a
//! nameserver when it needs one (DNS over TCP only: a UDP socket could
//! send anywhere), and answers the node's lookups, a call each, with
//! addresses the node then checks against the asking app's rules.
//!
//! - [`config`]: the init arguments, and hickory's configuration from them.
//! - [`lookup`]: a lookup's request and answer, and what a failure is.
//! - [`provider`]: hickory's I/O over streams the node opens.
//! - [`driver`]: the app, its worker thread, and the hand-back to the
//!   callback thread.

#![forbid(unsafe_code)]

pub mod config;
pub mod driver;
pub mod lookup;
pub mod provider;

pub use driver::Resolve;
