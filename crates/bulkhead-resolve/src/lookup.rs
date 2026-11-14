//! A lookup as the node asks for it and the driver answers it: a call with
//! schema [`RESOLVE_SCHEMA`], JSON both ways. The node keeps its own copy
//! of these shapes (`bulkhead_host`), since it does not link this crate.

use std::net::IpAddr;

use hickory_resolver::net::NetError;
use hickory_resolver::proto::rr::Name;
use serde::{Deserialize, Serialize};

/// The schema of the node's call asking for a name's addresses.
pub const RESOLVE_SCHEMA: &str = "bulkhead/resolve";

/// The longest name looked up, in bytes (RFC 1035's 253 for the text
/// form).
pub const MAX_NAME: usize = 253;

/// The node's request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// A host name, not an address: the node answers those itself.
    pub host: String,
}

/// Why a lookup found no addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Failure {
    /// The name does not exist, or has no address records.
    NotFound,
    /// No nameserver answered in time, or none could be reached: trying
    /// again may work.
    Temporary,
    /// Not a host name.
    Invalid,
}

/// The driver's answer: addresses, or why none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    /// The addresses, in the order the resolver gave them.
    #[serde(default)]
    pub addrs: Vec<IpAddr>,
    /// Set when `addrs` is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    /// The resolver's words, for the node's log.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

impl Answer {
    /// Addresses found.
    #[must_use]
    pub fn found(addrs: Vec<IpAddr>) -> Self {
        if addrs.is_empty() {
            return Self::failed(Failure::NotFound, "no addresses".into());
        }
        Self {
            addrs,
            failure: None,
            detail: String::new(),
        }
    }

    /// None found, and why.
    #[must_use]
    pub fn failed(failure: Failure, detail: String) -> Self {
        Self {
            addrs: Vec::new(),
            failure: Some(failure),
            detail,
        }
    }
}

/// `host` as a name to look up: a host name of at most [`MAX_NAME`]
/// bytes, not an address.
///
/// # Errors
///
/// Why it is not one.
pub fn name(host: &str) -> Result<Name, String> {
    if host.is_empty() || host.len() > MAX_NAME {
        return Err(format!("a host name has 1 to {MAX_NAME} bytes"));
    }
    if host.parse::<IpAddr>().is_ok() {
        return Err("an address, not a host name".into());
    }
    Name::from_utf8(host).map_err(|e| e.to_string())
}

/// What a resolver's error means for the asker.
#[must_use]
pub fn failure(e: &NetError) -> Failure {
    if e.is_nx_domain() || e.is_no_records_found() {
        Failure::NotFound
    } else {
        Failure::Temporary
    }
}

#[cfg(test)]
#[path = "lookup_tests.rs"]
mod tests;
