//! `s40 probe --allowed A --other O --self S [--v6]`: the enforcement
//! matrix, one line per case. Run inside a fence with `rules/probe*.toml`:
//! `A` the target its rules allow (ports 7001 and, as the guard's case, a
//! broad rule's 7100), `O` an address they do not, `S` its own address,
//! where it binds 9000 to 9009 and connects to itself.

use std::io;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::net;

fn show(case: &str, r: io::Result<String>) {
    match r {
        Ok(s) => println!("{case:<52} ok {s}"),
        Err(e) => println!("{case:<52} refused {}", net::name(&e)),
    }
}

fn tcp(to: SocketAddr) -> io::Result<String> {
    TcpStream::connect_timeout(&to, Duration::from_millis(800)).map(|_| String::new())
}

fn udp_sendto(to: SocketAddr) -> io::Result<String> {
    let s = net::socket(net::family(to), libc::SOCK_DGRAM, 0)?;
    if to.is_ipv6() {
        net::dual_stack(&s)?;
    }
    net::sendto(&s, b"x", 0, to).map(|n| format!("{n} B"))
}

/// A TCP connect from a dual-stack IPv6 socket (to a mapped address).
fn tcp6(to: SocketAddr) -> io::Result<String> {
    let s = net::socket(libc::AF_INET6, libc::SOCK_STREAM, 0)?;
    net::dual_stack(&s)?;
    net::connect(&s, to).map(|()| String::new())
}

fn arg(args: &[String], name: &str, default: &str) -> IpAddr {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map_or(default, String::as_str)
        .parse()
        .unwrap()
}

