//! `bulkhead:blobs`: data passed between apps as sealed memory (ROADMAP
//! BL1; the mechanism from spike S42, `spikes/s42/README.md`).
//!
//! A writer asks for a blob of N bytes: the node makes a memfd (on
//! FreeBSD an anonymous shared memory object), sizes it, seals it against
//! growing and shrinking, keeps a descriptor and passes the writer one.
//! The writer fills it and asks the node to seal it against writes, which
//! both systems refuse while a writable mapping remains (`mapped`), so a
//! blob the node hands out can never change. The writer's app then shares
//! it with apps its messaging ACL lets it call, and sends the id in a
//! message of its own; an instance of a shared app opens it and gets a
//! read-only descriptor.
//!
//! The node frees a blob (drops its descriptor, and the bytes leave the
//! writing app's `max_bytes`) once the writing app and every app it was
//! shared with released it, or when its time to live runs out; an unsealed
//! blob ends with the instance that made it. A descriptor an app holds
//! cannot be taken back: it stays readable until the app closes it, so
//! expiry ends new opens, not reads. Linux charges a blob's memory to the
//! cgroup of the instance that wrote it; FreeBSD counts a mapping in each
//! holder's memory.

#[cfg(test)]
#[path = "blobs_tests.rs"]
mod tests;

use std::collections::{BTreeSet, HashMap};
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use bulkhead_bundle::manifest::{AppSpec, Capability};
use bulkhead_native::protocol::{BlobAnswer, BlobError, BlobId, BlobRequest, FdRef};

use super::{InstanceInfo, Service};
use crate::config::{Blobs as BlobsConfig, RuntimeConfig};
use crate::error::HostError;
use crate::metrics::AppStats;
use crate::pid::Pid;

/// The blobs service.
pub(crate) struct Blobs;

/// What the manifest asks: how many bytes the app's live blobs may hold,
/// if it declares `bulkhead:blobs`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlobsSpec {
    /// The scope's `max_bytes`; `None` without the capability.
    pub max_bytes: Option<u64>,
}

/// One blob the node holds.
struct Blob {
    size: u64,
    /// The app that wrote it, whose `max_bytes` it counts against.
    app: String,
    /// The instance that made it; it alone fills and seals it.
    maker: Pid,
    sealed: bool,
    /// When it is freed, whoever still holds it; set at seal.
    expires: Option<Instant>,
    /// Apps it was shared with.
    shared: BTreeSet<String>,
    /// Apps that are done with it, the writing app among them.
    released: BTreeSet<String>,
    /// The node's descriptor: the blob lives in the node while it is held.
    fd: OwnedFd,
    stats: Arc<AppStats>,
}

impl Blob {
    /// Whether every app that may open it released it.
    fn done(&self) -> bool {
        self.released.contains(&self.app) && self.shared.is_subset(&self.released)
    }
}

/// Every blob on the node, and what each app's live blobs hold.
#[derive(Default)]
struct Table {
    next: u64,
    blobs: HashMap<u64, Blob>,
    bytes: HashMap<String, u64>,
}

impl Table {
    /// Free blob `id`: drop the node's descriptor and its bytes from its
    /// app's count, counted as `outcome`.
    fn free(&mut self, id: u64, outcome: usize) {
        if let Some(blob) = self.blobs.remove(&id) {
            if let Some(n) = self.bytes.get_mut(&blob.app) {
                *n = n.saturating_sub(blob.size);
                if *n == 0 {
                    self.bytes.remove(&blob.app);
                }
            }
            blob.stats.blob_bytes(-i128::from(blob.size));
            blob.stats.blob(outcome);
        }
    }

    /// Free every blob past its time.
    fn sweep(&mut self, now: Instant) {
        let expired: Vec<u64> = self
            .blobs
            .iter()
            .filter(|(_, b)| b.expires.is_some_and(|e| e <= now))
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.free(id, EXPIRED);
        }
    }
}

