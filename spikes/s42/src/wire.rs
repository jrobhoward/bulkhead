//! The spike's frames on `bulkhead-native`'s rings: `[len u32][kind u8]
//! [a u64][b u64][payload]`, `len` counting what follows it. Not the
//! node's protocol: only what S42 needs to drive a writer and a reader.

/// Node to writer: send `a` bytes to the node as one `DATA` frame.
pub const RING_SEND: u8 = 1;
/// Node to writer: make a blob of `a` bytes (ask, fill, report), as `b`
/// says ([`MODE_PWRITE`], [`POPULATE`], [`KEEP`], [`PROBE`]).
pub const BLOB_MAKE: u8 = 2;
/// Node to reader: read blob `a` (its descriptor passed as id `a`), as
/// `b` says ([`POPULATE`], [`KEEP`]); answered `OK(sum)`.
pub const BLOB_READ: u8 = 3;
/// Node to app: try to change blob `a` every way; answered `TEXT`.
pub const TRY: u8 = 4;
/// Node to writer: write through the kept mapping of blob `a`.
pub const POKE: u8 = 5;
/// Node to reader: sum the kept mapping of blob `a` again; `OK(sum,
/// first byte)`.
pub const RESUM: u8 = 6;
/// Node to app: drop blob `a`'s mapping and descriptor.
pub const DROP: u8 = 7;
/// Node to app: who are you; answered `TEXT`.
pub const WHO: u8 = 8;
/// Node to app: end.
pub const EXIT: u8 = 9;
/// Either way: bytes (to the reader: sum them, answer `OK`).
pub const DATA: u8 = 10;
/// Writer to node: a blob of `a` bytes, please.
pub const REQ: u8 = 11;
/// Node to writer: blob `a` granted, its descriptor passed as id `a`.
pub const GRANT: u8 = 12;
/// Writer to node: blob `a` is filled; `b` the errno of an `ftruncate`
/// tried before filling ([`PROBE`]), or 0.
pub const FILLED: u8 = 13;
/// App to node: done, with `a` and `b`.
pub const OK: u8 = 14;
/// App to node: a report.
pub const TEXT: u8 = 15;

/// Fill with `pwrite`, not a mapping.
pub const MODE_PWRITE: u64 = 1;
/// Map with `MAP_POPULATE` (Linux) or `MAP_PREFAULT_READ` (FreeBSD).
pub const POPULATE: u64 = 2;
/// Keep the mapping (and descriptor) after the operation.
pub const KEEP: u64 = 4;
/// Try `ftruncate` before filling, and report its errno.
pub const PROBE: u64 = 8;
/// Read with `pread` into a buffer the reader reuses, not a mapping.
pub const COPY: u64 = 16;

/// A frame's header and payload.
pub struct Frame {
    pub kind: u8,
    pub a: u64,
    pub b: u64,
    pub payload: Vec<u8>,
}

/// The header of a frame of `kind` with `payload_len` bytes after it.
pub fn header(kind: u8, a: u64, b: u64, payload_len: usize) -> [u8; 21] {
    let mut h = [0u8; 21];
    let len = u32::try_from(17 + payload_len).expect("a frame under 4 GiB");
    h[..4].copy_from_slice(&len.to_le_bytes());
    h[4] = kind;
    h[5..13].copy_from_slice(&a.to_le_bytes());
    h[13..21].copy_from_slice(&b.to_le_bytes());
    h
}

/// Parse the 17 bytes after the length.
pub fn parse(head: &[u8; 17], payload: Vec<u8>) -> Frame {
    Frame {
        kind: head[0],
        a: u64::from_le_bytes(head[1..9].try_into().unwrap()),
        b: u64::from_le_bytes(head[9..17].try_into().unwrap()),
        payload,
    }
}

/// The bytes every writer writes for a blob or frame of `n` bytes.
pub fn pattern(n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| (i.wrapping_mul(31).wrapping_add(7)) as u8)
        .collect()
}

/// A sum that reads every byte: little-endian words, then the rest.
pub fn sum(bytes: &[u8]) -> u64 {
    let mut s = 0u64;
    let mut words = bytes.chunks_exact(8);
    for w in &mut words {
        s = s.wrapping_add(u64::from_le_bytes(w.try_into().unwrap()));
    }
    for &b in words.remainder() {
        s = s.wrapping_add(u64::from(b));
    }
    s
}