pub fn main(args: &[String]) {
    let a = arg(args, "--allowed", "127.0.0.1");
    let o = arg(args, "--other", "127.0.0.2");
    let me = arg(args, "--self", "127.0.0.1");
    let v6 = args.iter().any(|x| x == "--v6");
    let at = |ip: IpAddr, port| SocketAddr::new(ip, port);
    let mapped = |ip: IpAddr, port| match ip {
        IpAddr::V4(v4) => SocketAddr::new(IpAddr::V6(v4.to_ipv6_mapped()), port),
        v6 => SocketAddr::new(v6, port),
    };

    println!("# TCP connect");
    show(&format!("tcp {a}:7001 (allowed)"), tcp(at(a, 7001)));
    show(
        &format!("tcp {a}:7002 (port not in rules)"),
        tcp(at(a, 7002)),
    );
    show(
        &format!("tcp {o}:7001 (address not in rules)"),
        tcp(at(o, 7001)),
    );
    show(
        &format!("tcp {a}:7100 (broad rule; guard if loopback)"),
        tcp(at(a, 7100)),
    );
    show(
        "tcp 169.254.169.254:7100 (broad rule, link-local)",
        tcp(at("169.254.169.254".parse().unwrap(), 7100)),
    );
    show(
        "tcp 0.0.0.0:7100 (broad rule, unspecified)",
        tcp(at("0.0.0.0".parse().unwrap(), 7100)),
    );
    show(
        &format!("tcp {} (mapped, allowed)", mapped(a, 7001)),
        tcp6(mapped(a, 7001)),
    );
    show(
        &format!("tcp {} (mapped, port not in rules)", mapped(a, 7002)),
        tcp6(mapped(a, 7002)),
    );
    if v6 {
        show(
            "tcp [::1]:7001 (IPv6 rule)",
            tcp("[::1]:7001".parse().unwrap()),
        );
        show(
            "tcp [::1]:7002 (IPv6, port not in rules)",
            tcp("[::1]:7002".parse().unwrap()),
        );
    }
    #[cfg(target_os = "linux")]
    show(
        &format!("tcp fast open {a}:7002 (sendto, port not in rules)"),
        {
            net::socket(net::family(at(a, 7002)), libc::SOCK_STREAM, 0)
                .and_then(|s| net::sendto(&s, b"x", libc::MSG_FASTOPEN, at(a, 7002)))
                .map(|n| format!("{n} B"))
        },
    );

    println!("# UDP");
    show(
        &format!("udp sendto {a}:7001 (allowed)"),
        udp_sendto(at(a, 7001)),
    );
    show(
        &format!("udp sendto {a}:7002 (port not in rules)"),
        udp_sendto(at(a, 7002)),
    );
    show(
        &format!("udp sendto {o}:7001 (address not in rules)"),
        udp_sendto(at(o, 7001)),
    );
    show(
        &format!("udp sendto {} (mapped, port not in rules)", mapped(a, 7002)),
        udp_sendto(mapped(a, 7002)),
    );
    show(&format!("udp connect {a}:7002 (port not in rules)"), {
        net::socket(net::family(at(a, 7002)), libc::SOCK_DGRAM, 0)
            .and_then(|s| net::connect(&s, at(a, 7002)).map(|()| String::new()))
    });
    show(&format!("udp connected to {a}:7001, sendto {a}:7002"), {
        net::socket(net::family(at(a, 7001)), libc::SOCK_DGRAM, 0).and_then(|s| {
            net::connect(&s, at(a, 7001))?;
            net::sendto(&s, b"x", 0, at(a, 7002)).map(|n| format!("{n} B"))
        })
    });

    println!("# bind and listen");
    let any = |port| {
        SocketAddr::new(
            if me.is_ipv4() {
                "0.0.0.0".parse().unwrap()
            } else {
                "::".parse().unwrap()
            },
            port,
        )
    };
    for (case, port, ty) in [
        ("tcp bind :9001 (in rules)", 9001, libc::SOCK_STREAM),
        ("tcp bind :9100 (not in rules)", 9100, libc::SOCK_STREAM),
        ("tcp bind :0 (ephemeral)", 0, libc::SOCK_STREAM),
        ("udp bind :9001 (in rules)", 9001, libc::SOCK_DGRAM),
        ("udp bind :9100 (not in rules)", 9100, libc::SOCK_DGRAM),
    ] {
        show(
            case,
            net::socket(net::family(any(port)), ty, 0)
                .and_then(|s| net::bind(&s, any(port)).map(|()| String::new())),
        );
    }
    show(&format!("listen :9002, connect {me}:9002, accept"), {
        TcpListener::bind(any(9002)).and_then(|l| {
            let _c = TcpStream::connect_timeout(&at(me, 9002), Duration::from_millis(800))?;
            l.accept().map(|(_, from)| format!("from {from}"))
        })
    });

    println!("# other domains and types");
    for (case, d, t) in [
        ("socket AF_INET SOCK_RAW", libc::AF_INET, libc::SOCK_RAW),
        #[cfg(target_os = "linux")]
        (
            "socket AF_PACKET SOCK_DGRAM",
            libc::AF_PACKET,
            libc::SOCK_DGRAM,
        ),
        #[cfg(target_os = "linux")]
        (
            "socket AF_NETLINK SOCK_DGRAM",
            libc::AF_NETLINK,
            libc::SOCK_DGRAM,
        ),
        #[cfg(target_os = "freebsd")]
        ("socket PF_ROUTE SOCK_RAW", libc::PF_ROUTE, libc::SOCK_RAW),
    ] {
        let proto = if t == libc::SOCK_RAW && d == libc::AF_INET {
            libc::IPPROTO_ICMP
        } else {
            0
        };
        show(case, net::socket(d, t, proto).map(|_| String::new()));
    }
    #[cfg(target_os = "linux")]
    show("unix connect @s40-outside (abstract, outside)", {
        use std::os::linux::net::SocketAddrExt;
        std::os::unix::net::SocketAddr::from_abstract_name(b"s40-outside")
            .and_then(|n| std::os::unix::net::UnixStream::connect_addr(&n))
            .map(|_| String::new())
    });

    println!("# names");
    show(
        "getaddrinfo localhost",
        "localhost:1"
            .to_socket_addrs()
            .map(|v| format!("{:?}", v.collect::<Vec<_>>())),
    );
    show(
        "getaddrinfo example.com (DNS)",
        "example.com:443"
            .to_socket_addrs()
            .map(|v| format!("{} addresses", v.count())),
    );

    println!("# inbound");
    show(
        &format!("udp bind :9003, datagram from {o} (outside the rules)"),
        {
            std::net::UdpSocket::bind(any(9003)).and_then(|s| {
                s.set_read_timeout(Some(Duration::from_millis(1500)))?;
                let mut b = [0u8; 64];
                s.recv_from(&mut b)
                    .map(|(n, from)| format!("{n} B from {from}"))
            })
        },
    );
}
