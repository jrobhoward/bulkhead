//! `s40 bench --target A`: costs inside a fence. Connect and close (reset,
//! so no `TIME_WAIT`) to `A:7001`; unconnected UDP `sendto` of 64 bytes
//! to `A:7001`; 256 MiB over one TCP connection to the sink at `A:7004`.
//!
//! `s40 passed --target A`: the same connect and UDP through the
//! launcher, standing in for node-made sockets: fd 3 asks for a connected
//! stream and gets it over `SCM_RIGHTS`; fd 4 takes datagrams the
//! launcher sends on.

use std::io::Write;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use crate::net;

fn target(args: &[String]) -> IpAddr {
    args.iter()
        .position(|a| a == "--target")
        .and_then(|i| args.get(i + 1))
        .map_or("127.0.0.1", String::as_str)
        .parse()
        .unwrap()
}

fn report(what: &str, mut times: Vec<Duration>, total: Duration) {
    times.sort();
    let at = |q: f64| times[((times.len() - 1) as f64 * q) as usize];
    println!(
        "{what}: p50 {:.1} µs, p99 {:.1} µs, {:.0} per second ({} runs)",
        at(0.5).as_secs_f64() * 1e6,
        at(0.99).as_secs_f64() * 1e6,
        times.len() as f64 / total.as_secs_f64(),
        times.len()
    );
}

const CONNECTS: usize = 20_000;
const DATAGRAMS: usize = 200_000;

pub fn main(args: &[String]) {
    let a = target(args);
    let to = SocketAddr::new(a, 7001);
    // Warm up: the first connects pay for route and neighbour caches.
    for _ in 0..200 {
        let c = TcpStream::connect(to).unwrap();
        net::reset_on_close(&c);
    }
    let mut times = Vec::with_capacity(CONNECTS);
    let all = Instant::now();
    for _ in 0..CONNECTS {
        let t = Instant::now();
        let c = TcpStream::connect(to).unwrap();
        net::reset_on_close(&c);
        drop(c);
        times.push(t.elapsed());
    }
    report("tcp connect+close", times, all.elapsed());

    let s = net::socket(net::family(to), libc::SOCK_DGRAM, 0).unwrap();
    let buf = [7u8; 64];
    let t = Instant::now();
    for _ in 0..DATAGRAMS {
        net::sendto(&s, &buf, 0, to).unwrap();
    }
    let e = t.elapsed();
    println!(
        "udp sendto 64 B: {:.0} ns per datagram ({DATAGRAMS} datagrams)",
        e.as_nanos() as f64 / DATAGRAMS as f64
    );

    let mut c = TcpStream::connect(SocketAddr::new(a, 7004)).unwrap();
    let chunk = vec![1u8; 1 << 16];
    let total = 256usize << 20;
    let t = Instant::now();
    let mut sent = 0;
    while sent < total {
        c.write_all(&chunk).unwrap();
        sent += chunk.len();
    }
    let e = t.elapsed();
    println!(
        "tcp bulk 256 MiB: {:.0} MB/s",
        total as f64 / e.as_secs_f64() / 1e6
    );
}

/// Receive one descriptor sent with `SCM_RIGHTS` on `s`.
fn recv_fd(s: &OwnedFd) -> std::io::Result<OwnedFd> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(size_of::<i32>() as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    // SAFETY: all-zero is a valid `msghdr`; pointers set to live buffers.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: `msg` is valid for the call.
    if unsafe { libc::recvmsg(s.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) } <= 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the kernel filled one SCM_RIGHTS header with one int.
    let fd = unsafe {
        std::ptr::read_unaligned(libc::CMSG_DATA(libc::CMSG_FIRSTHDR(&msg)).cast::<i32>())
    };
    // SAFETY: a new descriptor we own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub fn passed(_args: &[String]) {
    // SAFETY: the launcher set fds 3 and 4 before exec.
    let (stream, dgram) = unsafe { (OwnedFd::from_raw_fd(3), OwnedFd::from_raw_fd(4)) };
    let mut ask = std::os::unix::net::UnixStream::from(stream.try_clone().unwrap());
    let mut get = || {
        ask.write_all(b"c").unwrap();
        drop(recv_fd(&stream).unwrap());
    };
    for _ in 0..200 {
        get();
    }
    let mut times = Vec::with_capacity(CONNECTS);
    let all = Instant::now();
    for _ in 0..CONNECTS {
        let t = Instant::now();
        get();
        times.push(t.elapsed());
    }
    report("tcp connect by the launcher, passed", times, all.elapsed());

    let d = std::os::unix::net::UnixDatagram::from(dgram);
    let buf = [7u8; 64];
    let t = Instant::now();
    for _ in 0..DATAGRAMS {
        // FreeBSD refuses a datagram with `ENOBUFS` while the receiver's
        // buffer is full, where Linux blocks: wait the same way.
        loop {
            match d.send(&buf) {
                Ok(_) => break,
                Err(e) if e.raw_os_error() == Some(libc::ENOBUFS) => std::thread::yield_now(),
                Err(e) => panic!("{e}"),
            }
        }
    }
    let e = t.elapsed();
    println!(
        "udp through the launcher 64 B: {:.0} ns per datagram ({DATAGRAMS} datagrams)",
        e.as_nanos() as f64 / DATAGRAMS as f64
    );
}
