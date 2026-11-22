//! `s40 launch`: as root, the targets the roles reach (outside the fence),
//! the fence, and the fenced child, which runs `s40 enter` under an
//! unprivileged uid. Stands in for the helper and the runner.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::Duration;

use crate::rules::Rules;

pub struct Opts {
    pub fence: String,
    pub rules: String,
    pub app: String,
    pub uid: u32,
    pub landlock_net: bool,
    /// The target address the rules allow; `other` one they do not;
    /// `self_ip` the child's own address (its jail's on FreeBSD's pf).
    pub allowed: IpAddr,
    pub other: IpAddr,
    pub self_ip: IpAddr,
    pub v6: bool,
    pub role: Vec<String>,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let split = args
        .iter()
        .position(|a| a == "--")
        .ok_or("missing -- ROLE")?;
    let (flags, role) = (&args[..split], &args[split + 1..]);
    let get = |name: &str| {
        flags
            .iter()
            .position(|a| a == name)
            .and_then(|i| flags.get(i + 1))
            .cloned()
    };
    let ip = |name: &str, default: &str| -> Result<IpAddr, String> {
        get(name)
            .unwrap_or_else(|| default.into())
            .parse()
            .map_err(|e| format!("{name}: {e}"))
    };
    Ok(Opts {
        fence: get("--fence").unwrap_or_else(|| "none".into()),
        rules: get("--rules").ok_or("--rules FILE")?,
        app: get("--app").unwrap_or_else(|| "probe".into()),
        uid: get("--uid")
            .map_or(Ok(65534), |u| u.parse())
            .map_err(|e| format!("--uid: {e}"))?,
        landlock_net: get("--landlock-net").as_deref() != Some("off"),
        allowed: ip("--allowed", "127.0.0.1")?,
        other: ip("--other", "127.0.0.2")?,
        self_ip: ip("--self", "127.0.0.1")?,
        v6: flags.iter().any(|a| a == "--v6"),
        role: role.to_vec(),
    })
}

/// Accept and drop connections on `at` until the process ends.
fn accepting(at: SocketAddr) -> std::io::Result<()> {
    let l = TcpListener::bind(at)?;
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            drop(c);
        }
    });
    Ok(())
}

/// Read and discard every connection's bytes on `at`.
fn sink(at: SocketAddr) -> std::io::Result<()> {
    let l = TcpListener::bind(at)?;
    std::thread::spawn(move || {
        for mut c in l.incoming().flatten() {
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 1 << 16];
                while matches!(c.read(&mut buf), Ok(n) if n > 0) {}
            });
        }
    });
    Ok(())
}

fn draining(at: SocketAddr) -> std::io::Result<()> {
    let s = UdpSocket::bind(at)?;
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        while s.recv(&mut buf).is_ok() {}
    });
    Ok(())
}

/// The targets: listeners the probe and bench reach, a sink, a UDP drain,
/// datagrams sent to the child's port 9003 from outside its rules, and
/// (Linux) an abstract Unix socket outside its scope.
fn targets(o: &Opts) -> std::io::Result<()> {
    let at = |ip: IpAddr, port: u16| SocketAddr::new(ip, port);
    for port in [7001, 7002, 7100] {
        accepting(at(o.allowed, port))?;
    }
    accepting(at(o.other, 7001))?;
    if o.v6 {
        accepting("[::1]:7001".parse().unwrap())?;
    }
    sink(at(o.allowed, 7004))?;
    draining(at(o.allowed, 7001))?;
    let to = at(o.self_ip, 9003);
    let from = at(o.other, 0);
    std::thread::spawn(move || {
        let Ok(s) = UdpSocket::bind(from) else { return };
        loop {
            let _ = s.send_to(b"from outside the rules", to);
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    #[cfg(target_os = "linux")]
    {
        use std::os::linux::net::SocketAddrExt;
        let name = std::os::unix::net::SocketAddr::from_abstract_name(b"s40-outside")?;
        let l = std::os::unix::net::UnixListener::bind_addr(&name)?;
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                drop(c)
            }
        });
    }
    Ok(())
}

