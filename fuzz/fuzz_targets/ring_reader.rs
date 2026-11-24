//! The node's reader of the ring a native app writes (ROADMAP T1b): the
//! app shares the ring's memory, so a hostile one can store anything in
//! its positions and data while the node reads. Arbitrary sequences of
//! raw stores to `head`, `tail` and the data area, and of the node's
//! reads (`Ring::read`, and `Ring::read_into`, which the node's link uses
//! since ROADMAP PF1), then the node's frame splitter over what it read.
//!
//! Checked: a read returns at most what was asked and at most the ring's
//! capacity, moves `head` by exactly that, and fails (`Corrupt`) only
//! when the positions claim more than the capacity; every copy stays in
//! the mapping (AddressSanitizer); the splitter consumes no more than it
//! was given and never panics.

#![no_main]

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU64, Ordering};

use arbitrary::Arbitrary;
use bulkhead_native::channel::{CHANNEL_BYTES, Channel, HEADER, RING_BYTES, RING_CAP};
use libfuzzer_sys::fuzz_target;

/// The ring to the node's `head` and `tail` words, from its start.
const HEAD: usize = 0;
const TAIL: usize = 64;

#[derive(Debug, Arbitrary)]
enum Step {
    /// The app stores a reader position (the node's word: the app can
    /// write it all the same).
    SetHead(u64),
    /// The app stores a writer position.
    SetTail(u64),
    /// The app writes bytes into the data area at an offset.
    Write { at: u16, bytes: Vec<u8> },
    /// The node reads up to this many bytes.
    Read(u16),
    /// The node reads up to this many bytes onto the end of what it read,
    /// into spare capacity (`Ring::read_into`).
    ReadInto(u16),
}

/// The node's channel, and the app's own mapping of the same memory.
struct Shared {
    channel: Channel,
    _memfd: OwnedFd,
    app: *mut u8,
}

thread_local! {
    static SHARED: Shared = {
        let (channel, memfd) = Channel::create().unwrap();
        // SAFETY: a fresh shared mapping of the whole memfd, kept for the
        // process's life.
        let app = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                CHANNEL_BYTES,
                rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
                rustix::mm::MapFlags::SHARED,
                &memfd,
                0,
            )
        }
        .unwrap()
        .cast::<u8>();
        Shared { channel, _memfd: memfd, app }
    };
}

impl Shared {
    /// A word of the ring to the node, as the app sees it.
    fn word(&self, offset: usize) -> &AtomicU64 {
        // SAFETY: inside the mapping, 8-aligned (page-aligned ring start,
        // offsets 0 and 64), and only ever used atomically.
        unsafe { &*self.app.add(RING_BYTES + offset).cast::<AtomicU64>() }
    }
}

fuzz_target!(|steps: Vec<Step>| {
    SHARED.with(|s| {
        s.word(HEAD).store(0, Ordering::SeqCst);
        s.word(TAIL).store(0, Ordering::SeqCst);
        let ring = s.channel.to_node();
        let mut read = Vec::new();
        let mut buf = vec![0u8; usize::from(u16::MAX)];
        for step in steps.iter().take(64) {
            match step {
                Step::SetHead(h) => s.word(HEAD).store(*h, Ordering::SeqCst),
                Step::SetTail(t) => s.word(TAIL).store(*t, Ordering::SeqCst),
                Step::Write { at, bytes } => {
                    let at = usize::from(*at) % RING_CAP;
                    let n = bytes.len().min(RING_CAP - at);
                    // SAFETY: `at + n <= RING_CAP`, inside the data area.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            bytes.as_ptr(),
                            s.app.add(RING_BYTES + HEADER + at),
                            n,
                        );
                    }
                }
                Step::Read(n) => {
                    let n = usize::from(*n);
                    let head = s.word(HEAD).load(Ordering::SeqCst);
                    let queued = s.word(TAIL).load(Ordering::SeqCst).wrapping_sub(head);
                    match ring.read(&mut buf[..n]) {
                        Ok(k) => {
                            assert!(queued <= RING_CAP as u64, "read a corrupt ring");
                            assert_eq!(k as u64, (n as u64).min(queued));
                            assert!(k <= RING_CAP);
                            assert_eq!(
                                s.word(HEAD).load(Ordering::SeqCst),
                                head.wrapping_add(k as u64)
                            );
                            read.extend_from_slice(&buf[..k]);
                        }
                        Err(_) => assert!(queued > RING_CAP as u64, "refused a sound ring"),
                    }
                }
                Step::ReadInto(n) => {
                    let n = usize::from(*n);
                    let head = s.word(HEAD).load(Ordering::SeqCst);
                    let queued = s.word(TAIL).load(Ordering::SeqCst).wrapping_sub(head);
                    let before = read.len();
                    match ring.read_into(&mut read, n) {
                        Ok(k) => {
                            assert!(queued <= RING_CAP as u64, "read a corrupt ring");
                            assert_eq!(k as u64, (n as u64).min(queued));
                            assert_eq!(read.len(), before + k);
                            assert_eq!(
                                s.word(HEAD).load(Ordering::SeqCst),
                                head.wrapping_add(k as u64)
                            );
                        }
                        Err(_) => {
                            assert!(queued > RING_CAP as u64, "refused a sound ring");
                            assert_eq!(read.len(), before);
                        }
                    }
                }
            }
        }
        let (consumed, _broken) = bulkhead_host::frames_for_fuzzing(&read);
        assert!(consumed <= read.len());
    });
});
