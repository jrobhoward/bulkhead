# S40: client libraries that open their own sockets

Spike S40 of `docs/ROADMAP.md` (Phase 9). Libraries such as Kafka, AMQP and MQTT clients open their own sockets, which a confined app cannot today:
- the runner's seccomp filter refuses `socket`, `connect` and `accept`, and its Landlock ruleset denies every network right;
- a Linux instance has an empty network namespace of its own;
- a FreeBSD instance's jail has no addresses, and capability mode refuses `connect` and `bind` to network addresses.

AP4's `unconfined` lifts every fence at once. S40 asks whether an app can open its own sockets while the manifest's `bulkhead:sockets` rules (`connect` and `bind`, each a list of `{ cidr, ports }`, with the node's address guard) are still enforced, by the kernel rather than the node. It also asks what that costs, and what the app gives up. It decides whether node-made sockets stay the default with own sockets as an opt-in profile (AP2), and which mechanism each system uses.

Run 2026-11-22:
- **Linux:** the Raspberry Pi 5 (aarch64, Ubuntu 26.04, Linux 7.0), idle (load 0.33 at the start).
- **x86-64 Linux:** an Ubuntu 26.04 VM (Linux 7.0, 4 vCPUs) on the 16-core machine, running at the same time as the FreeBSD VM. A function check only.
- **FreeBSD:** a FreeBSD 15.0 VM (amd64, 4 vCPUs) on the same machine. Its times are function checks only: the VM shares the host's cores.

Results:
- `results/pi5-linux-2026-11-22.md`;
- `results/freebsd-vm-2026-11-22.md`;
- `results/x86-vm-linux-2026-11-22.md`;
- `results/raw/`, each run's whole output.

## What was built

One binary, `s40`. `launch` runs as root and stands in for the helper and the runner; the real helper is unchanged. It does three things:
- **Targets.** It starts, outside the fence: listeners, a sink, a UDP drain, datagrams sent to the app from an address outside its rules, and on Linux an abstract Unix socket.
- **The fence.** It sets up the fence named by `--fence`.
- **The child.** It starts `s40 enter` under `nobody` inside the fence. `enter` applies the child's own fences, then runs a role in the same process.

### The fences

| `--fence` | system | what it is |
| --- | --- | --- |
| `none` | Linux | a cgroup `s40/apps/<app>` and the host's network namespace, nothing else (as an unconfined app's) |
| `bpf` | Linux | plus six `BPF_PROG_TYPE_CGROUP_SOCK_ADDR` programs attached to the app's cgroup (`connect4`, `connect6`, `sendmsg4`, `sendmsg6` for the `connect` rules with the guard, `bind4`, `bind6` for the `bind` rules) |
| `nft` | Linux | plus an `inet` table whose output chain matches `socket cgroupv2 level 3 "s40/apps/<app>"`: a `connect` rule accepts packets to its block and ports, a `bind` rule replies from its ports; the rest are reset (TCP) or dropped |
| `none` | FreeBSD | a jail on `/` with the host's addresses (`ip4=inherit`), no capability mode |
| `pf` | FreeBSD | a jail with an address of its own (`ip4.addr`, an alias on `lo1`) and pf rules from that address in anchor `s40/<app>` |
| `ipfw` | FreeBSD | a jail with the host's addresses and ipfw rules matching `out jail <jid>` |

**The BPF programs.** `bpf.rs` generates them from the rules: 7 instructions with no rules, about 6 more per IPv4 rule in an IPv4 program and 9 in an IPv6 one (which checks IPv4 rules against mapped addresses), more where the guard applies. They are plain instructions (masked 32-bit compares on `user_ip4` or `user_ip6[]`, `be16` on `user_port`, port ranges), loaded through `bpf(2)` with `libc`:
- no compiler, no BPF crate and no helper call, so no GPL-only helper;
- an IPv6 program checks IPv4 rules against IPv4-mapped addresses, and IPv6 rules skip them;
- the guard skips a rule for the loopback, link-local and unspecified ranges it covers without lying inside;
- 64 rules make 393 instructions (IPv4) and 588 (IPv6), against the verifier's limit of a million.

`bpf_tests.rs` runs each program in a small interpreter against `rules::allows`, the reference, for 25 addresses, 12 ports, both families and with and without the guard. It also checks that every instruction is reachable, which the verifier requires: a family with no rules gets no allow block.

**pf and ipfw.** Both stop at the first match, so the guard becomes order:
1. rules inside a guarded range;
2. a block of each guarded range a broader rule covers;
3. the broader rules;
4. a block of the rest (pf: `block drop`; ipfw: `deny`, TCP and UDP rules each).