/// The "node" for role `passed`: on fd 3 (a stream pair) each byte asks
/// for a TCP connection to `target`, sent back over `SCM_RIGHTS`; on fd 4
/// (a datagram pair) each datagram is sent on to `target` by UDP.
fn node(stream: OwnedFd, dgram: OwnedFd, target: SocketAddr) {
    std::thread::spawn(move || {
        let mut s = std::os::unix::net::UnixStream::from(stream);
        let mut b = [0u8; 1];
        while matches!(s.read(&mut b), Ok(1)) {
            let Ok(c) = TcpStream::connect(target) else {
                break;
            };
            crate::net::reset_on_close(&c);
            if send_fd(&s, c.as_raw_fd()).is_err() {
                break;
            }
        }
    });
    std::thread::spawn(move || {
        let d = std::os::unix::net::UnixDatagram::from(dgram);
        let Ok(out) = UdpSocket::bind(SocketAddr::new(target.ip(), 0)) else {
            return;
        };
        let mut buf = [0u8; 2048];
        while let Ok(n) = d.recv(&mut buf) {
            let _ = out.send_to(&buf[..n], target);
        }
    });
}

pub fn send_fd(s: &std::os::unix::net::UnixStream, fd: i32) -> std::io::Result<()> {
    let mut byte = *b"f";
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(size_of::<i32>() as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    // SAFETY: all-zero is a valid `msghdr`; its pointers are set below to
    // buffers that outlive the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: the control buffer holds one header and one int.
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(size_of::<i32>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<i32>(), fd);
    }
    // SAFETY: `msg` is valid for the call.
    if unsafe { libc::sendmsg(s.as_raw_fd(), &msg, 0) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn pair(ty: libc::c_int) -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    // SAFETY: valid array of two descriptors.
    let r = unsafe { libc::socketpair(libc::AF_UNIX, ty, 0, fds.as_mut_ptr()) };
    assert_eq!(r, 0, "socketpair: {}", std::io::Error::last_os_error());
    use std::os::fd::FromRawFd;
    // SAFETY: two new descriptors we own.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

pub fn main(args: &[String]) {
    let o = parse(args).unwrap_or_else(|e| {
        eprintln!("s40 launch: {e}");
        std::process::exit(2)
    });
    let rules = Rules::read(&o.rules).unwrap_or_else(|e| panic!("{e}"));
    targets(&o).unwrap_or_else(|e| panic!("targets: {e}"));
    let fence =
        crate::os::Fence::up(&o, &rules).unwrap_or_else(|e| panic!("fence {}: {e}", o.fence));
    let mut passed = Vec::new();
    if o.role.first().map(String::as_str) == Some("passed") {
        let (s_node, s_app) = pair(libc::SOCK_STREAM);
        let (d_node, d_app) = pair(libc::SOCK_DGRAM);
        node(s_node, d_node, SocketAddr::new(o.allowed, 7001));
        passed = vec![s_app, d_app];
    }
    let exe = std::env::current_exe().unwrap();
    let mut cmd = Command::new(exe);
    cmd.arg("enter")
        .arg(&o.rules)
        .arg(if o.landlock_net { "on" } else { "off" })
        .args(&o.role);
    let raw: Vec<i32> = passed.iter().map(AsRawFd::as_raw_fd).collect();
    let mut into = fence.child_hook();
    let (uid, gid) = (o.uid, o.uid);
    // SAFETY: the closure runs in the forked child before exec and makes
    // only async-signal-safe system calls.
    unsafe {
        cmd.pre_exec(move || {
            into()?;
            for (i, fd) in raw.iter().enumerate() {
                if libc::dup2(*fd, 3 + i as i32) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            if libc::setgroups(0, std::ptr::null()) < 0
                || libc::setgid(gid) < 0
                || libc::setuid(uid) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let status = cmd.status();
    drop(passed);
    fence.down();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("s40 launch: child {s}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("s40 launch: spawn: {e}");
            std::process::exit(1);
        }
    }
    let _ = std::io::stdout().flush();
}
