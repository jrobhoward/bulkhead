//! Blobs: data passed between apps as sealed memory (`bulkhead:blobs`,
//! ROADMAP BL1), for what a message carries badly: more than a message's
//! 16 MiB, data written once and read later or by several apps without a
//! copy each, data the node should not hold.
//!
//! A writer makes a blob of a fixed size, fills it and seals it; from then
//! on nobody can change it. It shares the blob with the apps that should
//! read it and sends the [`BlobId`] in a message of its own; an instance
//! of a shared app opens it and reads it, mapped or copied. Each app
//! releases it when done: the node frees it once the writing app and every
//! app it was shared with released it, or when its time runs out
//! (`ttl`, capped by the node). A blob counts against the writing app's
//! `max_bytes` until then.
//!
//! ```no_run
//! # use bulkhead_sdk::blob::{self, BlobWriter};
//! # fn run(report: &[u8]) -> Result<(), blob::BlobError> {
//! let blob = BlobWriter::create(report.len() as u64)?;
//! blob.write_at(0, report)?;
//! let id = blob.seal(None)?;
//! blob::share(id, "archiver")?;
//! // ... send `id` to the archiver in a message ...
//! blob::release(id)?;
//! # Ok(())
//! # }
//! ```
//!
//! ```no_run
//! # use bulkhead_sdk::blob::{self, BlobId};
//! # fn run(id: BlobId) -> Result<(), blob::BlobError> {
//! let blob = blob::open(id)?;
//! let bytes = blob.map()?;
//! // ... read `bytes` ...
//! drop(bytes);
//! blob::release(id)?;
//! # Ok(())
//! # }
//! ```
//!
//! A blob below about 1 MiB is cheaper sent in the message itself (spike
//! S42). A descriptor the app opened stays readable after the blob is
//! freed, until it is dropped. On FreeBSD a mapped blob counts in the
//! reader's memory; [`BlobReader::read_at`] copies instead.
//!
//! From a thread of the app's own, use [`crate::sender::Sender::blobs`].

#[cfg(test)]
#[path = "blob_tests.rs"]
mod tests;

use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::time::Duration;

use bulkhead_native::app;
use bulkhead_native::protocol as p;

/// A blob's id, to send in a message. It grants nothing by itself: only
/// the writing app and apps it shared the blob with may open it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct BlobId(pub u64);

impl std::fmt::Display for BlobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for BlobId {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse().map(Self)
    }
}

/// Why a blob request failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BlobError {
    /// Not granted, not this app's or instance's to do, or a share the
    /// messaging ACLs refuse.
    AccessDenied,
    /// No such blob: never made, freed, or expired.
    NotFound,
    /// Over the app's `max_bytes`.
    OverQuota,
    /// Larger than the node's largest blob, or empty.
    TooLarge,
    /// A writable mapping of it remains, so it cannot be sealed.
    Mapped,
    /// Not sealed yet.
    NotSealed,
    /// Reading or writing it failed, or anything else.
    Other(String),
}

impl std::fmt::Display for BlobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AccessDenied => f.write_str("access denied"),
            Self::NotFound => f.write_str("no such blob"),
            Self::OverQuota => f.write_str("over the app's blob quota"),
            Self::TooLarge => f.write_str("larger than the node's largest blob, or empty"),
            Self::Mapped => f.write_str("a writable mapping of the blob remains"),
            Self::NotSealed => f.write_str("the blob is not sealed"),
            Self::Other(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for BlobError {}

impl From<p::BlobError> for BlobError {
    fn from(e: p::BlobError) -> Self {
        match e {
            p::BlobError::Denied => Self::AccessDenied,
            p::BlobError::NotFound => Self::NotFound,
            p::BlobError::OverQuota => Self::OverQuota,
            p::BlobError::TooLarge => Self::TooLarge,
            p::BlobError::Mapped => Self::Mapped,
            p::BlobError::NotSealed => Self::NotSealed,
            p::BlobError::Other(why) => Self::Other(why),
        }
    }
}

fn io(e: &std::io::Error) -> BlobError {
    BlobError::Other(e.to_string())
}

/// Who asks the node: the callback thread, or a sender.
pub(crate) type Ask<'a> = &'a mut dyn FnMut(p::BlobRequest) -> Result<p::BlobAnswer, p::BlobError>;

fn callback(request: p::BlobRequest) -> Result<p::BlobAnswer, p::BlobError> {
    app::Ctx::current().blob(request)
}

fn unexpected(answer: &p::BlobAnswer) -> BlobError {
    BlobError::Other(format!("unexpected answer from the node: {answer:?}"))
}

fn take(fd: p::FdRef) -> Result<File, BlobError> {
    app::take_fd(fd).map(File::from).map_err(|e| io(&e))
}

/// A blob being filled. Seal it to let others read it; dropped unsealed,
/// it is freed when the instance ends or the app releases it.
#[derive(Debug)]
pub struct BlobWriter {
    id: BlobId,
    size: u64,
    file: File,
}

impl BlobWriter {
    /// A new blob of `size` bytes, zeroed.
    ///
    /// # Errors
    ///
    /// [`BlobError::AccessDenied`] without `bulkhead:blobs`,
    /// [`BlobError::TooLarge`], [`BlobError::OverQuota`].
    pub fn create(size: u64) -> Result<Self, BlobError> {
        Self::create_with(&mut callback, size)
    }

    pub(crate) fn create_with(ask: Ask<'_>, size: u64) -> Result<Self, BlobError> {
        match ask(p::BlobRequest::Create { size })? {
            p::BlobAnswer::Created { id, fd } => Ok(Self {
                id: BlobId(id.0),
                size,
                file: take(fd)?,
            }),
            other => Err(unexpected(&other)),
        }
    }

    /// Its id.
    #[must_use]
    pub fn id(&self) -> BlobId {
        self.id
    }

    /// Its size in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.size
    }

    /// Whether it holds no bytes (never: a blob has at least one).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Write `bytes` at `offset`.
    ///
    /// # Errors
    ///
    /// Past its end, or the write failed.
    pub fn write_at(&self, offset: u64, bytes: &[u8]) -> Result<(), BlobError> {
        let end = offset.saturating_add(bytes.len() as u64);
        if end > self.size {
            return Err(BlobError::Other(format!(
                "writing to byte {end} of a {}-byte blob",
                self.size
            )));
        }
        self.file.write_all_at(bytes, offset).map_err(|e| io(&e))
    }

    /// Seal it against any further write: it lives `ttl` at most (`None`
    /// for the node's default; the node caps it), or until its app and
    /// every app it is shared with released it. Its id, to share and send.
    ///
    /// # Errors
    ///
    /// As the node refuses it.
    pub fn seal(self, ttl: Option<Duration>) -> Result<BlobId, BlobError> {
        self.seal_with(&mut callback, ttl)
    }

    pub(crate) fn seal_with(
        self,
        ask: Ask<'_>,
        ttl: Option<Duration>,
    ) -> Result<BlobId, BlobError> {
        let ttl_ms = ttl.map_or(0, |t| {
            u64::try_from(t.as_millis()).unwrap_or(u64::MAX).max(1)
        });
        match ask(p::BlobRequest::Seal {
            id: p::BlobId(self.id.0),
            ttl_ms,
        })? {
            p::BlobAnswer::Unit => Ok(self.id),
            other => Err(unexpected(&other)),
        }
    }
}