**The child's own fences** (`enter.rs`, Linux):
- **Landlock (ABI 6):** the filesystem read-only, and abstract Unix sockets and signals scoped. With `--landlock-net on`, TCP connect and bind are limited to the rules' ports: one Landlock rule per port, 23 for the probe's rules. Landlock rules cannot say "any port", so a rule without ports turns that half off.
- **seccomp:** `socket` is refused for domains other than `AF_UNIX`, `AF_INET` and `AF_INET6`, for `SOCK_RAW`, `SOCK_RDM` and `SOCK_PACKET`, and `io_uring_setup` is refused. It is a deny-list on top of the default, not the runner's allow-list.

FreeBSD has no counterpart: the jail and the packet filter are the fence.

### The roles

- **`probe`.** A matrix of 30 cases on Linux and 25 on FreeBSD, one line each with the result or errno:
  - TCP: allowed, wrong port, wrong address, the guard's three ranges under a broad rule, IPv4-mapped, IPv6, TCP Fast Open;
  - UDP: `sendto` allowed, wrong port, wrong address, mapped; `connect`; a connected socket's `sendto` to another address;
  - `bind` inside and outside the rules and to port 0; listen and accept;
  - raw, packet, netlink and routing sockets; an abstract Unix socket outside;
  - `getaddrinfo` for `localhost` and for a DNS name;
  - a datagram from outside the rules to a bound port.
- **`bench`.**
  - TCP connect and close (`SO_LINGER` 0) to `A:7001`, 20,000 times;
  - unconnected UDP `sendto` of 64 bytes, 200,000 times;
  - 256 MiB over one TCP connection.
  - Rules files of 2, 16 and 64 rules, the two the bench uses last, so a fence that checks in order walks the rest first.
- **`passed`.** The stand-in for node-made sockets: the launcher connects and passes each stream over `SCM_RIGHTS`, and sends on each datagram it receives over a Unix socket. The real node's path (the rings, its task) costs more than this stand-in. It was not measured here.
- **`mqtt`.** rumqttc (Apache-2.0) against a local mosquitto:
  - the time to CONNACK;
  - 100,000 QoS 0 messages of 64 bytes through the broker to a subscriber;
  - 20,000 QoS 1 publishes acknowledged;
  - a broker port outside the rules.

## Results

### Enforcement (Pi; the x86-64 VM gave the same results case for case)

"ok" is allowed, an errno is a refusal, "timeout" a hang until the probe's 0.8 s. Landlock net is off except in the `+ll` column.

| case | none | none+ll | bpf | bpf+ll | nft |
| --- | --- | --- | --- | --- | --- |
| TCP to the allowed address and port | ok | ok | ok | ok | ok |
| TCP, port not in the rules | ok | EACCES | EPERM | EACCES | ECONNREFUSED |
| TCP, address not in the rules (127.0.0.2) | ok | ok | EPERM | EPERM | ECONNREFUSED |
| TCP 127.0.0.1:7100 under `0.0.0.0/0` port 7100 (guard) | ok | ok | EPERM | EPERM | ECONNREFUSED |
| TCP 169.254.169.254 and 0.0.0.0 under the broad rule | timeout, ok | timeout, ok | EPERM | EPERM | ECONNREFUSED |
| TCP IPv4-mapped, allowed / wrong port | ok / ok | ok / EACCES | ok / EPERM | ok / EACCES | ok / ECONNREFUSED |
| TCP `[::1]` allowed / wrong port | ok / ECONNREFUSED | ok / EACCES | ok / EPERM | ok / EACCES | ok / ECONNREFUSED |
| TCP Fast Open (`sendto`), wrong port | ok | EACCES | EPERM | EACCES | ECONNREFUSED |
| UDP `sendto`, wrong port / address / mapped | ok | ok | EPERM | EPERM | EPERM |
| UDP `connect`, wrong port | ok | ok | EPERM | EPERM | ok (sends nothing) |
| connected UDP, `sendto` another port | ok | ok | EPERM | EPERM | EPERM |
| TCP bind outside the rules, and to port 0 | ok | EACCES | EPERM | EACCES | ok |
| UDP bind outside the rules | ok | ok | EPERM | EPERM | ok |
| listen, connect to itself, accept | ok | ok | ok | ok | ok |
| raw, packet, netlink sockets | EPERM (seccomp) | EPERM | EPERM | EPERM | EPERM |
| abstract Unix socket outside | EPERM (Landlock scope) | EPERM | EPERM | EPERM | EPERM |
| `getaddrinfo` of a DNS name (resolver at 127.0.0.53) | ok | ok | fails | fails | fails |
| a datagram from outside the rules to a bound port | received | received | received | received | received |

