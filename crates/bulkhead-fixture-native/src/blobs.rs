//! The fixture's blob commands (`bulkhead:blobs`, ROADMAP BL1). A blob's
//! descriptors and mappings are kept by id until `blob-drop`:
//!
//! | Command | Reply |
//! | --- | --- |
//! | `blob-create N` | the new blob's id, its writable descriptor kept |
//! | `blob-fill ID TEXT` | `ok` once `TEXT`, repeated, fills it (`pwrite`) |
//! | `blob-map ID`, `blob-unmap ID` | `ok`: a writable mapping kept, or removed |
//! | `blob-seal ID [TTL_MS]` | `ok` |
//! | `blob-share ID APP` | `ok` |
//! | `blob-open ID` | `SIZE TEXT`: its size and first 16 bytes, through a read-only mapping; the descriptor kept |
//! | `blob-reread ID` | its first 16 bytes again, through the kept descriptor (`pread`) |
//! | `blob-release ID` | `ok` |
//! | `blob-try ID` | `pwrite=… ftruncate=… mmap_write=…`: each change tried on the kept descriptor (`ok` or the OS error) |
//! | `blob-drop ID` | `ok`, the descriptor and mappings closed |
//! | `sender-blob N TEXT` | the id of a blob made, filled and sealed from a thread of its own on a sender channel |
//!
//! A refused request is `err:` and the node's error.

use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, OwnedFd};

use bulkhead_native::app::{self, Ctx};
use bulkhead_native::protocol::{BlobAnswer, BlobError, BlobId, BlobRequest};

/// A blob's kept descriptor and writable mapping.
struct Kept {
    fd: OwnedFd,
    map: Option<(usize, usize)>,
}

impl Drop for Kept {
    fn drop(&mut self) {
        if let Some((p, len)) = self.map.take() {
            // SAFETY: a mapping this fixture made; nothing borrows it.
            unsafe { libc::munmap(p as *mut libc::c_void, len) };
        }
    }
}

/// The blobs the fixture holds, by id.
#[derive(Default)]
pub(crate) struct Blobs {
    kept: BTreeMap<u64, Kept>,
}

fn errno() -> String {
    std::io::Error::last_os_error().to_string()
}

fn size_of(fd: &OwnedFd) -> usize {
    // SAFETY: a zeroed `stat` the call fills.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: our descriptor and a valid buffer.
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } == 0 {
        usize::try_from(st.st_size).unwrap_or(0)
    } else {
        0
    }
}

/// Fill `fd` with `text`, repeated.
fn fill(fd: &OwnedFd, text: &str) -> Result<(), String> {
    let size = size_of(fd);
    let bytes: Vec<u8> = text.bytes().cycle().take(size).collect();
    let mut off = 0;
    while off < size {
        // SAFETY: `size - off` bytes of `bytes` from `off`.
        let n = unsafe {
            libc::pwrite(
                fd.as_raw_fd(),
                bytes[off..].as_ptr().cast(),
                size - off,
                off as libc::off_t,
            )
        };
        if n <= 0 {
            return Err(errno());
        }
        off += n as usize;
    }
    Ok(())
}

