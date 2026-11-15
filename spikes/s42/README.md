# S42: blobs, the claim check

Spike S42 of `docs/ROADMAP.md` (Phase 7, before DA5; decided with the user 2026-11-15). Large data between apps, and between apps and drivers, passed by reference: a writer gets a blob from the node, fills it, and sends a message carrying its id; a reader opens the id and reads in place; the node frees it when the last holder lets go. S42 asks whether the mechanism holds under the runner's fences on both systems, who pays for the memory, what the node can revoke, and from what size it beats the node's rings.

The spike is one binary in three roles. The node role makes the blobs and passes descriptors on `bulkhead-native`'s descriptor socket; the writer and reader run through the real `bulkhead-runner --executable` on channels of `bulkhead-native` (the rings, doors and bells the node uses). Built from this directory alone (`cargo build --release`, `bulkhead-native` by path); `scripts/run.sh checks`, `root-checks` (as root) and `bench`.

The mechanism, as tested:
1. The writer asks for a blob of N bytes. The node makes a memfd (on FreeBSD an anonymous shared memory object), sizes it, seals it against growing and shrinking, keeps a descriptor and passes the writer one (on FreeBSD limited to `rights::MEMFD` and `CAP_SEEK`).
2. The writer fills it, mapped (`mmap`, copy, `munmap`) or with `pwrite`, and says so.
3. The node seals it against writes (`F_SEAL_WRITE`, `F_SEAL_SEAL`) and passes a reader a descriptor (on FreeBSD `rights::SNAPSHOT`). The reader checks the seal, maps it read-only (or `pread`s it) and reads.

Run 2026-11-15. Function: the 16-core machine (Linux 6.8) as the user, the Ubuntu 26.04 VM (Linux 7.0) and the FreeBSD 15 VM, each as the user and as root with the writer and reader under users 61001 and 61002 (and on Linux a cgroup each). Cost: release builds, two rounds each, the 16-core machine (no VM running, load under 1) and the Raspberry Pi 5 (load under 1.3). Results: `results/checks-2026-11-15.txt`, `results/{x86,pi5}-2026-11-15.txt`.

## Answers

- **The mechanism holds on both systems, under the fences.** Every check passes on Linux (Landlock and seccomp) and FreeBSD (Capsicum), as one user and as two. After the seal neither app can write, resize or map the blob writable (`EPERM`, `EACCES`, and on FreeBSD `ENOTCAPABLE` from the rights), and a reader cannot `mprotect` its read-only mapping writable. The size seal holds from the grant: the writer cannot resize a blob it was given.
- **The node must seal; the writer must unmap first.** Both systems refuse `F_SEAL_WRITE` while any writable shared mapping exists (`EBUSY`), so a writer that keeps its mapping gets its blob refused, and the node never hands out a blob that can still change. Linux's `F_SEAL_FUTURE_WRITE` would allow the seal with the mapping kept, and a write through that mapping still landed afterwards: it cannot make a blob immutable, so BL1 does not use it.
- **The writer pays, on Linux; every holder, on FreeBSD.** Linux charges a blob's pages to the cgroup of the process that wrote them, never to readers; when the writer's cgroup goes, the charge moves to its parent (under the helper, from `apps/<app>/i<id>` to `apps/<app>`), and it ends when the last descriptor and mapping close. FreeBSD counts a mapped blob in each holder's resident memory (`rctl`: writer 44 MiB, reader 23 MiB for a 20 MiB blob) and address space, so a reader that maps a blob spends its own `memory_max` there; one that `pread`s pays for its copy instead.
- **The node cannot revoke a blob.** A reader keeps reading a 20 MiB blob after the node closed its descriptor and the writer exited: a descriptor or mapping in an app lives until the app drops it or exits. Expiry can only stop new opens and drop the node's own reference.
- **Two users change nothing.** A descriptor carries its access; no file permissions are involved, under the helper's users or not.
- **Blobs are not the fix for large bodies.** The same bytes through `bulkhead-native`'s rings, writer to node to reader (two crossings, as through a driver), against a blob made, filled, sealed, passed and read:

