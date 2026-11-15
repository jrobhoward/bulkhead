//! Address networks (`10.0.0.0/24`), for the server's `allow` list.

use std::net::IpAddr;

/// `addr/prefix` as an address and a prefix length.
pub(crate) fn parse(entry: &str) -> Option<(IpAddr, u8)> {
    let (addr, prefix) = entry.split_once('/')?;
    let addr: IpAddr = addr.parse().ok()?;
    let prefix: u8 = prefix.parse().ok()?;
    let max = if addr.is_ipv4() { 32 } else { 128 };
    (prefix <= max).then_some((addr, prefix))
}

/// Whether `ip` is in the network `(net, prefix)`.
pub(crate) fn contains((net, prefix): (IpAddr, u8), ip: IpAddr) -> bool {
    match (net, ip) {
        (IpAddr::V4(n), IpAddr::V4(a)) => {
            let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            u32::from(n) & mask == u32::from(a) & mask
        }
        (IpAddr::V6(n), IpAddr::V6(a)) => {
            let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
            u128::from(n) & mask == u128::from(a) & mask
        }
        _ => false,
    }
}

#[cfg(test)]
#[path = "cidr_tests.rs"]
mod cidr_tests;