/// Indexes into [`crate::metrics::BLOB_OUTCOMES`].
const CREATED: usize = 0;
const SEALED: usize = 1;
const REFUSED: usize = 2;
const FREED: usize = 3;
const EXPIRED: usize = 4;

/// The node-wide state: the configuration and every blob.
pub(crate) struct Shared {
    config: BlobsConfig,
    table: Mutex<Table>,
}

impl Shared {
    fn new(config: &RuntimeConfig) -> Self {
        Self {
            config: config.blobs.clone(),
            table: Mutex::default(),
        }
    }

    fn table(&self) -> std::sync::MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Free expired blobs every second while `shared` lives, if a tokio
    /// runtime runs (requests also sweep, so none outlives its time by
    /// more than the next request without one).
    pub(crate) fn sweep_every_second(shared: &Arc<Self>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let weak: Weak<Self> = Arc::downgrade(shared);
        handle.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                let Some(shared) = weak.upgrade() else {
                    return;
                };
                shared.table().sweep(Instant::now());
            }
        });
    }

    /// The live blobs of `app` and their bytes, for `bulkhead status`.
    pub(crate) fn usage(&self, app: &str) -> (usize, u64) {
        let table = self.table();
        let count = table.blobs.values().filter(|b| b.app == app).count();
        (count, table.bytes.get(app).copied().unwrap_or(0))
    }
}

/// A descriptor a request gives the app, passed before its answer.
#[derive(Debug)]
pub(crate) enum Give {
    /// A blob to fill: read, write, map read-write.
    Write(OwnedFd),
    /// A sealed blob to read: read, map read-only.
    Read(OwnedFd),
}

/// What a request answers: the answer (its descriptor field to be set
/// once passed) and the descriptor to pass, if any.
pub(crate) type Reply = (BlobAnswer, Option<Give>);

/// One instance's view of the blobs.
pub(crate) struct Instance {
    granted: bool,
    app: String,
    pid: Pid,
    max_bytes: u64,
    shared: Arc<Shared>,
    stats: Arc<AppStats>,
}

impl Service for Blobs {
    const CAPABILITY: Capability = Capability::Blobs;
    type Spec = BlobsSpec;
    type Shared = Shared;
    type Instance = Instance;

    fn spec(manifest: &AppSpec) -> BlobsSpec {
        BlobsSpec {
            max_bytes: manifest
                .capabilities
                .blobs
                .as_ref()
                .map(|s| s.max_bytes.bytes()),
        }
    }

    fn shared(config: &RuntimeConfig) -> Result<Shared, HostError> {
        Ok(Shared::new(config))
    }

    fn instance(shared: &Arc<Shared>, spec: &BlobsSpec, info: &InstanceInfo<'_>) -> Instance {
        let app = info.handle.app().to_string();
        Instance {
            granted: info.granted && spec.max_bytes.is_some(),
            max_bytes: spec.max_bytes.unwrap_or(0),
            pid: info.handle.pid(),
            stats: info.metrics.stats(&app),
            app,
            shared: shared.clone(),
        }
    }

    fn exited(shared: &Shared, pid: Pid) {
        let mut table = shared.table();
        let unsealed: Vec<u64> = table
            .blobs
            .iter()
            .filter(|(_, b)| !b.sealed && b.maker == pid)
            .map(|(id, _)| *id)
            .collect();
        for id in unsealed {
            table.free(id, FREED);
        }
    }
}

impl Instance {
    /// Answer `request`. `may_share(app)` says whether this app's
    /// messaging ACL lets it call `app` and `app` accepts it.
    ///
    /// # Errors
    ///
    /// As [`BlobError`] describes.
    pub(crate) fn native(
        &self,
        request: BlobRequest,
        may_share: impl FnOnce(&str) -> bool,
    ) -> Result<Reply, BlobError> {
        if !self.granted {
            self.stats.blob(REFUSED);
            return Err(BlobError::Denied);
        }
        let mut table = self.shared.table();
        table.sweep(Instant::now());
        let answer = match request {
            BlobRequest::Create { size } => self.create(&mut table, size),
            BlobRequest::Seal { id, ttl_ms } => self.seal(&mut table, id, ttl_ms),
            BlobRequest::Share { id, app } => self.share(&mut table, id, &app, may_share),
            BlobRequest::Open { id } => self.open(&table, id),
            BlobRequest::Release { id } => self.release(&mut table, id),
        };
        if answer.is_err() {
            self.stats.blob(REFUSED);
        }
        answer
    }