| | Rings, p50 and CPU | Blob, best way | Blob, pages made ahead |
| --- | --- | --- | --- |
| 16-core, 64 KiB | 14–18 µs, 47–54 µs | 117–140 µs, 213–233 µs | 120–136 µs, 239–258 µs |
| 16-core, 1 MiB | 913–930 µs, 1.70–1.72 ms | 909–972 µs, 1.05–1.09 ms (`pwrite`, mapped) | 794–946 µs, 0.91–1.07 ms |
| 16-core, 20 MiB | 20.4–20.7 ms, 35.7–36.1 ms | 18.0–18.1 ms, 18.1 ms (`pwrite`, mapped) | 18.5–20.9 ms |
| Pi, 64 KiB | 14 µs, 42–43 µs | 92–119 µs, 184–215 µs | 51 µs, 140 µs |
| Pi, 1 MiB | 389 µs, 0.71 ms | 1.09–1.18 ms, 1.24–1.31 ms | 783–792 µs, 0.91–0.92 ms |
| Pi, 20 MiB | 14.8–15.8 ms, 24–26 ms | 17.0–17.9 ms, 17–18 ms (`pwrite`, mapped) | 15.0–15.3 ms, 15.2–15.5 ms |

  - A blob costs about 100 µs before its first byte (a memfd, its size and seals, two descriptors passed, two mappings made and removed), and then a page fault, a zeroed page and its freeing every 4 KiB on top of the copies. The rings copy four times but into memory that stays warm.
  - So the rings are faster at 64 KiB everywhere, and at 1 MiB on the Pi; at 1 MiB on the 16-core machine they tie in time and the blob takes two fifths less CPU; at 20 MiB the blob takes half the CPU on the 16-core machine and a third less on the Pi, and is faster on the 16-core machine and slower on the Pi (even with its pages made ahead). Pages made ahead (`fallocate`, as a pool of ready blobs would) save 15 to 45% of a blob's time at 64 KiB and 1 MiB, and their making still costs CPU somewhere. `MAP_POPULATE` changed nothing (dropped from the table). Huge pages for shared memory, which would cut the faults 512-fold, are off by default (`shmem_enabled = never`) on both machines and need the operator.
  - **The real node is the gap.** These rings move 1 MiB through the node twice in 0.4 to 0.9 ms of time and 0.7 to 1.7 ms of CPU. Through the real node, the same two crossings (an egress driver's 1 MiB response to its app) added 1.6 to 2.8 ms (BASELINE.md, 2026-11-15), two to seven times this floor, and one 1 MiB request costs 7 to 9.5 ms of CPU in the node's own ingress with HTTP included (S37): most of the cost is in the node's handling of a large frame (a wake-up per 64 KiB piece, the envelope built and parsed whole as CBOR, copies), not in the rings. That is PF1 again, and it is worth more than blobs would save.

## What follows for the roadmap

- **PF1 stays a performance item of its own**, ahead of blobs for bodies: the node's large frames brought near the rings' floor measured here.
- **BL2 (bodies as blobs) narrows** to what frames cannot carry: bodies over the 16 MiB frame limit, and bodies many readers share.
- **BL1 stays, for what blobs do that frames cannot:** data over 16 MiB; data written once and read later, by another app, or by several without copies; and data the node never holds (a 20 MiB message through the rings is 20 MiB of the node's own memory while it passes).

## The design BL1 builds on

- **Requests** (protocol): `BlobCreate { size }` answered with a blob id and a writable descriptor (passed as today's descriptors are, a new `Passed::BlobWrite`: on FreeBSD `MEMFD`'s rights and `CAP_SEEK`); `BlobSeal { id }`, after the writer unmapped (the SDK does it), refused with the reason while a writable mapping exists; `BlobShare { id, app }`, allowed where the messaging ACL lets the writer's app call that app; `BlobOpen { id }` answered with a read-only descriptor (`Passed::Snapshot`), for the writer's app and the apps it shared with; `BlobRelease { id }`. A message carries the id as plain data: the node does not look inside messages, so access is the share list, not possession of the id.
- **The node's table:** per blob its size, the writing app, the apps it is shared with, who opened it, and its expiry. The node keeps its descriptor until the writer releases it and every opener released it or exited, or until expiry; expiry stops new opens and drops the node's descriptor, and an app's own copy lives until it drops it.
- **Quota:** blob bytes counted against the writing app at create, `blobs.max_bytes` in the manifest under a node ceiling, until the node drops its descriptor. That is the portable rule: Linux's cgroups also charge the writer (and its app's cgroup after the instance ends), while FreeBSD charges each holder's mapping, so readers there should `pread` large blobs or have the room.
- **The SDK:** create, write (`pwrite` or a mapping it removes before sealing), seal, share, send the id in a message; open, read (mapped or copied), release. A blob below about 1 MiB is better sent in the message.

## Not measured

- Fan-out: one blob read by several readers against the same bytes sent to each through the rings (the case where blobs should win most).
- Huge pages for shared memory (`shmem_enabled = advise` and `MADV_HUGEPAGE`), which needs root on the test machines.
- The node's own large-frame path with PF1's changes, against the rings' floor here.