/// The first 16 bytes of `fd`, read with `pread`.
fn head(fd: &OwnedFd) -> String {
    let mut buf = [0u8; 16];
    // SAFETY: our descriptor and a 16-byte buffer.
    let n = unsafe { libc::pread(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
    if n < 0 {
        return format!("err:{}", errno());
    }
    String::from_utf8_lossy(&buf[..n as usize]).into_owned()
}

fn id(s: &str) -> u64 {
    s.parse().unwrap_or(0)
}

/// Make, fill and seal a blob with `blob`, which asks the node.
fn make(
    mut blob: impl FnMut(BlobRequest) -> Result<BlobAnswer, BlobError>,
    size: u64,
    text: &str,
) -> String {
    let id = match blob(BlobRequest::Create { size }) {
        Ok(BlobAnswer::Created { id, fd }) => match app::take_fd(fd) {
            Ok(fd) => match fill(&fd, text) {
                Ok(()) => id,
                Err(e) => return format!("err:{e}"),
            },
            Err(e) => return format!("err:{e}"),
        },
        Ok(other) => return format!("err:unexpected {other:?}"),
        Err(e) => return format!("err:{e:?}"),
    };
    match blob(BlobRequest::Seal { id, ttl_ms: 0 }) {
        Ok(_) => id.0.to_string(),
        Err(e) => format!("err:{e:?}"),
    }
}

impl Blobs {
    pub(crate) fn command(&mut self, ctx: &mut Ctx, verb: &str, rest: &str) -> String {
        let (first, tail) = rest.split_once(' ').unwrap_or((rest, ""));
        let unit = |r: Result<BlobAnswer, BlobError>| match r {
            Ok(_) => "ok".to_string(),
            Err(e) => format!("err:{e:?}"),
        };
        match verb {
            "blob-create" => match ctx.blob(BlobRequest::Create {
                size: first.parse().unwrap_or(0),
            }) {
                Ok(BlobAnswer::Created { id, fd }) => match app::take_fd(fd) {
                    Ok(fd) => {
                        self.kept.insert(id.0, Kept { fd, map: None });
                        id.0.to_string()
                    }
                    Err(e) => format!("err:{e}"),
                },
                Ok(other) => format!("err:unexpected {other:?}"),
                Err(e) => format!("err:{e:?}"),
            },
            "blob-fill" => match self.kept.get(&id(first)) {
                Some(k) => fill(&k.fd, tail).map_or_else(|e| format!("err:{e}"), |()| "ok".into()),
                None => "err:not kept".into(),
            },
            "blob-map" => match self.kept.get_mut(&id(first)) {
                Some(k) => {
                    let len = size_of(&k.fd);
                    // SAFETY: a new shared writable mapping at an address
                    // the kernel picks.
                    let p = unsafe {
                        libc::mmap(
                            std::ptr::null_mut(),
                            len,
                            libc::PROT_READ | libc::PROT_WRITE,
                            libc::MAP_SHARED,
                            k.fd.as_raw_fd(),
                            0,
                        )
                    };
                    if p == libc::MAP_FAILED {
                        format!("err:{}", errno())
                    } else {
                        k.map = Some((p as usize, len));
                        "ok".into()
                    }
                }
                None => "err:not kept".into(),
            },
            "blob-unmap" => match self.kept.get_mut(&id(first)) {
                Some(k) => {
                    if let Some((p, len)) = k.map.take() {
                        // SAFETY: the mapping `blob-map` made.
                        unsafe { libc::munmap(p as *mut libc::c_void, len) };
                    }
                    "ok".into()
                }
                None => "err:not kept".into(),
            },
            "blob-seal" => unit(ctx.blob(BlobRequest::Seal {
                id: BlobId(id(first)),
                ttl_ms: tail.parse().unwrap_or(0),
            })),
            "blob-share" => unit(ctx.blob(BlobRequest::Share {
                id: BlobId(id(first)),
                app: tail.to_string(),
            })),
            "blob-open" => match ctx.blob(BlobRequest::Open {
                id: BlobId(id(first)),
            }) {
                Ok(BlobAnswer::Opened { size, fd }) => match app::take_fd(fd) {
                    Ok(fd) => match bulkhead_native::channel::Mapped::open(&fd) {
                        Ok(m) => {
                            let text =
                                String::from_utf8_lossy(&m.bytes()[..m.bytes().len().min(16)])
                                    .into_owned();
                            self.kept.insert(id(first), Kept { fd, map: None });
                            format!("{size} {text}")
                        }
                        Err(e) => format!("err:{e}"),
                    },
                    Err(e) => format!("err:{e}"),
                },
                Ok(other) => format!("err:unexpected {other:?}"),
                Err(e) => format!("err:{e:?}"),
            },
            "blob-reread" => match self.kept.get(&id(first)) {
                Some(k) => head(&k.fd),
                None => "err:not kept".into(),
            },
            "blob-release" => unit(ctx.blob(BlobRequest::Release {
                id: BlobId(id(first)),
            })),
            "blob-try" => match self.kept.get(&id(first)) {
                Some(k) => try_all(&k.fd),
                None => "err:not kept".into(),
            },
            "blob-drop" => {
                self.kept.remove(&id(first));
                "ok".into()
            }
            "sender-blob" => match ctx.sender() {
                Ok(mut sender) => {
                    let size = first.parse().unwrap_or(0);
                    let text = tail.to_string();
                    std::thread::spawn(move || make(|r| sender.blob(r), size, &text))
                        .join()
                        .unwrap_or_else(|_| "err:thread".into())
                }
                Err(e) => format!("err:{e:?}"),
            },
            other => format!("err:unknown {other}"),
        }
    }
}

/// Every way to change the blob behind `fd`, and what each got.
fn try_all(fd: &OwnedFd) -> String {
    let len = size_of(fd);
    let byte = *b"X";
    let outcome = |ok: bool| if ok { "ok".to_string() } else { errno() };
    // SAFETY: our descriptor and a one-byte buffer.
    let pw = outcome(unsafe { libc::pwrite(fd.as_raw_fd(), byte.as_ptr().cast(), 1, 0) } == 1);
    // SAFETY: our descriptor; no pointers.
    let tr = outcome(unsafe { libc::ftruncate(fd.as_raw_fd(), (len * 2) as libc::off_t) } == 0);
    // SAFETY: a new shared writable mapping, removed at once if made.
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len.max(1),
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    let mw = if p == libc::MAP_FAILED {
        errno()
    } else {
        // SAFETY: the mapping just made.
        unsafe { libc::munmap(p, len.max(1)) };
        "ok".into()
    };
    format!("pwrite={pw} ftruncate={tr} mmap_write={mw}")
}
