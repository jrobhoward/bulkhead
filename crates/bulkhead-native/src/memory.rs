//! How much freed memory glibc's allocator keeps for reuse (ROADMAP PF1,
//! PF1b).
//!
//! By default glibc maps each block over about 128 KiB on its own and
//! gives it back on free, and trims an arena as soon as its top is free,
//! so every large message faulted its pages in again: a 1 MiB request
//! through the ingress driver spent most of its time in page faults and
//! `madvise`, and half of the node's CPU for a 1 MiB body between
//! instances on a Raspberry Pi 5 was the kernel zeroing and unmapping
//! pages. [`keep_freed_memory`] raises both thresholds, so what a process
//! frees below them is reused: its resident memory stays near its peak
//! use instead.
//!
//! FreeBSD's allocator (jemalloc) decays freed pages on its own, and other
//! C libraries are left as they are.

/// glibc's two thresholds, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    /// Blocks smaller than this come from the arenas rather than a mapping
    /// of their own.
    pub mmap: usize,
    /// An arena gives back its free top only once the top is this large.
    pub trim: usize,
}

/// What an app keeps: 32 MiB, above the largest frame
/// ([`crate::protocol::MAX_FRAME`]), since an app holds few threads.
pub const FOR_APP: Thresholds = Thresholds {
    mmap: 32 << 20,
    trim: 32 << 20,
};

/// What the node keeps: blocks under 4 MiB from the arenas, and up to
/// 8 MiB free at the top of each of its many arenas (ROADMAP DR3b), a
/// quarter of an app's. Enough for messages of 1 MiB to reuse their
/// buffers: with a trim threshold of 4 MiB the node's threads still gave
/// 4 MiB back over and over on a Raspberry Pi 5, and 8 MiB measured as
/// fast as 32 (BASELINE.md, 2026-11-24).
pub const FOR_NODE: Thresholds = Thresholds {
    mmap: 4 << 20,
    trim: 8 << 20,
};

/// What [`keep_freed_memory`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kept {
    /// Both thresholds set.
    Set,
    /// The environment sets them already (`MALLOC_MMAP_THRESHOLD_`,
    /// `MALLOC_TRIM_THRESHOLD_` or `GLIBC_TUNABLES`), which wins: an
    /// operator's choice for a node's unit (OPERATIONS.md).
    Environment,
    /// glibc refused them.
    Refused,
    /// Not glibc: nothing to set.
    Unsupported,
}

/// Have glibc keep freed memory for the next allocation, up to `kept`.
/// Call it before the process starts other threads.
#[must_use]
pub fn keep_freed_memory(kept: Thresholds) -> Kept {
    if set_by_environment(|name| std::env::var_os(name)) {
        return Kept::Environment;
    }
    set_thresholds(kept)
}

/// Whether the environment (read through `var`) sets either threshold.
fn set_by_environment(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    let set = |name| var(name).is_some();
    set("MALLOC_MMAP_THRESHOLD_")
        || set("MALLOC_TRIM_THRESHOLD_")
        || var("GLIBC_TUNABLES").is_some_and(|t| {
            let t = t.to_string_lossy();
            t.contains("glibc.malloc.mmap_threshold") || t.contains("glibc.malloc.trim_threshold")
        })
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn set_thresholds(kept: Thresholds) -> Kept {
    // glibc's `mallopt` parameters.
    const M_TRIM_THRESHOLD: i32 = -1;
    const M_MMAP_THRESHOLD: i32 = -3;
    unsafe extern "C" {
        fn mallopt(param: i32, value: i32) -> i32;
    }
    let value = |bytes: usize| i32::try_from(bytes).unwrap_or(i32::MAX);
    // SAFETY: `mallopt` only sets the allocator's parameters.
    let ok = unsafe {
        mallopt(M_MMAP_THRESHOLD, value(kept.mmap)) == 1
            && mallopt(M_TRIM_THRESHOLD, value(kept.trim)) == 1
    };
    if ok { Kept::Set } else { Kept::Refused }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn set_thresholds(_kept: Thresholds) -> Kept {
    Kept::Unsupported
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;
