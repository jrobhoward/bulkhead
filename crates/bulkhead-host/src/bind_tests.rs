#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn bind_tcp____free_port_no_helper____bound_by_the_node() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

    let listener = bind_tcp(addr, &NativeConfig::default()).unwrap();

    assert_ne!(listener.local_addr().unwrap().port(), 0);
}

#[test]
fn bind_tcp____low_port_no_helper____the_systems_refusal() {
    let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
    // Root, or a system that lets anyone bind low ports, binds it.
    if TcpListener::bind(addr).is_ok() {
        eprintln!("port 1 is open to this user here: skipped");
        return;
    }

    let refused = bind_tcp(addr, &NativeConfig::default()).unwrap_err();

    assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
}

#[test]
fn bind_udp____low_port_no_helper____the_error_unchanged() {
    let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
    if UdpSocket::bind(addr).is_ok() {
        eprintln!("port 1 is open to this user here: skipped");
        return;
    }

    let refused = bind_udp(addr, &NativeConfig::default()).unwrap_err();

    assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
    assert!(!refused.to_string().contains("helper"), "{refused}");
}