/// Let `app` open blob `id`, which this app wrote and sealed, where the
/// messaging ACLs let this app call `app`.
///
/// # Errors
///
/// [`BlobError::AccessDenied`] outside the ACLs or for a blob of another
/// app, [`BlobError::NotSealed`], [`BlobError::NotFound`].
pub fn share(id: BlobId, app: &str) -> Result<(), BlobError> {
    share_with(&mut callback, id, app)
}

pub(crate) fn share_with(ask: Ask<'_>, id: BlobId, app: &str) -> Result<(), BlobError> {
    unit(ask(p::BlobRequest::Share {
        id: p::BlobId(id.0),
        app: app.to_string(),
    })?)
}

/// This app is done with blob `id`.
///
/// # Errors
///
/// [`BlobError::AccessDenied`] for a blob this app may not open,
/// [`BlobError::NotFound`].
pub fn release(id: BlobId) -> Result<(), BlobError> {
    release_with(&mut callback, id)
}

pub(crate) fn release_with(ask: Ask<'_>, id: BlobId) -> Result<(), BlobError> {
    unit(ask(p::BlobRequest::Release {
        id: p::BlobId(id.0),
    })?)
}

fn unit(answer: p::BlobAnswer) -> Result<(), BlobError> {
    match answer {
        p::BlobAnswer::Unit => Ok(()),
        other => Err(unexpected(&other)),
    }
}

/// Open blob `id` to read: this app wrote it, or it was shared with it.
///
/// # Errors
///
/// [`BlobError::AccessDenied`], [`BlobError::NotSealed`],
/// [`BlobError::NotFound`].
pub fn open(id: BlobId) -> Result<BlobReader, BlobError> {
    open_with(&mut callback, id)
}

pub(crate) fn open_with(ask: Ask<'_>, id: BlobId) -> Result<BlobReader, BlobError> {
    match ask(p::BlobRequest::Open {
        id: p::BlobId(id.0),
    })? {
        p::BlobAnswer::Opened { size, fd } => Ok(BlobReader {
            id,
            size,
            file: take(fd)?,
        }),
        other => Err(unexpected(&other)),
    }
}

/// A sealed blob, open to read. It stays readable until dropped, even
/// after the node freed the blob.
#[derive(Debug)]
pub struct BlobReader {
    id: BlobId,
    size: u64,
    file: File,
}

impl BlobReader {
    /// Its id.
    #[must_use]
    pub fn id(&self) -> BlobId {
        self.id
    }

    /// Its size in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.size
    }

    /// Whether it holds no bytes (never: a blob has at least one).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Copy bytes from `offset` into `buf`: how many (fewer at its end).
    ///
    /// # Errors
    ///
    /// The read failed.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, BlobError> {
        self.file.read_at(buf, offset).map_err(|e| io(&e))
    }

    /// Its bytes in place, mapped read-only: no copy.
    ///
    /// # Errors
    ///
    /// It could not be mapped.
    pub fn map(&self) -> Result<BlobMap, BlobError> {
        let fd = OwnedFd::from(self.file.try_clone().map_err(|e| io(&e))?);
        bulkhead_native::channel::Mapped::open(&fd)
            .map(BlobMap)
            .map_err(|e| io(&e))
    }
}

/// A sealed blob's bytes, mapped read-only; unmapped when dropped.
#[derive(Debug)]
pub struct BlobMap(bulkhead_native::channel::Mapped);

impl std::ops::Deref for BlobMap {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.0.bytes()
    }
}
