//! Binding the node's sockets (ROADMAP PS3b). Under the privileged
//! helper the node holds no capabilities, so the system refuses it a port
//! below 1024 (or below `net.ipv4.ip_unprivileged_port_start`; FreeBSD's
//! `net.inet.ip.portrange.reservedhigh`). The node binds as usual and,
//! when the system refuses it a TCP port and a helper started it, asks the
//! helper for that address; the helper binds only the ports its own
//! configuration lists. UDP is not passed by the helper: a refused UDP
//! port stays refused, with an error saying so.

use std::io;
use std::net::{SocketAddr, TcpListener, UdpSocket};

use crate::config::NativeConfig;

/// A TCP listener on `addr`: bound by the node, or, when the system
/// refuses the node the port and `native` has the privileged helper, by
/// the helper.
///
/// # Errors
///
/// The node's bind failed, and either there is no helper or the helper
/// refused (its reason in the message).
pub fn bind_tcp(addr: SocketAddr, native: &NativeConfig) -> io::Result<TcpListener> {
    match TcpListener::bind(addr) {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            if let Some(helper) = &native.helper {
                tracing::info!(%addr, "asking the privileged helper to bind a port");
                return helper.bind(addr);
            }
            let _ = native;
            Err(e)
        }
        bound => bound,
    }
}

/// A UDP socket on `addr`, bound by the node; under the helper a refused
/// port says that the helper binds TCP ports only.
///
/// # Errors
///
/// The bind failed.
pub fn bind_udp(addr: SocketAddr, native: &NativeConfig) -> io::Result<UdpSocket> {
    UdpSocket::bind(addr).map_err(|e| udp_refused(e, native))
}

/// `e`, with a note when the system refused a UDP port under the helper.
fn udp_refused(e: io::Error, native: &NativeConfig) -> io::Error {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    if e.kind() == io::ErrorKind::PermissionDenied && native.helper.is_some() {
        return io::Error::new(
            e.kind(),
            format!("{e} (the privileged helper binds TCP ports only)"),
        );
    }
    let _ = native;
    e
}

#[cfg(test)]
#[path = "bind_tests.rs"]
mod tests;
