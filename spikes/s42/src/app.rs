//! The app's side: one binary for the writer and the reader, run through
//! `bulkhead-runner --executable` (no arguments reach it), driven by the
//! node's frames on the channel it inherits at descriptors 3 to 6.

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};

use bulkhead_native::channel::{AppEnd, Spin};

use crate::wire::{self, Frame};

/// A blob the app holds: its descriptor and a mapping, if kept.
struct Held {
    fd: OwnedFd,
    map: Option<(*mut libc::c_void, usize, bool)>,
}

impl Held {
    fn unmap(&mut self) {
        if let Some((p, len, _)) = self.map.take() {
            // SAFETY: a mapping this app made and no slice outlives.
            unsafe { libc::munmap(p, len) };
        }
    }
}

struct App {
    end: AppEnd,
    held: HashMap<u64, Held>,
    pattern: Vec<u8>,
    buf: Vec<u8>,
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn name(e: i32) -> String {
    match e {
        0 => "ok".into(),
        libc::EPERM => "EPERM".into(),
        libc::EACCES => "EACCES".into(),
        libc::EBADF => "EBADF".into(),
        libc::EBUSY => "EBUSY".into(),
        libc::EINVAL => "EINVAL".into(),
        #[cfg(target_os = "freebsd")]
        libc::ENOTCAPABLE => "ENOTCAPABLE".into(),
        e => format!("errno {e}"),
    }
}

#[cfg(target_os = "linux")]
const POPULATE_FLAG: libc::c_int = libc::MAP_POPULATE;
#[cfg(target_os = "freebsd")]
const POPULATE_FLAG: libc::c_int = libc::MAP_PREFAULT_READ;

/// Map `len` bytes of `fd` shared; `None` with the errno on failure.
fn map(fd: &OwnedFd, len: usize, write: bool, populate: bool) -> Result<*mut libc::c_void, i32> {
    let prot = libc::PROT_READ | if write { libc::PROT_WRITE } else { 0 };
    let flags = libc::MAP_SHARED | if populate { POPULATE_FLAG } else { 0 };
    // SAFETY: a new shared mapping at an address the kernel picks.
    let p = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, flags, fd.as_raw_fd(), 0) };
    if p == libc::MAP_FAILED {
        Err(errno())
    } else {
        Ok(p)
    }
}

impl App {
    fn read_exact(&self, out: &mut [u8]) {
        let mut off = 0;
        while off < out.len() {
            let n = self.end.read_from_node(&mut out[off..]).expect("ring");
            assert!(n > 0, "the node went away");
            off += n;
        }
    }

    fn write_all(&self, bytes: &[u8]) {
        let mut off = 0;
        while off < bytes.len() {
            off += self.end.write_to_node(&bytes[off..]).expect("ring");
        }
    }

    fn send(&self, kind: u8, a: u64, b: u64, payload: &[u8]) {
        self.write_all(&wire::header(kind, a, b, payload.len()));
        if !payload.is_empty() {
            self.write_all(payload);
        }
    }

    fn text(&self, s: &str) {
        self.send(wire::TEXT, 0, 0, s.as_bytes());
    }

