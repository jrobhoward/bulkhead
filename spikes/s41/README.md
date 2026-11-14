# S41: the HTTP ingress driver on hyper and tokio

Spike S41 of `docs/ROADMAP.md`, the first step of DA2 (moving the node's HTTP ingress into a driver app). DA1's driver (`spikes/s37/driver-da1`) met Phase 7's target with a hand-written `httparse` loop. DA2 was decided (with the user, 2026-11-13) to run the node's own stack instead, hyper and rustls on tokio, inside the driver, so the driver keeps chunked bodies, timeouts, keep-alive and a road to HTTP/2 without a hand-written HTTP/1.1 server facing every client. S41 measures that against DA1's loop before DA2 builds on it.

The code lives in S37's workspace, so S37's harness runs it unchanged:

- `crates/bulkhead-native/src/sender_async.rs` (feature `tokio`): a sender channel awaited on tokio. Calls start from any task of the thread; a pump task reads the answers without waiting, spins for `sender_spin_us` while a call is unanswered, then arms the ring's waiting word and waits for the door in tokio's reactor.
- `spikes/s37/driver-hyper`: DA1's driver with hyper's HTTP/1.1 server and rustls, a tokio current-thread runtime and a sender channel per worker thread. Worker 0 accepts and hands each connection to the workers in turn, because the runner's seccomp filter allows `accept4` on the passed descriptor numbers only, so a duplicate cannot accept.

Run, from the main checkout, after `cargo build --release -p bulkhead-native --bin bulkhead-runner` and `(cd spikes/s37 && cargo build --release)`:

```sh
DRIVER=s37-driver-hyper S37_DRIVER=da1 sh spikes/s37/scripts/run.sh   # the hyper driver
DRIVER=s37-driver-da1   S37_DRIVER=da1 sh spikes/s37/scripts/run.sh   # DA1's loop
```

Run 2026-11-13, release builds, two rounds of S37's matrix each, each driver beside the node's own ingress in the same run:

- **Raspberry Pi 5:** Cortex-A76, 4 cores, Linux 7.0, nice 10, load under 1 at each start.
- **16-core machine:** Threadripper 1950X, Linux 6.8, no VM running. Kernel tasks stuck in an uninterruptible wait on a USB device held the load near 2 at no CPU (the CPUs 92% idle); each run started under a load of 3.

Results: `spikes/s37/results/{pi5,x86}-s41-*2026-11-13.txt`.

## Answer: the hyper driver, with a gap on the Pi

Requests a second as a share of the node's own ingress, and CPU a request against its CPU (the node and its instances, the driver among them):

| | DA1's loop | hyper driver |
| --- | --- | --- |
| Pi, GET, 32 connections | 84–91%, 1.18–1.27× | 75–76%, 1.46–1.48× |
| Pi, HTTPS GET, 32 | 86–95%, 1.18–1.29× | 75%, 1.48× |
| Pi, POST 1 KiB, 32 | 87–89%, 1.24–1.25× | 70%, 1.58× |
| Pi, GET, 256 | 79–84%, 1.29–1.37× | 70–71%, 1.54–1.56× |
| Pi, one connection, p50 above the node's: GET, HTTPS, POST 1 KiB | 26%, 27%, 18–19% | 38–39%, 31%, 26% |
| 16-core, GET, 32 | 159–160%, 1.05× | 147–153%, 1.07–1.08× |
| 16-core, HTTPS GET, 32 | 145–154%, 1.04–1.06× | 137–139%, 1.08–1.09× |
| 16-core, POST 1 KiB, 32 | 159–164%, 0.98–0.99× | 147–148%, 1.01× |
| 16-core, one connection, p50 above the node's: GET, HTTPS, POST 1 KiB | 2–6%, −3%, 1–3% | 13–14%, 18–21%, 9–11% |

- **The 16-core machine meets Phase 7's target** (two thirds of the node's requests at 32 connections, at most 1.5 times its CPU, one connection's p50 at most 20% above) but for HTTPS on one connection in one round of two (21%), and is 4 to 10% behind DA1's loop at 32 connections.
- **The Pi meets it for GET and HTTPS at 32 connections,** not for POST at 32 (1.58 times the CPU), and not for one connection, which DA1's loop misses too. The hyper driver is 11 to 21% behind DA1's loop at 32 connections, against the 10% the plan allowed.
- **Chosen anyway** (with the user, 2026-11-13): a hand-written HTTP server facing every client is the larger risk, and security comes before performance. What is left is recorded as ROADMAP PF2.

## What profiling found

The first build missed every target on the Pi: 63 to 65% of the node's requests at 32 connections at 1.72 to 1.98 times its CPU, one connection 85% above. A system-wide `perf` profile (root, 32 connections) put the driver at about 49 µs of CPU a request against DA1's 22 µs, and two system-call traces of one connection showed where:

- **A 64 KiB `memset` per look at the ring.** The pump grew its buffer by a zeroed 64 KiB chunk each time it read, and read several times a request: an unnamed libc function at 6.5% of all CPU (0.5% in DA1's driver). It now reads into a buffer zeroed once and copies what it read. That fix alone: 76 to 80% at 32 connections, 1.39 to 1.46 times the CPU.
- **The pump slept where it should have spun.** An answer the pump had read, but its caller had not taken yet, counted as a call in flight, so the pump spun then, after the answer, and was asleep on the door when the next call started: every answer was waited for in `epoll_wait`. It now spins and sleeps only while a call is unanswered. One connection's p50 on the Pi: 110 to 92 µs.
- **A spin with `sched_yield` holding the thread**, as a blocking sender spins, made the Pi worse with the second bug still in (one connection 173 µs, 72% at 32 connections) and was dropped. With the sender's spin at 0 the first build measured the same as at 50 µs: the spin was not the cost then.

What the traces show is left, per request, beside DA1's loop: tokio waking its own reactor with an eventfd write after each answer, and a second task between the ring and the connection (the pump, then the connection's task). Neither was changed here (PF2).