    fn create(&self, table: &mut Table, size: u64) -> Result<Reply, BlobError> {
        if size == 0 || size > self.shared.config.max_blob {
            return Err(BlobError::TooLarge);
        }
        let held = table.bytes.get(&self.app).copied().unwrap_or(0);
        if held.saturating_add(size) > self.max_bytes {
            return Err(BlobError::OverQuota);
        }
        let fd = memory::make(size).map_err(|e| BlobError::Other(format!("a blob: {e}")))?;
        let copy = memory::copy(&fd).map_err(|e| BlobError::Other(format!("a blob: {e}")))?;
        table.next += 1;
        let id = table.next;
        table.blobs.insert(
            id,
            Blob {
                size,
                app: self.app.clone(),
                maker: self.pid,
                sealed: false,
                expires: None,
                shared: BTreeSet::new(),
                released: BTreeSet::new(),
                fd,
                stats: self.stats.clone(),
            },
        );
        *table.bytes.entry(self.app.clone()).or_insert(0) += size;
        self.stats.blob_bytes(i128::from(size));
        self.stats.blob(CREATED);
        Ok((
            BlobAnswer::Created {
                id: BlobId(id),
                fd: FdRef(0),
            },
            Some(Give::Write(copy)),
        ))
    }

    fn seal(&self, table: &mut Table, id: BlobId, ttl_ms: u64) -> Result<Reply, BlobError> {
        let config = &self.shared.config;
        let blob = table.blobs.get_mut(&id.0).ok_or(BlobError::NotFound)?;
        if blob.maker != self.pid {
            return Err(BlobError::Denied);
        }
        if !blob.sealed {
            memory::seal(&blob.fd)?;
            blob.sealed = true;
            let ttl = if ttl_ms == 0 {
                config.default_ttl
            } else {
                Duration::from_millis(ttl_ms).min(config.max_ttl)
            };
            blob.expires = Some(Instant::now() + ttl);
            self.stats.blob(SEALED);
        }
        Ok((BlobAnswer::Unit, None))
    }

    fn share(
        &self,
        table: &mut Table,
        id: BlobId,
        app: &str,
        may_share: impl FnOnce(&str) -> bool,
    ) -> Result<Reply, BlobError> {
        let blob = table.blobs.get_mut(&id.0).ok_or(BlobError::NotFound)?;
        if blob.app != self.app {
            return Err(BlobError::Denied);
        }
        if !blob.sealed {
            return Err(BlobError::NotSealed);
        }
        if app != self.app && !may_share(app) {
            return Err(BlobError::Denied);
        }
        if app != self.app {
            blob.shared.insert(app.to_string());
            blob.released.remove(app);
        }
        Ok((BlobAnswer::Unit, None))
    }

    fn open(&self, table: &Table, id: BlobId) -> Result<Reply, BlobError> {
        let blob = table.blobs.get(&id.0).ok_or(BlobError::NotFound)?;
        if blob.app != self.app && !blob.shared.contains(&self.app) {
            return Err(BlobError::Denied);
        }
        if !blob.sealed {
            return Err(BlobError::NotSealed);
        }
        let copy = memory::copy(&blob.fd).map_err(|e| BlobError::Other(format!("a blob: {e}")))?;
        Ok((
            BlobAnswer::Opened {
                size: blob.size,
                fd: FdRef(0),
            },
            Some(Give::Read(copy)),
        ))
    }

