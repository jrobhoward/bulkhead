//! The node's side: two apps (a writer and a reader) on channels of
//! `bulkhead-native`, started through `bulkhead-runner` when one is given,
//! blobs made, sealed and passed as S42's plan says, and the same bytes
//! sent through the rings for comparison.

use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{Ordering, fence};
use std::time::{Duration, Instant};

use bulkhead_native::channel::{
    Channel, bell, descriptors, door, install, send_fd, take_waiting, wake_reader, wake_writer,
};
use rustix::fs::SealFlags;

use crate::wire::{self, Frame};

/// How a child is started.
#[derive(Clone, Default)]
pub struct Start {
    /// `bulkhead-runner`, or `None` to run the app directly.
    pub runner: Option<PathBuf>,
    /// The app's user and group (as root), as the helper gives each app one.
    pub user: Option<u32>,
    /// A cgroup directory the child joins before it executes (as root).
    pub cgroup: Option<PathBuf>,
}

pub struct App {
    channel: Channel,
    door: OwnedFd,
    bell: OwnedFd,
    fds: OwnedFd,
    pub child: Child,
    buf: Vec<u8>,
}

/// Start the spike's binary as an app.
pub fn spawn(start: &Start) -> App {
    let (channel, memfd) = Channel::create().unwrap();
    let (door_r, door_w) = door().unwrap();
    let (bell_r, bell_w) = bell().unwrap();
    let (fds_node, fds_app) = descriptors().unwrap();
    let me = std::env::current_exe().unwrap();
    let mut cmd = match &start.runner {
        Some(r) => {
            let mut c = Command::new(r);
            c.arg("--executable").arg(&me);
            c
        }
        None => {
            let mut c = Command::new(&me);
            c.arg("app");
            c
        }
    };
    let procs = start
        .cgroup
        .as_ref()
        .map(|c| CString::new(c.join("cgroup.procs").to_str().unwrap()).unwrap());
    let user = start.user;
    let (m, b, d, f) = (
        memfd.as_raw_fd(),
        bell_w.as_raw_fd(),
        door_r.as_raw_fd(),
        fds_app.as_raw_fd(),
    );
    // SAFETY: between fork and exec: system calls on descriptors and
    // strings made before the fork, no allocation.
    unsafe {
        cmd.pre_exec(move || {
            use std::os::fd::BorrowedFd;
            install(
                BorrowedFd::borrow_raw(m),
                BorrowedFd::borrow_raw(b),
                BorrowedFd::borrow_raw(d),
                BorrowedFd::borrow_raw(f),
            )?;
            if let Some(p) = &procs {
                let fd = libc::open(p.as_ptr(), libc::O_WRONLY);
                if fd < 0 || libc::write(fd, b"0".as_ptr().cast(), 1) != 1 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(fd);
            }
            if let Some(u) = user {
                if libc::setgroups(0, std::ptr::null()) != 0
                    || libc::setgid(u as libc::gid_t) != 0
                    || libc::setuid(u as libc::uid_t) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let child = cmd.spawn().expect("start the app");
    drop((memfd, bell_w, door_r, fds_app));
    App {
        channel,
        door: door_w,
        bell: bell_r,
        fds: fds_node,
        child,
        buf: Vec::new(),
    }
}

impl App {
    /// Wait for the app to ring the bell; `false` once it has gone.
    fn wait_bell(&self) {
        let mut b = [0u8; 64];
        match rustix::io::read(&self.bell, &mut b) {
            Ok(0) => panic!("the app went away"),
            _ => {}
        }
    }

    fn write_all(&self, bytes: &[u8]) {
        let ring = self.channel.to_app();
        let mut off = 0;
        while off < bytes.len() {
            let n = ring.write(&bytes[off..]).expect("ring");
            if n > 0 {
                off += n;
                wake_reader(ring.reader_waiting(), self.door.as_fd());
                continue;
            }
            ring.writer_waiting().store(1, Ordering::SeqCst);
            fence(Ordering::SeqCst);
            if !ring.has_room() {
                self.wait_bell();
            }
            let _ = take_waiting(ring.writer_waiting());
        }
    }

    pub fn send(&self, kind: u8, a: u64, b: u64, payload: &[u8]) {
        self.write_all(&wire::header(kind, a, b, payload.len()));
        if !payload.is_empty() {
            self.write_all(payload);
        }
    }

    fn read_exact(&self, out: &mut [u8]) {
        let ring = self.channel.to_node();
        let mut off = 0;
        let mut spun = Instant::now();
        while off < out.len() {
            let n = ring.read(&mut out[off..]).expect("ring");
            if n > 0 {
                off += n;
                // Clears the word and wakes the writer if it slept.
                wake_writer(ring.writer_waiting());
                spun = Instant::now();
                continue;
            }
            // As the node's poller does: spin 20 µs, then sleep on the bell.
            if spun.elapsed() < Duration::from_micros(20) {
                std::thread::yield_now();
                continue;
            }
            ring.reader_waiting().store(1, Ordering::SeqCst);
            fence(Ordering::SeqCst);
            if !ring.is_readable() {
                self.wait_bell();
            }
            let _ = take_waiting(ring.reader_waiting());
            spun = Instant::now();
        }
    }

    /// The next frame from the app.
    pub fn recv(&mut self) -> Frame {
        let mut len = [0u8; 4];
        self.read_exact(&mut len);
        let len = u32::from_le_bytes(len) as usize;
        let mut head = [0u8; 17];
        self.read_exact(&mut head);
        let mut payload = std::mem::take(&mut self.buf);
        payload.resize(len - 17, 0);
        self.read_exact(&mut payload);
        wire::parse(&head, payload)
    }

    /// Give a frame's payload buffer back for the next.
    pub fn give_back(&mut self, f: Frame) {
        self.buf = f.payload;
    }

    pub fn text(&mut self) -> String {
        let f = self.recv();
        assert_eq!(f.kind, wire::TEXT);
        String::from_utf8_lossy(&f.payload).into_owned()
    }

    pub fn ok(&mut self) -> (u64, u64) {
        let f = self.recv();
        assert_eq!(f.kind, wire::OK, "OK, not kind {}", f.kind);
        (f.a, f.b)
    }

    pub fn pass(&self, id: u64, fd: &OwnedFd) {
        send_fd(self.fds.as_fd(), id, fd.as_fd()).expect("pass a descriptor");
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn exit(mut self) {
        self.send(wire::EXIT, 0, 0, &[]);
        let _ = self.child.wait();
    }
}

/// Whether the node allocates a blob's pages before granting it, as a
/// pool of ready blobs would (`S42_READY`), and the time that took.
pub static READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub static READY_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A new blob of `n` bytes: sized, sealed against growing and shrinking.
pub fn make_blob(n: usize) -> OwnedFd {
    let fd = rustix::fs::memfd_create(
        "s42-blob",
        rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING,
    )
    .expect("memfd");
    rustix::fs::ftruncate(&fd, n as u64).expect("size");
    rustix::fs::fcntl_add_seals(&fd, SealFlags::GROW | SealFlags::SHRINK).expect("seal size");
    if READY.load(Ordering::Relaxed) {
        let t = Instant::now();
        rustix::fs::fallocate(&fd, rustix::fs::FallocateFlags::empty(), 0, n as u64)
            .expect("fallocate");
        READY_NS.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
    fd
}

/// A copy of `fd` for an app: on FreeBSD limited to `writer`'s or a
/// reader's rights (the node's `for_app`); on Linux the descriptor itself.
pub fn for_app(fd: &OwnedFd, writer: bool) -> OwnedFd {
    let copy = rustix::io::fcntl_dupfd_cloexec(fd, 0).expect("dup");
    #[cfg(target_os = "freebsd")]
    {
        use bulkhead_native::freebsd::{limit_rights, rights};
        let mut list = if writer {
            rights::MEMFD.to_vec()
        } else {
            rights::SNAPSHOT.to_vec()
        };
        if writer {
            // `pwrite` needs `CAP_PWRITE` (write and seek), the rings'
            // memfds do not.
            list.push(libc::CAP_SEEK);
        }
        limit_rights(copy.as_raw_fd(), &list).expect("limit rights");
    }
    #[cfg(not(target_os = "freebsd"))]
    let _ = writer;
    copy
}

/// The writer's side of a blob: ask, grant, fill. Returns the node's
/// descriptor and the probe's errno.
pub fn produce(w: &mut App, id: u64, n: usize, how: u64) -> (OwnedFd, u64) {
    w.send(wire::BLOB_MAKE, n as u64, how, &[]);
    let req = w.recv();
    assert_eq!(req.kind, wire::REQ);
    assert_eq!(req.a as usize, n);
    w.give_back(req);
    let fd = make_blob(n);
    w.pass(id, &for_app(&fd, true));
    w.send(wire::GRANT, id, 0, &[]);
    let filled = w.recv();
    assert_eq!(filled.kind, wire::FILLED);
    let probe = filled.b;
    w.give_back(filled);
    (fd, probe)
}

/// Seal `fd` against writes; the errno if refused.
pub fn seal(fd: &OwnedFd) -> Result<(), rustix::io::Errno> {
    rustix::fs::fcntl_add_seals(fd, SealFlags::WRITE | SealFlags::SEAL)
}

/// CPU nanoseconds of `pid` (`/proc/<pid>/schedstat`, Linux), or 0.
pub fn cpu_ns(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/schedstat"))
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0)
}
