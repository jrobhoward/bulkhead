//! Sockets through libc where std hides what the probe needs: unbound UDP
//! sends, TCP Fast Open, raw domains, and errno names.

use std::io;
use std::net::SocketAddr;
use std::os::fd::{FromRawFd, OwnedFd};

/// `addr` as a `sockaddr_storage` and its length.
pub fn sockaddr(addr: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: all-zero is a valid `sockaddr_storage`.
    let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let len = match addr {
        SocketAddr::V4(a) => {
            // SAFETY: `sockaddr_storage` is large enough and aligned for
            // `sockaddr_in`.
            let sin = unsafe { &mut *std::ptr::from_mut(&mut ss).cast::<libc::sockaddr_in>() };
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_port = a.port().to_be();
            sin.sin_addr.s_addr = u32::from_ne_bytes(a.ip().octets());
            #[cfg(target_os = "freebsd")]
            {
                sin.sin_len = size_of::<libc::sockaddr_in>() as u8;
            }
            size_of::<libc::sockaddr_in>()
        }
        SocketAddr::V6(a) => {
            // SAFETY: as above, for `sockaddr_in6`.
            let sin6 = unsafe { &mut *std::ptr::from_mut(&mut ss).cast::<libc::sockaddr_in6>() };
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_port = a.port().to_be();
            sin6.sin6_addr.s6_addr = a.ip().octets();
            sin6.sin6_scope_id = a.scope_id();
            #[cfg(target_os = "freebsd")]
            {
                sin6.sin6_len = size_of::<libc::sockaddr_in6>() as u8;
            }
            size_of::<libc::sockaddr_in6>()
        }
    };
    (ss, len as libc::socklen_t)
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

pub fn socket(domain: libc::c_int, ty: libc::c_int, proto: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: plain system call; a new descriptor on success.
    let fd = cvt(unsafe { libc::socket(domain, ty | libc::SOCK_CLOEXEC, proto) })?;
    // SAFETY: we own the new descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub fn family(addr: SocketAddr) -> libc::c_int {
    if addr.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    }
}

pub fn sendto(fd: &OwnedFd, buf: &[u8], flags: libc::c_int, to: SocketAddr) -> io::Result<usize> {
    use std::os::fd::AsRawFd;
    let (ss, len) = sockaddr(to);
    // SAFETY: valid buffer and address for the call's duration.
    let n = unsafe {
        libc::sendto(
            fd.as_raw_fd(),
            buf.as_ptr().cast(),
            buf.len(),
            flags,
            std::ptr::from_ref(&ss).cast(),
            len,
        )
    };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

pub fn bind(fd: &OwnedFd, at: SocketAddr) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let (ss, len) = sockaddr(at);
    // SAFETY: valid address for the call's duration.
    cvt(unsafe { libc::bind(fd.as_raw_fd(), std::ptr::from_ref(&ss).cast(), len) }).map(|_| ())
}

pub fn connect(fd: &OwnedFd, to: SocketAddr) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let (ss, len) = sockaddr(to);
    // SAFETY: valid address for the call's duration.
    cvt(unsafe { libc::connect(fd.as_raw_fd(), std::ptr::from_ref(&ss).cast(), len) }).map(|_| ())
}

/// `SO_LINGER` zero: close sends a reset and leaves no `TIME_WAIT`, so a
/// connect loop does not run out of ports.
pub fn reset_on_close(fd: &impl std::os::fd::AsRawFd) {
    let l = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: valid option value of its size.
    unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            std::ptr::from_ref(&l).cast(),
            size_of::<libc::linger>() as libc::socklen_t,
        );
    }
}

/// Clear `IPV6_V6ONLY`, so an IPv6 socket reaches IPv4-mapped addresses
/// (FreeBSD sets it by default; Linux does not).
pub fn dual_stack(fd: &OwnedFd) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let off: libc::c_int = 0;
    // SAFETY: valid option value of its size.
    cvt(unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::IPPROTO_IPV6,
            libc::IPV6_V6ONLY,
            std::ptr::from_ref(&off).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    })
    .map(|_| ())
}

/// A short name for an error: the errno's symbol where known.
pub fn name(e: &io::Error) -> String {
    let Some(n) = e.raw_os_error() else {
        return e.to_string();
    };
    let known = [
        (libc::EPERM, "EPERM"),
        (libc::EACCES, "EACCES"),
        (libc::ECONNREFUSED, "ECONNREFUSED"),
        (libc::ETIMEDOUT, "ETIMEDOUT"),
        (libc::ENETUNREACH, "ENETUNREACH"),
        (libc::EHOSTUNREACH, "EHOSTUNREACH"),
        (libc::EADDRNOTAVAIL, "EADDRNOTAVAIL"),
        (libc::EADDRINUSE, "EADDRINUSE"),
        (libc::EAFNOSUPPORT, "EAFNOSUPPORT"),
        (libc::EPROTONOSUPPORT, "EPROTONOSUPPORT"),
        (libc::EINPROGRESS, "EINPROGRESS"),
        (libc::EAGAIN, "EAGAIN"),
        (libc::EINVAL, "EINVAL"),
        (libc::ENOENT, "ENOENT"),
        (libc::EOPNOTSUPP, "EOPNOTSUPP"),
        (libc::EISCONN, "EISCONN"),
        (libc::ENOBUFS, "ENOBUFS"),
    ];
    known
        .iter()
        .find(|(k, _)| *k == n)
        .map_or_else(|| format!("errno {n}"), |(_, s)| (*s).to_string())
}