    fn release(&self, table: &mut Table, id: BlobId) -> Result<Reply, BlobError> {
        let blob = table.blobs.get_mut(&id.0).ok_or(BlobError::NotFound)?;
        if blob.app != self.app && !blob.shared.contains(&self.app) {
            return Err(BlobError::Denied);
        }
        blob.released.insert(self.app.clone());
        // An unsealed blob its writer releases is given up.
        if blob.done() || !blob.sealed {
            table.free(id.0, FREED);
        }
        Ok((BlobAnswer::Unit, None))
    }
}

/// Whether the app of `me` may share a blob with `app`: its messaging ACL
/// lets it call `app`, and `app`, running here, accepts it.
pub(crate) fn may_share(
    rt: &crate::runtime::RuntimeInner,
    me: &crate::instance::InstanceHandle,
    app: &str,
) -> bool {
    rt.app_acl(app).is_some_and(|target| {
        crate::acl::allowed(me.app(), &me.acl().may_call, app, &target.accept_from)
    })
}

/// Pass the descriptor a reply gives, with `send`, and name it in the
/// answer.
#[cfg(unix)]
pub(crate) fn pass(
    reply: Result<Reply, BlobError>,
    send: impl FnOnce(std::os::fd::BorrowedFd<'_>, crate::native::Passed) -> Result<FdRef, String>,
) -> Result<BlobAnswer, BlobError> {
    use std::os::fd::AsFd;
    let (mut answer, give) = reply?;
    let Some(give) = give else {
        return Ok(answer);
    };
    let (fd, kind) = match &give {
        Give::Write(fd) => (fd, crate::native::Passed::BlobWrite),
        Give::Read(fd) => (fd, crate::native::Passed::Snapshot),
    };
    let passed = send(fd.as_fd(), kind).map_err(BlobError::Other)?;
    match &mut answer {
        BlobAnswer::Created { fd, .. } | BlobAnswer::Opened { fd, .. } => *fd = passed,
        BlobAnswer::Unit => {}
    }
    Ok(answer)
}

/// The memory behind a blob: a sealed memfd, as `bulkhead_native::channel`
/// makes for published snapshots.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod memory {
    use std::io;
    use std::os::fd::OwnedFd;

    use bulkhead_native::protocol::BlobError;
    use rustix::fs::{MemfdFlags, SealFlags};

    /// A blob of `size` bytes, sealed against growing and shrinking.
    pub(super) fn make(size: u64) -> io::Result<OwnedFd> {
        let fd = rustix::fs::memfd_create(
            "bulkhead-blob",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )?;
        rustix::fs::ftruncate(&fd, size)?;
        rustix::fs::fcntl_add_seals(&fd, SealFlags::GROW | SealFlags::SHRINK)?;
        Ok(fd)
    }

    /// Another descriptor of the same blob, for an app.
    pub(super) fn copy(fd: &OwnedFd) -> io::Result<OwnedFd> {
        Ok(rustix::io::fcntl_dupfd_cloexec(fd, 0)?)
    }

    /// Seal it against writes for good; `Mapped` while a writable mapping
    /// remains (both systems refuse with `EBUSY`).
    pub(super) fn seal(fd: &OwnedFd) -> Result<(), BlobError> {
        match rustix::fs::fcntl_add_seals(fd, SealFlags::WRITE | SealFlags::SEAL) {
            Ok(()) => Ok(()),
            Err(rustix::io::Errno::BUSY) => Err(BlobError::Mapped),
            Err(e) => Err(BlobError::Other(format!("sealing a blob: {e}"))),
        }
    }
}

/// Elsewhere there are no native apps, so no blobs.
#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
mod memory {
    use std::io;
    use std::os::fd::OwnedFd;

    use bulkhead_native::protocol::BlobError;

    fn none() -> io::Error {
        io::Error::other("blobs need Linux or FreeBSD")
    }

    pub(super) fn make(_size: u64) -> io::Result<OwnedFd> {
        Err(none())
    }

    pub(super) fn copy(_fd: &OwnedFd) -> io::Result<OwnedFd> {
        Err(none())
    }

    pub(super) fn seal(_fd: &OwnedFd) -> Result<(), BlobError> {
        Err(BlobError::Other(none().to_string()))
    }
}
