# S43: bulk transfers over an emulated WAN

Spike S43 of `docs/ROADMAP.md` (Phase 8, DR3b; 2026-11-20). DR5's drill, with 50 ms of delay (up to 20 ms more) and 5% loss each way between the sites, found bundles taking 96 to 171 s to reach the DR node and a 1 MiB snapshot chunk 10 to 20 s, all on the one ordered data stream between two nodes. S43 asks what limits the rate, whether smaller chunks, a stream per transfer or several chunks in flight would help, and what each costs the other frames between the nodes.

One binary: a quinn client and server configured as the node's transport is (`transport/quic.rs`: the workspace's quinn 0.11 and features, its idle timeout and keep-alive), talking through a UDP proxy that delays, jitters and drops each datagram as the soak's `Mode::Wan` does (`crates/bulkhead-soak/src/soak/proxy.rs`): each datagram waits the delay plus a uniform draw up to the jitter, so datagrams overtake each other. The client asks for chunks one at a time, as the DR node asks for bundle and snapshot chunks, or several at a time; during a transfer it sends a 16-byte ping every 500 ms, on the transfer's stream (behind its chunks, as frames go today) or on a second stream with the transfer's at a lower quinn priority.

```sh
cargo build --release
TOTAL_MIB=4 ./target/release/s43 > results/<date>-<machine>.txt   # every case, about 40 min
./target/release/s43 hol/                                          # cases whose name contains a filter
```

Run 2026-11-20 on the 16-core workstation (Linux 6.8), 4 MiB a case, one run each; a browser used about one core throughout and a release build and two soaks ran during some cases. The rates depend on the emulated link, not the CPU, and the differences below are tenfold, but the numbers are one run each. Results: `results/2026-11-20-workstation.txt`.

## Answers

- **The emulated WAN's reordering, more than its loss, limits Cubic.** One 1 MiB chunk at a time, KiB/s:

| WAN, each way | Cubic (quinn's default, the node's today) | NewReno | BBR |
| --- | --- | --- | --- |
| loopback | 15,138 | 16,976 | 14,311 |
| 50 ms, no jitter, no loss | 468 | 2,669 | 3,244 |
| 50 ms + 20 ms jitter, no loss | 67 | 56 | 721 |
| 50 ms + 20 ms jitter, 0.1% loss | 64 | 57 | 641 |
| 50 ms + 20 ms jitter, 1% loss | 59 | 52 | 623 |
| 50 ms, no jitter, 5% loss | 69 | 64 | 1,537 |
| 50 ms + 20 ms jitter, 5% loss (the drill's) | 43 | 35 | 633 |

  Jitter drawn per datagram reorders them by up to 20 ms, which QUIC's loss detection reads as loss, so Cubic and NewReno shrink their windows with or without real loss. Real links reorder far less than this proxy does, so the drill's transfer times are pessimistic for Cubic; nothing here was measured on a real WAN. Cubic's 468 KiB/s with no jitter or loss is its slow start over 4 MiB. BBR, which does not read loss as congestion, kept 600 to 3,200 KiB/s throughout; quinn marks its BBR "experimental".
- **Chunk size does not change the rate.** On the drill's WAN with Cubic, 37 to 44 KiB/s for chunks of 64 KiB, 256 KiB and 1 MiB, one ask at a time or four. With BBR, 64 KiB chunks one at a time lost two thirds (210 KiB/s: a round trip per chunk counts at that rate), and four in flight made up for it.
- **Chunk size decides how long everything else waits.** A ping during a transfer, on the drill's WAN:

| | Rate, KiB/s | Ping p50 | Ping p90 |
| --- | --- | --- | --- |
| Cubic, same stream, 1 MiB chunks | 42 | 22.6 s | 26.4 s |
| Cubic, same stream, 256 KiB | 41 | 5.6 s | 6.9 s |
| Cubic, same stream, 64 KiB | 38 | 1.1 s | 1.6 s |
| Cubic, own stream, 1 MiB | 44 | 165 ms | 234 ms |
| BBR, same stream, 1 MiB | 579 | 1.1 s | 1.6 s |
| BBR, same stream, 256 KiB | 438 | 288 ms | 691 ms |
| BBR, same stream, 64 KiB | 195 | 132 ms | 288 ms |
| BBR, own stream, 1 MiB | 583 | 126 ms | 146 ms |

  On the node, what waits behind a bundle or snapshot chunk is the DR node's entries and probes (its lag, so the writes an activation would lose) and any other data frame between the two nodes; heartbeats and failover go on the control stream and wait for nothing. A second stream at a lower priority takes the wait to a round trip or two at no cost in rate.

## What changed

Decided with the user on 2026-11-20 (ROADMAP DR3b):
- **256 KiB chunks** for snapshots (`ship::CHUNK_BYTES`) and bundles (`dr_deploys::CHUNK`), and 1 MiB batches for a reseed's entries (it asked 4 MiB): a wait of about 6 s behind a chunk over the drill's WAN instead of 23 s, at the same rate with Cubic.
- **The bundle fetch's backstop** went from 10 s and three asks to 60 s and two, as for snapshot chunks: at 10 s a fetch gave up over the drill's WAN.
- **Not done:** a stream for bulk transfers (the wait down to a round trip, but a change to the transport's streams, gated on its version), and BBR as an opt-in congestion controller (ten times the rate on this emulated link, unmeasured on a real one), recorded in the roadmap.