    /// The next frame; a `DATA` frame's payload goes into `self.buf`.
    fn recv(&mut self) -> Frame {
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

    fn pattern(&mut self, n: usize) -> &[u8] {
        if self.pattern.len() != n {
            self.pattern = wire::pattern(n);
        }
        &self.pattern
    }

    /// Make, fill and report a blob of `n` bytes.
    fn make(&mut self, n: usize, how: u64) {
        self.send(wire::REQ, n as u64, 0, &[]);
        let grant = self.recv();
        self.buf = grant.payload;
        assert_eq!(grant.kind, wire::GRANT, "a grant");
        let id = grant.a;
        let fd = self.end.take_fd(id).expect("the blob's descriptor");
        let probe = if how & wire::PROBE != 0 {
            // SAFETY: plain system call on our descriptor.
            let r = unsafe { libc::ftruncate(fd.as_raw_fd(), (n * 2) as libc::off_t) };
            if r == 0 { 0 } else { errno() }
        } else {
            0
        };
        let populate = how & wire::POPULATE != 0;
        let mut held = Held { fd, map: None };
        self.pattern(n);
        if how & wire::MODE_PWRITE != 0 {
            let mut off = 0;
            while off < n {
                let chunk = (n - off).min(1 << 20);
                // SAFETY: `chunk` bytes of the pattern from `off`.
                let w = unsafe {
                    libc::pwrite(
                        held.fd.as_raw_fd(),
                        self.pattern[off..].as_ptr().cast(),
                        chunk,
                        off as libc::off_t,
                    )
                };
                assert!(w > 0, "pwrite: {}", name(errno()));
                off += w as usize;
            }
        } else {
            let p =
                map(&held.fd, n, true, populate).unwrap_or_else(|e| panic!("mmap: {}", name(e)));
            // SAFETY: `n` writable bytes at `p`, the pattern `n` long.
            unsafe { std::ptr::copy_nonoverlapping(self.pattern.as_ptr(), p.cast::<u8>(), n) };
            held.map = Some((p, n, true));
            if how & wire::KEEP == 0 {
                held.unmap();
            }
        }
        if how & wire::KEEP != 0 {
            self.held.insert(id, held);
        }
        self.send(wire::FILLED, id, probe as u64, &[]);
    }

    /// Read blob `id`, answering its sum.
    fn read(&mut self, id: u64, how: u64) {
        let fd = self.end.take_fd(id).expect("the blob's descriptor");
        let seals = rustix::fs::fcntl_get_seals(&fd).expect("seals");
        assert!(
            seals.contains(rustix::fs::SealFlags::WRITE),
            "unsealed blob"
        );
        let len = rustix::fs::fstat(&fd).expect("fstat").st_size as usize;
        if how & wire::COPY != 0 {
            let mut buf = std::mem::take(&mut self.buf);
            buf.resize(len, 0);
            let mut off = 0;
            while off < len {
                let n = rustix::io::pread(&fd, &mut buf[off..], off as u64).expect("pread");
                assert!(n > 0, "pread: end of blob");
                off += n;
            }
            let s = wire::sum(&buf);
            self.buf = buf;
            drop(fd);
            return self.send(wire::OK, s, 0, &[]);
        }
        let p = map(&fd, len, false, how & wire::POPULATE != 0)
            .unwrap_or_else(|e| panic!("mmap: {}", name(e)));
        // SAFETY: `len` readable bytes at `p`, sealed against writes.
        let s = wire::sum(unsafe { std::slice::from_raw_parts(p.cast::<u8>(), len) });
        let mut held = Held {
            fd,
            map: Some((p, len, false)),
        };
        if how & wire::KEEP != 0 {
            self.held.insert(id, held);
        } else {
            held.unmap();
        }
        self.send(wire::OK, s, 0, &[]);
    }

    /// Every way to change blob `id`, and what each got.
    fn try_all(&mut self, id: u64) {
        let Some(h) = self.held.get(&id) else {
            return self.text("no such blob");
        };
        let fd = h.fd.as_raw_fd();
        let len = rustix::fs::fstat(&h.fd).map_or(0, |s| s.st_size as usize);
        let byte = [0xAAu8];
        // SAFETY: plain system calls on our own descriptor and buffers.
        let pw = unsafe { libc::pwrite(fd, byte.as_ptr().cast(), 1, 0) };
        let pw = if pw == 1 { 0 } else { errno() };
        let tr = unsafe { libc::ftruncate(fd, (len * 2) as libc::off_t) };
        let tr = if tr == 0 { 0 } else { errno() };
        let mw = match map(&h.fd, len, true, false) {
            Ok(p) => {
                unsafe { libc::munmap(p, len) };
                0
            }
            Err(e) => e,
        };
        let mp = match h.map {
            Some((p, l, false)) => {
                let r = unsafe { libc::mprotect(p, l, libc::PROT_READ | libc::PROT_WRITE) };
                if r == 0 { 0 } else { errno() }
            }
            _ => -1,
        };
        let mp = if mp == -1 { "-".to_string() } else { name(mp) };
        self.text(&format!(
            "pwrite={} ftruncate={} mmap_write={} mprotect_write={}",
            name(pw),
            name(tr),
            name(mw),
            mp
        ));
    }

    fn serve(&mut self) {
        loop {
            let f = self.recv();
            match f.kind {
                wire::RING_SEND => {
                    let n = f.a as usize;
                    self.buf = f.payload;
                    self.pattern(n);
                    let p = std::mem::take(&mut self.pattern);
                    self.send(wire::DATA, 0, 0, &p);
                    self.pattern = p;
                }
                wire::DATA => {
                    let s = wire::sum(&f.payload);
                    self.buf = f.payload;
                    self.send(wire::OK, s, 0, &[]);
                }
                wire::BLOB_MAKE => {
                    self.buf = f.payload;
                    self.make(f.a as usize, f.b);
                }
                wire::BLOB_READ => {
                    self.buf = f.payload;
                    self.read(f.a, f.b);
                }
                wire::TRY => self.try_all(f.a),
                wire::POKE => {
                    let r = match self.held.get(&f.a).and_then(|h| h.map) {
                        Some((p, _, true)) => {
                            // SAFETY: the first byte of our writable mapping.
                            unsafe { *p.cast::<u8>() = 0xAA };
                            0
                        }
                        _ => 1,
                    };
                    self.send(wire::OK, r, 0, &[]);
                }
                wire::RESUM => match self.held.get(&f.a).and_then(|h| h.map) {
                    Some((p, len, _)) => {
                        // SAFETY: our mapping of `len` bytes.
                        let bytes = unsafe { std::slice::from_raw_parts(p.cast::<u8>(), len) };
                        self.send(wire::OK, wire::sum(bytes), u64::from(bytes[0]), &[]);
                    }
                    None => self.send(wire::OK, 0, u64::MAX, &[]),
                },
                wire::DROP => {
                    if let Some(mut h) = self.held.remove(&f.a) {
                        h.unmap();
                    }
                    self.send(wire::OK, 0, 0, &[]);
                }
                wire::WHO => {
                    let s = format!(
                        "pid={} uid={}",
                        std::process::id(),
                        rustix::process::getuid().as_raw()
                    );
                    self.text(&s);
                }
                wire::EXIT => return,
                k => panic!("frame of kind {k}"),
            }
        }
    }
}

/// Serve the node on the inherited channel until told to end.
pub fn serve() {
    let end = AppEnd::inherited().expect("the channel at descriptors 3 to 6");
    let _ = Spin::from_env();
    let _ = end.door().as_fd();
    let mut app = App {
        end,
        held: HashMap::new(),
        pattern: Vec::new(),
        buf: Vec::new(),
    };
    app.serve();
}