FreeBSD (VM):

| case | none | pf | ipfw |
| --- | --- | --- | --- |
| TCP and UDP to the allowed address and port | ok | ok | ok |
| TCP and UDP, port or address not in the rules | ok | EACCES | EACCES |
| guard: loopback under a broad rule | ok | (not loopback: the jail's own address) | EACCES |
| guard: link-local under a broad rule | timeout | EACCES | EACCES |
| IPv4-mapped (with `IPV6_V6ONLY` cleared), wrong port | ok | EPROTONOSUPPORT (the jail has no IPv6) | EACCES |
| UDP `connect`, wrong port | ok | ok (sends nothing) | ok (sends nothing) |
| connected UDP, `sendto` another address | EISCONN (FreeBSD refuses it) | EISCONN | EISCONN |
| TCP and UDP bind outside the rules, port 0 | ok | ok | ok |
| raw socket | EPERM (the jail) | EPERM | EPERM |
| routing socket (`PF_ROUTE`) | ok | ok | ok |
| `getaddrinfo` of a DNS name | ok | fails | fails |
| a datagram from outside the rules to a bound port | received | received | received |

### Cost (Pi, median of the two rounds)

| fence | rules | TCP connect and close, p50 | per second | UDP `sendto`, 64 B |
| --- | --- | --- | --- | --- |
| none | | 20.25 µs | 46,861 | 2,320 ns |
| bpf | 2 | 20.35 µs | 47,447 | 2,432 ns |
| bpf | 16 | 20.4 µs | 47,478 | 2,423 ns |
| bpf | 64 | 20.65 µs | 46,981 | 2,562 ns |
| bpf with Landlock ports | 2 | 20.4 µs | 47,526 | 2,433 ns |
| nft | 2 | 21.6 µs | 44,757 | 2,569 ns |
| nft | 16 | 22.95 µs | 42,255 | 3,230 ns |
| nft | 64 | 28.3 µs | 34,538 | 4,952 ns |
| made by the launcher, passed over `SCM_RIGHTS` | 2 | 38.25 µs | 25,161 | 3,332 ns (sent on by the launcher) |

- **Bulk TCP** ran at 2.8 to 3.7 GB/s in every row, with no order between fences.
- **MQTT** was the same under every fence:
  - QoS 0 at 143,000 to 145,000 messages a second;
  - QoS 1 at 68,000 to 71,000 acknowledged;
  - connect in 0.25 to 0.35 ms.

FreeBSD VM (function check; the host's other work moves these by several µs):
- **Connect and close:** 16 to 18 µs with no filter (16.3 to 16.4 µs on `lo1`, pf's interface), 22 to 25 µs under pf at 2 to 64 rules, and 17.5 to 21.5 µs under ipfw. One ipfw round of 8.7 µs is not explained.
- **UDP sends:** 0.9 to 1.7 µs in every case.
- **MQTT QoS 0:** 328,000 a second under pf, against 351,000 to 354,000 without a filter and under ipfw.

## Findings

- **cgroup BPF holds every outbound case** in the matrix, at once, with `EPERM`:
  - TCP, UDP `sendto` and `connect`, and a connected socket's `sendto` elsewhere;
  - TCP Fast Open (the `connect4` hook runs for it), IPv4-mapped addresses and IPv6;
  - the guard's three ranges;
  - `bind`, including port 0.

  At 64 rules it costs 0.4 µs a connect and 0.24 µs a datagram on the Pi, and nothing measurable on bulk transfer or MQTT. It is generated from the same rules the node checks, needs no toolchain, and attaching it to the app's cgroup covers every instance in it.
- **nftables is a weaker fence and dearer:**
  - it cannot see `bind` or a UDP `connect`, which send no packet;
  - a dropped SYN does not fail `connect`, since the kernel retransmits until the caller gives up, so TCP needs `reject with tcp reset`;
  - it runs on every packet, linear in the rules: at 64 rules, 8 µs more a connect and 2.1 times a datagram's cost;
  - its table is global state on the host;
  - replies from a listening app need the `bind` rules as source-port rules.
- **Landlock's port rules are a second fence for TCP only:**
  - they see ports, not addresses, and no UDP;
  - a rule without ports turns them off;
  - a range becomes one rule per port;
  - they refuse first (`EACCES`), so the BPF programs see only what Landlock allows.

  Where every rule lists its ports, they are free at this size (20.4 µs either way).
- **The seccomp domain filter and Landlock's scope remain necessary.** BPF's address hooks do not see raw, packet or netlink sockets, and leaving the network namespace makes the host's abstract Unix sockets reachable but for Landlock's scope (ABI 6, Linux 6.12 and later).
- **Gaps common to every fence:**
  - **Inbound UDP:** a datagram from any address reaches a bound UDP port; the node's sockets check each receive. A `cgroup_skb/ingress` program could drop datagrams from outside the rules; not built.
  - **Names:** the resolver is on loopback (127.0.0.53 on Ubuntu), which the guard keeps out of broad rules, so an own-sockets app needs a rule for its nameserver, or the resolver driver's answers.
  - **FreeBSD `bind`:** pf and ipfw cannot restrict it. `mac_portacl(4)` restricts binding by uid and port, and with a uid per app it might; not tested.
  - **FreeBSD sockets:** a jail opens routing sockets (read-only without privilege); FreeBSD has no seccomp to refuse a domain.
- **FreeBSD gives up capability mode** for an own-sockets app: capability mode refuses `connect` and `bind` to addresses. The jail (its root, no other processes, no raw sockets) and `rctl` remain.
  - **ipfw:** matches the jail's id, so instances need no address of their own; it held every outbound case including the guard and mapped addresses, and cost within the VM's noise. But loading ipfw puts a host under its filter, refusing everything unless `net.inet.ip.fw.default_to_accept` is set first, which is a site's decision.
  - **pf:** needs an address per jail, and IPv6 per jail if wanted. It fits sites already running pf (an anchor per app) and cost 6 to 9 µs a connect on the VM, against the same loopback interface without it.
- **Against node-made sockets:** an own connect cost 20.4 µs on the Pi against 38.3 µs for the launcher's stand-in, and a datagram 2.4 µs against 3.3 µs. The node's own path costs more than the stand-in. The difference matters for clients that connect often; for long-lived broker connections like MQTT's, which send over one socket, it does not.

## Decisions (recommended, for AP2)

- **Node-made sockets stay the default.** The node checks every address, inbound UDP included, and FreeBSD apps keep capability mode.
- **Own sockets become an opt-in profile,** narrower than `unconfined`, granted by policy in the same way: the app keeps the runner's Landlock, seccomp and minimal root.
  - **Linux:** the host's network namespace (not a new one), cgroup BPF programs generated from the manifest's rules on the app's cgroup by the helper, Landlock TCP ports where every rule lists ports, and the seccomp domain filter (in the runner's allow-list).
  - **FreeBSD:** a jail without capability mode and ipfw rules matching the jail, or pf where a site runs pf. Which to support is AP2's choice.
- **To decide in AP2:**
  - inbound UDP filtering (`cgroup_skb/ingress`), or a documented gap;
  - `bind` on FreeBSD (`mac_portacl`, or a gap);
  - the manifest's name for the profile;
  - DNS for own-sockets apps (a rule for the nameserver, or the resolver driver);
  - whether node config can forbid the profile outright, as `[native] unconfined` does.

## Not done

- The real node's path for node-made sockets (rings, the node's task) was not measured. The launcher's stand-in is cheaper than it.
- A `cgroup_skb/ingress` program for inbound UDP.
- `mac_portacl` on FreeBSD; a VNET jail (not chosen, 2026-11-22).
- Kafka (rdkafka) and AMQP clients; rumqttc stands for libraries that open their own sockets.
- Numbers on the 16-core machine, and FreeBSD numbers off a VM.

## Running

Build as a user, run as root. Linux (mosquitto from the distribution, on 127.0.0.1:1883):

```sh
cargo build --release
sudo sh scripts/setup-linux.sh
sudo sh scripts/run-linux.sh > results/raw/<machine>-linux-<date>.txt
sudo sh scripts/cleanup-linux.sh
```

FreeBSD (snapshot a VM first; the run loads pf and ipfw and leaves them loaded):

```sh
cargo build --release
sh scripts/setup-freebsd.sh       # as root: mosquitto, lo1 aliases
sh scripts/run-freebsd.sh > results/raw/freebsd-<date>.txt
```

One case at a time, for example the probe under BPF:

```sh
/opt/s40/s40 launch --fence bpf --rules /opt/s40/rules/probe-linux.toml --v6 \
    -- probe --allowed 127.0.0.1 --other 127.0.0.2 --self 127.0.0.1 --v6
```

`cargo test` runs the generators' tests (the BPF programs in the interpreter against the reference rules, the pf, ipfw and nftables texts) on any system.
