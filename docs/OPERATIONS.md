# Bulkhead: Operating a node

Where a node keeps its files, what it reads from the environment, what to back up, and how to run it as a service. Node configuration keys are in [MANIFEST.md](MANIFEST.md); the build features are in the [README](../README.md#building).

## Systems

A node runs on Linux, on x86-64 and aarch64, and on FreeBSD 15 on x86-64 (`freebsd-aarch64` is a bundle platform that CI compiles for but no machine runs, so it is untested). Apps run under `bulkhead-runner` on both: Landlock and a seccomp allow-list on Linux, Capsicum on FreeBSD, and under the privileged helper a user of their own, in a cgroup and namespaces or a jail. CI runs the whole test suite on Linux (x86-64 and aarch64) and on FreeBSD 15 (x86-64, in a virtual machine); the helper's root tests run on test machines of both systems. What differs on FreeBSD: `memory_max` counts an instance's whole address space, so it needs room for the 17 MiB or so an app reserves at start ([MANIFEST.md](MANIFEST.md)); rustbridge plugins are refused (an app is an executable); rc.d starts the node under the helper with [`examples/freebsd/bulkhead`](../examples/freebsd/bulkhead). The CLI also builds elsewhere, with no node.

## Paths

A node reads its configuration, keys and policy from wherever its config file names them (`/etc/bulkhead` by convention), and writes only under `data_dir` (`/var/lib/bulkhead` by convention). Logs go to stderr; there are no log files.

| Under `data_dir` | What | Back up |
| --- | --- | --- |
| `state.json` | Deployed apps: versions, running or stopped, deploy order | yes |
| `bundles/<app>/<version>.rbp` | The signed bundles of each app's current and previous version | yes |
| `kv/` | Keyvalue data: each app's buckets and the shared buckets, and on a pair node the replicated log with its `cluster` buckets (redb); on a DR node (`[dr]`) its copy of the pair's log and buckets | yes, from a stopped node or a filesystem snapshot |
| `audit.log` | Every admin change, one JSON record per line | as policy requires |
| `creation` | Raised on every start; part of every pid | never restore an older one (see below) |
| `failover-epoch` | A pair node's highest failover epoch | never restore an older one |
| `witness-state` | A witness's epoch, the node it last promoted, and `outdated` while that node's partner lacks replicated writes | never restore an older one |
| `priv/<app>/<version>/` | Each bundle's `priv/` files, extracted again on every start | no |
| `compiled/<app>/<version>/` | Each version's native program, written from its bundle; only the node can change it. Rebuilt from the bundle if missing. | no |

Modes (ROADMAP PS2): the node runs with umask 077, so what it makes is its own (0600 and 0700), and at each start it tightens a tree an older node left. `data_dir`, `compiled/` and `priv/` are 0711, searchable but not listable, so an app's user reaches its own directories by path; under the privileged helper `compiled/<app>` and `priv/<app>` are the node's with the app's group (directories 0750, files 0640, the program 0550), and everything else is the node's alone. Without the helper every app runs as the node's user and reads what the node reads.
| `tmp/` | Uploads being verified | no |
| `node.lock` | Held while the node runs; a second node on the same `data_dir` is refused | no |
| `admin.sock` | The admin API (mode 0600), unless `admin_socket` names another path | no |

Restoring a backup: keep the current `creation`, `failover-epoch` and `witness-state` files rather than the backup's. Each must never go back: `creation` keeps pids from an earlier run from being reused, and the epochs are failover's fencing tokens.

`cluster` buckets are kept on both nodes of a pair, so a node whose `kv/` is lost or older needs no restore: started while its partner runs as primary, it rejoins as the standby and the primary brings it up to date (from its log, or with a snapshot). Restoring an older `kv/` on a node that then leads (the only node left, or both nodes) loses every write since the backup.

Volumes (`[volumes]`) are the operator's directories, outside `data_dir`; back them up with the apps' data.

The privileged helper keeps its own state, apart from the node's, in its configuration's `state` (`/var/lib/bulkhead-priv` by convention, root's, mode 0700): `uids`, which user each app runs as, one `uid app` per line. Back it up with `data_dir`, and never restore it into a helper whose users are bound to other apps since: an app's files belong to its user.

## Environment

| Variable | Read by | Meaning |
| --- | --- | --- |
| `BULKHEAD_CONFIG` | `bulkhead node` | The node config file, instead of `--config` |
| `BULKHEAD_SOCKET` | every admin command | The admin socket, instead of `--socket` (default `/var/lib/bulkhead/admin.sock`) |
| `BULKHEAD_REMOTE`, `BULKHEAD_NODE_KEY`, `BULKHEAD_KEY` | every admin command | A node's remote admin API (`host:port`), its public key, and the operator's private key, instead of `--remote`, `--node-key` and `--key` |
| `BULKHEAD_CONTACTS` | `bulkhead shell` | The contacts file, instead of `--contacts` (default `contacts.toml` under `$XDG_CONFIG_HOME/bulkhead` or `~/.config/bulkhead`) |
| `XDG_CONFIG_HOME` | `bulkhead shell` | Where the contacts file and the shell's history (`bulkhead/history`) are |
| `BULKHEAD_KEY_PASSWORD` | `bulkhead bundle`, `bulkhead keygen` | The signing key's password, instead of a prompt |
| `SSL_CERT_FILE`, `SSL_CERT_DIR` | `bulkhead node` | Where the node reads the system's root certificates for HTTP egress, instead of the OS defaults (`[egress] system_roots`) |
| `RUST_LOG` | `bulkhead node` | Log filter (default `info`), for example `info,bulkhead::lease=debug` |
| `BULKHEAD_PRIV_SECCOMP_LOG` | `bulkhead-priv` (Linux) | Set: calls off the helper's seccomp list are logged by the kernel and allowed, not refused. For a test machine, never production ("The privileged helper") |
| `BULKHEAD_PRIV_FD` | `bulkhead node`, set by `bulkhead-priv` | The descriptor of the privileged helper's socket. Node config's `[priv]` and this variable go together: the node refuses to start with one and not the other |
| `BULKHEAD_PRIV_TESTS`, `BULKHEAD_CONFORMANCE_HELPER` | `bulkhead-host`'s tests, as root on a test machine | `1` runs the helper's own tests (`tests/priv.rs`), and the conformance suite's native mode under the helper |
| `BULKHEAD_PRIV`, `BULKHEAD_PREOPENS`, `BULKHEAD_SPIN_US`, `BULKHEAD_SENDER_SPIN_US` | a native-process app, set by the node | The directory of the bundle's `priv/` files; where each preopened directory is, one guest path and host directory per line, separated by a tab (with `\\`, `\t` and `\n` escaped), for `bulkhead_sdk::files`; and how long the app spins waiting for the node before it sleeps, on its instance's channel (`[native] spin_us`) and on a sender channel (`[native] sender_spin_us`). A native app's environment holds nothing else, but for a driver app's `BULKHEAD_LISTENERS` and, on FreeBSD, `BULKHEAD_PREOPEN_FDS` and `LD_LIBRARY_PATH_FDS` |
| `BULKHEAD_PREOPEN_FDS` | a native-process app on FreeBSD, set by the runner | The descriptor of each preopen in `BULKHEAD_PREOPENS`' order, joined by commas, which `bulkhead_sdk::files` opens under (capability mode refuses paths) |
| `LD_LIBRARY_PATH_FDS` | a native-process app on FreeBSD, set by the runner | The library directories' descriptors, for the run-time linker (rtld(1)) |
| `BULKHEAD_LISTENERS` | a driver app, set by the node | Its listening sockets (`[drivers.<app>] listeners`), `name=fd` pairs joined by commas (`https=8,http=9`), for `bulkhead_native::driver::listeners` |

Secrets may also come from variables the node config names (`[secrets.<name>] env = ...`).

## The remote admin API

`[admin] address` serves the admin API over TCP with TLS 1.3 to operators on other machines (cargo feature `tls`, on by default), with the same routes and rights as the admin socket. Since ROADMAP DA7 the node terminates no TLS for it: it binds the address and starts `bulkhead-admin`, which accepts operators and carries each connection to the node, and the node checks every request's signature (DESIGN.md, "Remote administration outside the node"; SECURITY.md, "The admin socket and the remote admin API"). Keys are pinned both ways, with no certificate authority:

- **Install** `bulkhead-admin` beside `bulkhead-runner` (`cargo build --release -p bulkhead-admin`), or name it in `[native] admin`; a node with `[admin]` and no such file refuses to start. It runs as an app named `bulkhead-admin` (a reserved name) that `bulkhead ps` lists, restarted if it exits; connections it held are lost, the port stays open.
- **The admin key is its own:** make it with `bulkhead cluster keygen <path>` and set `[admin] key`. It must not be `[cluster] key` (by path or key): the driver holds it, and the cluster key stays in the node. A node that used the cluster key, the default before 2026-11-14, refuses to start until it has one. `bulkhead cluster pubkey <key file>` prints the public key operators pin with `--node-key` (or `BULKHEAD_NODE_KEY`); changing the key means giving operators the new one.
- Each operator makes a key with `bulkhead cluster keygen <path>`, and the node's operators file lists the public keys it accepts, by name: `[operators] alice = ["ed25519:..."]`. The node and the driver each read it again on SIGHUP and `bulkhead reload-tls`; removing a key stops new connections, not open ones.
- `bulkhead --remote <host:port> --node-key ed25519:... --key <operator key> <command>` runs any command against the node; the CLI signs each request with the operator's key, so the operator's clock must be within 60 s of the node's (a refused request says how far off it was). The shell keeps these in a contacts file (below). A CLI from before 2026-11-14 does not sign and is refused (401).
- **Under the privileged helper** put the admin key and the operators file in a directory of their own that the node's user owns and the helper's `binds` lists (`/var/lib/bulkhead-admin`, mode 0700): the node shares each file with the driver's group, read-only, and gives the directory search for others (`o+x`). `[native] runner` names the helper's runner, beside which the node finds the driver.

Every request but a `GET` is written to the audit log with the operator its signature proves, and every refused request as `remote_admin_refused`. Every listed operator has every right; put the address on a management network.

## The shell

`bulkhead shell` runs `bulkhead` commands at a prompt against one node: the admin socket, or the node `--remote` names. `connect <name>` (or a node id, or `local` for the socket) moves the session to a node in the operator's contacts file, reached over its remote admin API (`[admin]`) with the key pinned there:

```toml
# ~/.config/bulkhead/contacts.toml
key = "operator.key"        # the operator's private key, relative to this file; --key overrides it

[[node]]
name = "alpha"
id = 1                      # optional: `connect 1`, and the CONTACT column of `members`
address = "10.0.0.1:7443"   # the node's [admin] address
node_key = "ed25519:..."    # the node's public key
```

Ctrl-C stops the command running (a follow, a slow call); Ctrl-D or `exit` leaves. History is kept in `~/.config/bulkhead/history`.

## Signals

- **SIGTERM or Ctrl-C** stops every app, in reverse start order, and leaves the cluster.
- **SIGHUP** reloads the HTTPS certificates, the cluster key and peers file, the remote admin operators file, and `resolv.conf` and the hosts file for the resolver (as `bulkhead reload-tls` does). The certificates, the cluster identity and the resolver's files also reload by themselves when they change (`tls_reload_s`, `identity_reload_s`, `[resolver] reload_s`).

## Running as a service

[`examples/systemd/bulkhead.service`](../examples/systemd/bulkhead.service) runs a node as its own unprivileged user. Privileges:

- The node sets `no_new_privs` on itself at start and marks itself not dumpable (on FreeBSD, both through `procctl`: `PROC_NO_NEW_PRIVS_CTL` and `PROC_TRACE_CTL`): it leaves no core dump, and other processes of its user cannot trace it or read its memory or environment through `/proc`. Node config's `debuggable = true` leaves it dumpable, for a debugger or a core on a development machine.
- No root. `CAP_NET_BIND_SERVICE` is the only capability, and only for ingress or listener ports below 1024; leave it out otherwise. Under the privileged helper the node holds none: the helper binds its low TCP ports ("The privileged helper").
- Each app under a user of its own takes the privileged helper in place of this unit (below).
- `dialout` (or the device's group) for the failover serial line (`[failover] serial`), with `PrivateDevices` off. A device that cannot be opened is logged and retried every second; the path stays silent meanwhile, and `bulkhead failover` shows it (`paths: link heard, serial silent`).
- `StateDirectory` creates `data_dir` owned by the service user, mode 0750; the node creates what is under it.
- Every member node also needs `bulkhead-resolve`, its resolver of host names ("Host names", below), and a node with `[datasources]` needs `bulkhead-sql` ("SQL datasources", below).
- A node with `[metrics]` needs `bulkhead-metrics` beside the runner (`cargo build --release -p bulkhead-metrics`, or `[native] metrics`): it serves `GET /metrics` as an app of its own, `bulkhead-metrics`, asking the node for the text on each scrape (ROADMAP DA7). A witness with `[metrics]` needs the runner and it too. A node with `[admin]` needs `bulkhead-admin` ("The remote admin API", above).
- Native-process apps need `bulkhead-runner` beside `bulkhead` (or `[native] runner`), built with `cargo build --release -p bulkhead-runner`. A node with `[http]` also needs `bulkhead-ingress` (below). For their memory and process limits, give the node a cgroup: `Delegate=yes` in the unit, `ProtectControlGroups` removed, and `[native] cgroup` set to the unit's cgroup (`/sys/fs/cgroup/system.slice/bulkhead.service`); the node must be the only process in it, and moves itself into its `node` child.
- Native-process apps and the node spin for `[native] spin_us` (default 20 µs) waiting for each other before they sleep, and an app's thread waiting for an answer on a sender channel for `[native] sender_spin_us` (default 50 µs), which took one client through a driver app about 15% off each request on the Pi (BASELINE.md, 2026-11-13): calls and requests within a node cost a fifth to two fifths of what blocking waits cost, for 1.5 to 1.9 times the CPU per call while many instances are busy. On a small machine with many busy native instances (a Raspberry Pi 5 with 100), that CPU cost half the throughput until 2026-11-05; since T1b neither side spins after a callback with nothing queued behind it that came longer than the spin after the one before, which gave the Pi about 1.8 times the calls in flight and the 16-core machine a third less CPU per call, with sequential calls unchanged (BASELINE.md, 2026-11-05, "spinning only where a callback follows soon"). `spin_us = 0` is no longer needed for that; it still trades requests of 3 to 4 µs for 13 to 28. The spinning stops when the traffic does. A published bucket (`[buckets.<name>] publish_ms`) is rebuilt whole after each change, by one thread per node: for data written far less often than it is read.
- Descriptors: each native instance holds about five of the node's (its channel's bell and door, its descriptor socket, its log pipe), and each sender channel its threads open two more (its bell and door; `limits.senders_max`, 8 by default). 100 instances with 8 senders each need about 2,100, past the usual soft limit of 1,024: the unit sets `LimitNOFILE=65536`, and a node started by hand needs `ulimit -n` raised the same way. Past the limit a sender is refused (`Other("... Too many open files")`) and an instance fails to start; the node runs on.
- On aarch64 CPUs with LSE atomics (ARMv8.1 and later: Raspberry Pi 5, AWS Graviton 2 and later, Apple silicon), building with `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-feature=+lse"` makes messages between instances 3 to 9% cheaper (BASELINE.md, 2026-10-29). Such a binary does not run on ARMv8.0 CPUs (Raspberry Pi 4).

Reaching the primary of a pair from outside needs no root. Either put a load balancer in front of both nodes, with a health check on the app's own route expecting 200, or set `partner_url` on each node's `[http.<name>]` listener to the other node's address for it, as clients reach it: the standby then answers a singleton's requests with `307 Temporary Redirect` to the primary, and clients that follow redirects can use either address (tutorial chapter 2). Or set `standby = "forward"` on the listener: the standby passes a singleton's requests over the cluster to the primary's app and answers with its reply, so either address works for any client. For one address and no load balancer, a separate agent can move a virtual IP (below).

### Planned switchover

`bulkhead failover switchover` on the primary hands its singletons to the standby, for maintenance of the primary or to swap roles on purpose. The primary stops its singletons gracefully (each runs `terminate`; one in `snapshot` upgrade mode, on a pair with `[failover] replication = true`, instead hands its state over and starts on the standby from it, logged as `singleton stopped: planned switchover, state moved` and `singleton moved in`), then steps down and releases its lease at the witness; the standby runs them within a few heartbeats (about a lease and its margins, 3.5 to 4 s with the defaults, if the release does not reach the witness, or the witness runs the previous release; no node timeout either way), and the old primary rejoins as its standby. It is refused, and nothing stops, when the node is not the primary, its arbiter is not a witness, it does not hear its partner as a standby (a moment after the partner becomes one, try again), or the partner is still catching up on replicated writes (the witness has it recorded out of date). Recorded in the audit log as `failover_switchover`.

### Confirming a promotion

A standby asks the operator to confirm only while it cannot hear its partner on any path and cannot reach the witness either (with arbiter `"operator"`, whenever the partner looks gone); `bulkhead failover` then shows `waiting:`. `bulkhead failover confirm` promotes it once the partner's lease has run out, and the confirmation is final. Confirm only when the old primary is down and stays down, powered off or fenced, until the two nodes hear each other again: the witness never learns of the confirmation, so an old primary that restarts and reaches the witness resumes, and both run the singleton until they hear each other (docs/tla/VERIFICATION.md, finding 6).

### Upgrading the builds of a pair

A node speaks its own wire protocol version and the one before, so the witness and the two nodes can be upgraded one at a time, each to the next release: the witness first, then the standby, a planned switchover, then the other node. `bulkhead release` shows the versions a node speaks, the version each connected member agreed on, and the version of the formats the pair keeps. Until the pair is finalized, either node can be rolled back to the build before. With every node on the new build, `bulkhead release finalize` on either node of a pair with `[failover] replication = true` raises the pair's version to the build's; it is refused unless the partner and the witness, and a DR node (`[failover] dr`), are connected at that version. Upgrade a DR node with the witness, before the pair. From then on a node refuses to start a build older than the pair's version, naming both versions; one rolled back while it was cut off from the raise learns of it from its partner, logs `the pair was finalized at a version newer than this build`, and shuts down with the same message. Recorded in the audit log as `release_finalized`. Builds before 2026-10-21 do not check, so a rollback to one of those is not stopped. The keyvalue file (`kv/`) needs nothing either way: builds from 2026-10-25 on use redb 4 and earlier ones redb 3, and each opens and writes the other's files (checked with Bulkhead's tables in both directions, compaction included, 2026-10-25).

Native apps are held to the same window: a node accepts apps built against its native protocol version (`bulkhead_native::protocol::VERSION`) and the one before, so the apps deployed keep running across one node upgrade. A release whose node changes that version twice since an app was built refuses the app at its start (`crashed: native protocol version N; this node accepts ...`); rebuild and redeploy apps before then. The CHANGELOG says when a release changes the version.

### Releasing an app across a pair

`bulkhead release upgrade <app> <bundle> --pair pair.toml` upgrades an app on both nodes: the standby, then the primary in place, waiting after each for the app to run at the new version there (`--timeout-s`, 60 by default). The pair file lists each node as `[[node]]` with its `id` and either `socket` (its admin socket) or `address` and `key` (its remote admin API and public key, with `--key` for the operator's private key). Each run reads both nodes first, so after a stop (a node out of reach, a change of roles) running the same command continues; a refused step or a failed gate rolls back what the run changed (`--no-abort` leaves it). `--standby-only` upgrades the standby alone and leaves the primary for a later run without it, to look at the new version before the primary moves (`examples/scada/drill.sh` uses it). `bulkhead release rollback <app> --pair pair.toml` returns both nodes to their common previous version. The rollout settings of `bulkhead upgrade` (`--approve`, `--batch`, `--observe-ms`, `--max-restarts`, `--restart`, and for a blue-green app `--steps` and `--drain-ms`) apply on each node. With `--json` the report lists each step and the outcome (`done`, `stopped`, `aborted`); the command exits non-zero unless it is `done`.

### A virtual IP that follows the primary

`bulkhead failover --watch` prints the node's standing once, then a line each time its leadership changes: role, epoch, and whether singletons may run here. With `--json`, each line is one object, such as `{"awaiting_confirmation":false,"epoch":3,"may_run":true,"role":"primary"}`. When the node cannot be reached, it prints `{"may_run":false,"role":"unreachable"}` once and tries again every second. It reads `GET /v1/failover?after=<generation>&wait_ms=<ms>`, a long poll that answers when the leadership has changed since that generation (`generation` in `bulkhead failover --json`), or after the wait (at most 60 s).

An agent beside each node of the pair holds the address while its node may run singletons. [`examples/systemd/bulkhead-vip.sh`](../examples/systemd/bulkhead-vip.sh) does this with `ip addr` and a gratuitous `arping`, and [`bulkhead-vip.service`](../examples/systemd/bulkhead-vip.service) runs it as the `bulkhead` user with `CAP_NET_ADMIN` and `CAP_NET_RAW` only. `bulkhead node` keeps neither. A cloud provider's API for moving an address fits the same loop. Neither the script nor the unit is tested in CI, since both need those capabilities.

- **When the address moves.** It follows `may_run`, so it moves when the singletons do: the old primary releases it when its lease ends, and the new primary takes it when it is promoted, about the time of a takeover (3 to 4 s with the tutorial's timings). Two nodes never hold it while both may run singletons, since the lease rules out two primaries; for a moment they can both hold it while one is releasing, and neighbours follow the last gratuitous ARP.
- **A lost node.** The watch prints `unreachable` when the agent cannot reach its own node, so the agent releases the address. A node that is gone with its agent (a power cut) releases nothing, and the address moves when the other node takes it.
- **keepalived instead.** A site that already runs keepalived can have it follow Bulkhead's role. A `vrrp_script` that runs `bulkhead --json failover` and succeeds only when `"may_run": true` appears, with a weight above the gap between the two nodes' priorities, makes the Bulkhead primary the VRRP master. VRRP then runs its own election beside Bulkhead's, so the address can lag a takeover by keepalived's own interval, and during a network cut each side's keepalived decides on its own. The watch agent above does not have these gaps.

### A serial line between two VMs

Two nodes of a pair in virtual machines on one host can have the serial heartbeat path as well, through a second serial port in each guest that the host joins. With libvirt (`virsh edit <domain>`, as a member of the `libvirt` group; the VM's first port stays its console):

```xml
<serial type='tcp'>
  <source mode='bind' host='127.0.0.1' service='47111'/>   <!-- 47112 on the other VM -->
  <protocol type='raw'/>
  <target port='1'/>
</serial>
```

In the guest the port is `/dev/ttyS1` on Linux (group `dialout`) and `/dev/cuau1` on FreeBSD (group `dialer`); the node's user joins the group, and `[failover] serial = { device = "/dev/ttyS1" }` (or `/dev/cuau1`) names it. The host joins the two ports:

```sh
mkfifo /tmp/serial-line
nc 127.0.0.1 47111 < /tmp/serial-line | nc 127.0.0.1 47112 > /tmp/serial-line
```

Stopping the relay cuts the line and starting it again joins it; each node shows the path in `bulkhead failover` (`paths: link heard, serial heard`). For a line that stays up with no process on the host, give the two ports `type='udp'` instead, each with `<source mode='bind' host='127.0.0.1' service='<its port>'/>` and `<source mode='connect' host='127.0.0.1' service='<the other's port>'/>`; it cannot then be cut on its own (not tried here). [`scripts/vm-pair.sh`](../scripts/vm-pair.sh) runs a pair across a Linux and a FreeBSD VM this way, with the witness on the host, and cuts the network, the line and a node in turn (ROADMAP FB5, 2026-11-17).

### A disaster-recovery copy

A third node at a second site can keep a copy of the pair's replicated log and buckets, about a poll interval behind while the WAN is up (under a second in the simulation, with 50 ms of delay and 5% loss each way; not measured over a real WAN). ROADMAP Phase 8: DR2 ships it, DR4 activates it (below). It is the witness or a node of its own, without `[failover]`:

```toml
# On the DR node (the witness here):
[dr]
pair = [1, 2]
rpo_target_s = 300      # warn while the copy trails by longer
poll_ms = 1000          # between rounds once caught up
batch_bytes = "1MiB"    # entries asked for at once (at most 4 MiB)
# log_max = "64MiB", log_age_s = 3600: its log's bounds, as the pair's

# On both pair nodes, beside replication:
[failover]
dr = 3
```

The DR node pulls committed entries from either pair node over the cluster transport, so the pair nodes need it among their peers and it needs them. A node of its own puts the pair's addresses in `seeds`. `bulkhead dr` on the DR node shows the node it pulls from, its commit index and its lag; `bulkhead failover` on a pair node adds a `dr:` line with the same lag as that node sees it. Both report `bulkhead_dr_lag_seconds` (OBSERVABILITY.md), and each logs a warning once the lag passes `rpo_target_s`. The lag grows while the WAN is down whether or not anything is written: it is the time since the copy last held everything committed, not a count of writes. Node config, the cluster key, the peers file and policy are not shipped: keep them at site 2. A node at cluster protocol version 9 neither answers nor sends the frames, so a pair and its DR node ship once all three run this build; `bulkhead release finalize` waits for the DR node too ("Upgrading the builds of a pair"). A DR copy that applies a pair's version newer than its build stops pulling, as a standby stops replicating.

A DR node's resident memory grows with each activation and failback by memory glibc keeps for reuse rather than returning: from about 20 MiB to 72 MiB over an hour of DR5's drill, with under 10 MB in use (ROADMAP DR3b, 2026-11-20). `MALLOC_ARENA_MAX=2`, `MALLOC_MMAP_THRESHOLD_=131072` and `MALLOC_TRIM_THRESHOLD_=131072` in the node's environment (a systemd unit's `Environment=`) kept the drill's DR node near its starting size; their cost to throughput was not measured.

The DR node holds the primary's deployments too (ROADMAP DR3): every ten polls (10 s with the default `poll_ms`) it asks both pair nodes for their deploy state, follows the primary's, fetches the bundles it lacks a 256 KiB chunk at a time by SHA-256, and takes the primary's apps, versions and blue-green sides as its own, deployed and not started (`bulkhead list` there shows them as `dr copy`); they start when the node is activated (DR4). An app removed or upgraded on the primary is removed or upgraded there. Each held app is built as a deploy would build it, against the DR node's own trusted keys, policy, secrets, datasources, buckets and volumes, so keep those at site 2 as on the pair, the pair's `cluster` buckets included (`[buckets.<name>]` with `scope = "cluster"`, allowed on a DR node for this). An app that would not start there is reported, with why, as is one whose grant from site 2's policy differs from the primary's: `bulkhead dr` lists them (`not ready:`, `grants:`), `bulkhead_dr_apps_not_ready` counts them and each is logged as a warning. A DR node takes no deploys of its own: deploy, upgrade, rollback, traffic, start, stop and remove are refused there.

**Activating it** (ROADMAP DR4) is for when site 1 is lost, on a DR node that is also the pair's witness, or on a DR node apart from the witness that names it (below, "A DR node apart from the witness"). First make sure both site-1 nodes are down and stay down: powered off, or cut off from everything they serve and from each other. That judgement is yours. A site-1 pair that still hears itself keeps renewing its lease through its own heartbeats and goes on running its singletons and taking writes beside site 2, whatever the witness says (spike S38). Then:

```sh
bulkhead dr                         # how far the copy trails: about the writes you will lose
bulkhead dr activate --site-1-down
```

- **The held apps.** Activation is refused while this node does not hold the primary's deployments: none yet, still fetching them (`bulkhead dr` shows `fetching:`), or the latest refused. Wait, or add `--allow-missing-apps` to run what it holds; the answer then says what is missing. Held apps that would not start here are named in the answer either way.

- **The witness is fenced first.** It renews and grants nothing, then waits out the pair's lease as it last saw it renewed (`lease_ms` plus `margin_ms`, a few seconds), counted from its own start if it restarted since.
- **A site-1 node heard meanwhile abandons the activation** ("site 1 is not down"), and the pair is answered as before. Fence that node and ask again.
- **The new epoch.** The witness makes this node primary at a new epoch, kept on disk. This node then leads the replicated buckets from its copy: it takes writes, committed at once since no other copy waits, and serves linearizable reads. It also starts the pair's apps it holds, singletons included.
- **What is lost:** writes site 1 committed after the copy's last entry.
- **`bulkhead dr`** shows `active: epoch N`. The audit records are `dr_activate_asked` and `dr_activate`.
- **While active,** deploys on this node are its own. The pair's deployments are no longer followed.
- **Survives a restart:** the activation is in the witness's kept state, and the apps start again.
- **A site-1 node that comes back** is fenced by the witness, which tells it the DR node is active. It stays `down`, runs no singletons, and reseeds (below).

**Failing back**, once site 1 is repaired:

1. Start both site-1 nodes as they were configured. Each asks the witness, is told the DR node is active, and marks its replicated copy stale on disk. It then reseeds: it replaces its replicated buckets and log with a full snapshot of the DR node's, and pulls the DR node's entries after it. Whatever it committed after the DR node's point is dropped, its commit index included.
   - While stale it is `down`. It runs no singletons, and its `cluster` buckets refuse reads and writes (`unavailable`).
   - The mark outlasts a restart, which reseeds again from the start.
   - `bulkhead failover` on it shows `stale:`, then `caught up` once it holds everything the DR node had committed.
   - `bulkhead dr` on the DR node shows how far each has come (`site 1:`).
2. On the DR node, run `bulkhead dr failback --to 1` (the node to make primary; `--wait-s`, 60 by default).
   - It stops the apps there, so nothing more is written, and takes no more writes.
   - It waits until both site-1 nodes hold its last entry.
   - The witness then makes `--to` primary at a new epoch, kept on disk. That node resumes when it next asks, a lease or so later, and the other rejoins it as standby.
   - The DR node holds the pair's apps again and pulls its copy from the pair. Audit records: `dr_failback_asked` and `dr_failback`.
3. If a site-1 node has not caught up in time, the failback is refused, naming the node and how far it is. The DR node's apps start again and it takes writes as before; ask again later.

**A DR node apart from the witness** (ROADMAP DR4b). The witness and the DR node can be two machines: the witness at site 2 beside the DR node, or at a third site. The witness stays the one authority. The DR node asks it to fence and promote it, and to hand back, and leads only while the witness says it promoted it:

```toml
# On the witness:
[witness]
pair = [1, 2]
first = 1
dr = 4                  # the DR node, the only node whose asks it takes

# On the DR node (node 4, no [failover]):
[dr]
pair = [1, 2]
witness = 3

# On both pair nodes:
[failover]
dr = 4
```

- **Activation and failback are the same commands, run on the DR node.** It asks the witness every poll (`poll_ms`) until the witness's state shows it done. The witness fences and waits out the pair's lease as it does for itself, then promotes the DR node on its next ask.
- **The witness must be reachable.** Without its word in 25 s, `dr activate` answers that it is not confirmed and nothing runs at site 2. The DR node goes on following the witness: an activation the witness had already decided still takes effect at the DR node's next answer. There is no way to activate without the witness. Do not put the witness at site 1: lost with site 1, it leaves nothing to activate.
- **A failback without the witness's word stays frozen.** If the witness has not confirmed the hand-back in 25 s, `dr failback` says so, and the DR node keeps its apps stopped and takes no writes. It goes on asking, since the witness may have handed back with its answer lost. It never starts again on a timeout. `bulkhead dr` shows `active: epoch N` until the witness's answer arrives.
- **A restarted DR node leads again only once the witness says it promoted it.** It keeps nothing of its own about that, and starts the held apps then. The audit record of a hand-back left unconfirmed is `dr_failback_unconfirmed`.

Both site-1 nodes must reseed: a node that never did would hold writes the DR node does not, and could not join the pair. A failback carries the data, not the deployments. Site 1's nodes run the deployments they had; a deploy made on the active DR node is not shipped back, and the DR node takes the pair's deployments again once it follows the primary.

## Misbehaving apps and quarantine

The node holds each native instance to a misbehaviour policy (ROADMAP PS5). It counts the refusals it answers the instance, `access-denied` (something not granted) and messaging-ACL refusals together; past node config's `[limits] refusals_max` (100) within `refusals_period_ms` (10 s), or a lower `limits.refusals_max` in the manifest, it kills the instance, `crashed: policy(refusals)`. With `requests_per_sec` set (node config or manifest; none by default), more requests a second than that kill it too, `crashed: policy(rate)`. The supervisor restarts it as after any crash; `bulkhead_refusals_total` and `bulkhead_policy_kills_total` count them, and the log says which limit.

Each such kill, and each protocol break (`crashed: protocol: ...`, a frame the node refused), is an audit record, `misbehaved`. At `quarantine_after` (3) of them for one app within `quarantine_period_ms` (10 minutes), the node stops the app and quarantines it: `bulkhead list` and `status` show `quarantined` with the reason, the record `quarantine` is written, and the app stays stopped across node restarts (it is kept in `state.json`). Find out why from the log and `audit.log` (an app probing what it was not granted is broken or hostile), deploy a fixed version, or `bulkhead start APP` to run it again, which clears the quarantine and records it in the `start` record. `quarantine_after = 0` turns quarantine off; ordinary crashes are never counted, only the supervisor's restart intensity limits them.

## The HTTP ingress

Since ROADMAP DA2 (2026-11-14) the node parses no HTTP itself: for `[http.<name>]` it binds the addresses and starts `bulkhead-ingress`, which accepts the connections, terminates TLS and calls the app owning each route (DESIGN.md, "HTTP ingress"; SECURITY.md, "The ingress driver").

- **Install** `bulkhead-ingress` beside `bulkhead-runner`, built with `cargo build --release -p bulkhead-ingress`, or name it in `[native] ingress`. A node with `[http]` and no such file refuses to start. Under `[priv]` keep it in the runner's directory, which every instance's root shows. It belongs to the node's install, as the runner does: replace it with the node's binaries; it is not deployed, signed or upgraded as an app.
- **It runs as an app** named `bulkhead-ingress`, and its log lines carry that name; `bulkhead list` shows it, and the node's other drivers, after the deployed apps with `built-in` for a version, and `bulkhead status bulkhead-ingress` its instance and restarts (`GET /v1/apps?built_in=true`; without the parameter the API lists deployed apps only). `bulkhead status <app>` lists an app's HTTP routes the ingress serves (`route:`, `*` for any host). The name is reserved: a bundle or `[drivers.<app>]` of that name is refused. `[http]` takes at most 6 listeners.
- **Restarts.** If it exits, the node starts it again after 0.1 to 1 s and keeps doing so; the listening sockets are the node's, so the ports stay open and clients connecting meanwhile wait in the backlog. Connections it held, and requests in flight on them, are lost. It is stopped before the apps when the node stops.
- **Certificate files.** The node gives the driver each `tls` entry's `cert` and `key` read-only, under `/driver/` by file name, so two entries' files need distinct file names. The driver reloads them when their modification times change (every `tls_reload_s`, default 60), on SIGHUP and on `bulkhead reload-tls`, which reports each listener whose files failed (it keeps the old ones). A certificate replaced by rename works too, as certbot renews (new files in `archive/`, the links under `live/` switched): point `cert` and `key` at `live/<name>/fullchain.pem` and `privkey.pem`. The driver's access is bound to the files it was given at start (a Landlock rule on Linux, a descriptor on FreeBSD), so the node keeps each file's identity (device and inode, links followed) and, when one changed, restarts the driver on the new files, at the same moments (ROADMAP DA2b). The node keeps the listening sockets, so connections made meanwhile wait; connections in flight are dropped, about once per renewal. Its log says so, `bulkhead reload-tls` answers that it restarted, and the audit record is `ingress_restarted`. A file rewritten in place is reloaded without a restart.
- **Client certificates.** `client_ca` names a PEM file of the CAs whose client certificates a listener accepts. With `client_auth = "required"` (the default) a client without one, or with one those CAs did not sign, fails the TLS handshake; with `"optional"` it is served, with no certificate in its requests. The app gets a verified certificate's SHA-256 and DER (`client_cert`, GUEST_FORMATS.md). The CA file is given to the driver as the certificates are, under `/driver/` by its file name, and reloads with them, by rename too. Write the key before the certificate, or reload once both are written: a pair that does not match is refused and the old pair kept.
- **Under the privileged helper** node config must name the helper's runner in `[native] runner`, beside which the node finds `bulkhead-ingress`, and the helper's `binds` must hold each certificate file's directory (`/etc/bulkhead/tls`, say), which is bound into the driver's root read-only. The driver runs as a user of its own, so the node asks the helper to share each certificate and key file with it at start and before each `bulkhead reload-tls`: the file must be a regular file the node's user owns, with one link; it gets the driver's group and group read, and its directory, if the node's user owns it, search for others (`o+x`, no listing). Keep the key files in a directory of their own owned by the node's user, mode 0700 before the first start. A file the node's user does not own (one root owns, or certbot's archive) is not shared, and the driver cannot read it.
- **Metrics** keep their names (`bulkhead_http_requests_total` and the rest, OBSERVABILITY.md), with `app="bulkhead-ingress"`, and reach `bulkhead metrics` once a second.
- **Cost.** On a Raspberry Pi 5 the driver serves about 70% of the requests the node's own ingress did at 32 connections, for 1.5 to 1.6 times the CPU per request, and one connection's latency is 25 to 41% higher; on 16 cores it serves more than the node did, for about the same CPU (BASELINE.md, 2026-11-14). Large bodies cost about twice the CPU on both.

## Host names

Since ROADMAP DA9 (2026-11-14) the node resolves no host name itself: an app's `connect_host`, HTTP egress and datasources ask `bulkhead-resolve`, a driver every member node starts (a witness resolves nothing), which parses the DNS replies in a process of its own (DESIGN.md, "Host names"; SECURITY.md, "The resolver driver"). Addresses written as text never reach it.

- **Install** `bulkhead-resolve` beside `bulkhead-runner`, built with `cargo build --release -p bulkhead-resolve`, or name it in `[native] resolve`; a member without it refuses to start. It belongs to the node's install, as the runner and `bulkhead-ingress` do, and runs as an app named `bulkhead-resolve` (a reserved name) that `bulkhead ps` lists.
- **Where names come from.** The node reads `/etc/resolv.conf` and `/etc/hosts` (`[resolver] resolv_conf`, `hosts`) and gives the driver their text: `resolv.conf`'s `search`, `domain` and `options` (`ndots`, `timeout`, `attempts`) apply; its `nameserver` lines name the servers, unless `[resolver] nameservers` does (`ip` or `ip:port`, at most 8, which `resolv.conf` cannot express). A name in the hosts file is answered from it alone, as libc's `files` source answers. The driver asks the nameservers over TCP only, one connection to each for all its lookups: a nameserver that does not serve DNS over TCP (RFC 7766 requires it) answers nothing. Sources only NSS reaches are lost: mDNS (`.local`), LDAP, `myhostname`; a local stub that serves them over DNS (systemd-resolved's 127.0.0.53) keeps them. A link-local nameserver with a scope (`fe80::1%eth0`) is skipped.
- **Changes.** Both files are read again every `[resolver] reload_s` (5 s), on SIGHUP and on `bulkhead reload-tls`, by path, so a file replaced by rename (DHCP clients, NetworkManager, resolvconf) is seen. When either changed, the driver restarts with it: its connect rules name the nameservers. Lookups in flight are asked again of the new driver.
- **Its rights.** TCP to the nameservers' addresses and ports, and nothing else (`bulkhead:sockets` connect rules in its built-in manifest, apart from node config's `[sockets]` ceiling, which concerns deployed apps); messages from itself alone; 64 MiB. Under the privileged helper it runs as a user of its own, in the helper's runner directory.
- **Failures.** A lookup that finds no such name fails with `name-unresolvable`, one no nameserver answered in time with `temporary-resolver-failure` (an app's `connect_host`), a datasource's or egress's with the name and the reason. While the driver restarts (it never stops restarting, 0.1 to 1 s apart), a lookup is asked again until its timeout. A datasource's name is resolved for each new connection its driver asks for.
- **Metrics:** `bulkhead_resolve_lookups_total`, `bulkhead_resolve_lookups_failed_total` and `bulkhead_resolve_streams_total` (`app="bulkhead-resolve"`).
- **Cost.** A lookup through the driver takes 26 to 71 µs when the hosts file or its cache answers, about the time `getaddrinfo` takes from `/etc/hosts` and a fifth of its time through systemd-resolved; the driver's hop costs 20 to 50 µs and 90 to 190 µs of CPU (BASELINE.md, 2026-11-14). An app that resolves per request should keep its addresses.

## SQL datasources

Since ROADMAP DA3 (2026-11-15) the node runs no Postgres client: for each `[datasources.<name>]` it starts `bulkhead-sql` as the app `bulkhead-sql-<name>`, which holds that datasource's pool and runs the statements the node forwards, over connections the node opens for it (DESIGN.md, "SQL datasources"; SECURITY.md, "SQL datasources"). Apps and their manifests are unchanged.

- **Install** `bulkhead-sql` beside `bulkhead-runner`, built with `cargo build --release -p bulkhead-sql`, or name it in `[native] sql`; a node with datasources and no such file refuses to start. It belongs to the node's install, as the runner does. Names starting `bulkhead-sql-` are reserved: no bundle or `[drivers.<app>]` may take one, and a datasource's name must make a valid app name with that prefix (lower-case letters, digits, `-` and `_`).
- **One per datasource.** Each driver runs as one instance that `bulkhead ps` lists by its app name, with its log lines under that name, `memory_max` 256 MiB, restarted when it exits (0.1 to 1 s apart, never giving up). A node with a datasource whose driver does not start refuses to start, with the driver's exit reason. A witness starts none.
- **Connections.** The driver opens no socket: the node connects it to the datasource's one address when its pool needs a connection, within `connect_timeout_ms`. A `host` that is a name is resolved through `bulkhead-resolve` on each new connection, so a database moved to a new address is reached by the next connection. A `host` that is a directory (`/run/postgresql`) means Postgres's socket there (`.s.PGSQL.<port>`), which the node connects: peer authentication sees the node's user, so map that user to the database user (`pg_ident.conf`), or name it `user`. The driver needs no access to the socket's directory.
- **Passwords as files.** A `password_secret` bound to a file (an absolute path) is read by the driver alone, at its start, from `/driver/<file name>`; the node does not open it. The same holds for `ca_file`, so the two need distinct file names. A secret bound to an environment variable is read by the node at the driver's start and passed as an argument, so prefer a file. To change a password, rewrite the file in place (as for certificates: the driver's access is bound to the file it was given) and kill the datasource's driver (`bulkhead kill <pid>`, the pid from `bulkhead ps`); its supervisor starts it again with the new password. Transactions open in it are lost: their next statements answer `unavailable`.
- **Under the privileged helper** the driver runs as a user of its own. The helper's `binds` must hold the password file's and the CA file's directories, which the node shares with the driver as it does certificates: each file a regular file the node's user owns, with one link, gets the driver's group and group read, and its directory search for others. Keep password files in a directory of their own owned by the node's user, mode 0700.
- **Restarts.** A statement in flight when the driver ends answers `unavailable`, as do later statements of transactions it held; other requests wait for the new driver, up to `acquire_timeout_ms`. An instance's connection cap per datasource is the node's, so a driver restart does not change it.
- **Metrics:** `bulkhead_sql_requests_total`, `bulkhead_sql_requests_failed_total` and `bulkhead_sql_connections_total`, labelled `datasource` (OBSERVABILITY.md).
- **Cost.** A request through the driver takes 50 to 180 µs longer than through the node's own pool did, for 60 to 190 µs more CPU; 16 instances at once get about half the statements a second, since the driver runs every connection on one thread: about 21,000 a second on 16 cores and 5,500 on a Raspberry Pi 5 (BASELINE.md, 2026-11-15). Split a busier database across two datasources until the driver uses more threads (ROADMAP DA3b).

## HTTP egress

Since ROADMAP DA4 (2026-11-15) the node sends no app's HTTP request itself: for each app granted `bulkhead:http-egress` with an `allow` list it starts `bulkhead-egress` as the app `bulkhead-egress-<app>`, which holds that app's connection pool and sends the requests the node checked and forwards, over connections the node opens for it (DESIGN.md, "HTTP egress"; SECURITY.md, "HTTP egress"). Apps and their manifests are unchanged.

- **Install** `bulkhead-egress` beside `bulkhead-runner`, built with `cargo build --release -p bulkhead-egress`, or name it in `[native] egress`; deploying, starting or upgrading an app that needs it without such a file fails with that reason. It belongs to the node's install, as the runner does. Names starting `bulkhead-egress-` are reserved: no bundle or `[drivers.<app>]` may take one. An app declaring `bulkhead:http-egress` may have a name of at most 48 characters, so that its driver's name is a valid app name; a longer one is refused at deploy.
- **One per app,** started before the app and removed after it stops or is removed; an upgrade keeps it, and one that adds egress starts it. Each runs as one instance that `bulkhead ps` lists by its app name, with its log lines under that name, `memory_max` 256 MiB, restarted when it exits (never giving up). Idle, it uses about 5 to 6 MiB resident.
- **Connections.** The driver opens no socket: the node connects it to a host and port its app's `allow` names, within `connect_timeout_ms`, resolving a name through `bulkhead-resolve` on each new connection. The pool keeps idle connections for `pool_idle_timeout_ms`, at most `pool_max_idle_per_host` a host.
- **Root certificates** are read by the node once, as it starts, from the system (`system_roots`; `SSL_CERT_FILE` and `SSL_CERT_DIR` in the node's environment) and from `ca_file`, and passed to each driver as text; the driver reads no file. A new CA, or a change to the system's roots, takes effect when the node restarts.
- **Response size.** `[egress] max_response_body` (default 16 MiB, at most 16 MiB) bounds a response body; a larger one fails the request.
- **Under the privileged helper** the driver runs as a user of its own; nothing needs sharing with it.
- **Restarts.** A request in flight when the driver ends fails, and its slot is freed; others wait for the new driver, up to `connect_timeout_ms`.
- **Metrics:** `bulkhead_egress_connections_total` from the driver, labelled `for` (OBSERVABILITY.md), beside the node's `bulkhead_egress_requests_total`.
- **Cost.** A request through the driver takes 20 to 50 µs longer on 16 cores and 80 to 90 µs on a Raspberry Pi 5 than through the node's own pool did; 16 instances of one app at once get four fifths of the requests a second on 16 cores and two fifths on the Pi (about 15,000), since the driver runs every connection on one thread; a 1 MiB response takes 1.6 to 2.8 ms more (BASELINE.md, 2026-11-15; ROADMAP DA4b).

## Modbus

Since ROADMAP DA5 (2026-11-15) the node parses no Modbus: from `[extensions."bulkhead-modbus:modbus"]`, unchanged (MANIFEST.md), it starts drivers that poll the devices and serve masters (DESIGN.md, "Host services"; SECURITY.md, "Modbus"). Apps, manifests and `bulkhead-modbus-client` are unchanged.

- **Install** `bulkhead-modbus` and `bulkhead-modbus-server` beside `bulkhead-runner` (`cargo build --release -p bulkhead-modbus`), or name them in `[native] modbus` and `modbus_server`. A node with Modbus devices or a server and no such programs refuses to start. The node's `modbus` cargo feature is gone: `bulkhead` no longer needs building with it.
- **Which drivers run:** `bulkhead-modbus` for the devices, `bulkhead-modbus-primary` (a singleton: on an HA pair, the primary alone while its lease holds) for `primary_only` devices, `bulkhead-modbus-server` for the server; each one instance, restarted when it exits, listed by `bulkhead ps`, its log lines under its name. A witness starts none. The names are reserved for bundles and `[drivers.<app>]`.
- **Connections and ports:** each driver opens nothing itself. The node connects the devices' driver to each device's `address`, and binds the server's `listen` and passes it, so the port stays open while its driver restarts. Under the privileged helper each driver runs as a user of its own.
- **Logs:** a device going offline or back is logged by its driver. The node's old "modbus polling paused/resumed" lines are gone: whether `primary_only` devices are polled follows whether the singleton runs (`bulkhead ps`).
- **Cost:** 5 to 7% more CPU per update where the receiving pool keeps up, and far more updates stored where it does not (BASELINE.md, 2026-11-15).

## Blobs

Since ROADMAP BL1 (2026-11-15) apps granted `bulkhead:blobs` pass data to each other as blobs: memory the node makes, one app fills and apps it shares it with read (DESIGN.md, "Blobs"; MANIFEST.md).

- **Limits** (`[blobs]`, all optional): `max_bytes` (1 GiB) is the most an app's `max_bytes` may be, refused at deploy above it; `max_blob` (256 MiB) the largest single blob; `default_ttl_s` (60) and `max_ttl_s` (3600) how long a sealed blob lives when its app asks for no time, and at most. A node refuses to start with `max_blob` over `max_bytes` or the default over the maximum.
- **Memory.** Blobs are memory, not disk, and a node restart loses them. On Linux they count in the writing instance's cgroup (`memory_max`), then its app's; on FreeBSD in each process that maps one. Size `max_bytes` and the apps' `memory_max` together.
- **Watching:** `bulkhead status <app>` shows the blobs the node holds for an app and their bytes; `bulkhead_blobs_bytes` and `bulkhead_blobs_total` (OBSERVABILITY.md). A blob is freed once every app it concerns released it or it expired; an app that never releases its blobs holds its quota until they expire.

## The privileged helper

With the privileged helper, `bulkhead-priv` (ROADMAP PS1), each app runs under a user of its own instead of the node's. The service manager starts the helper as root; the helper starts the node under the node's user and then does only what the node asks: start an instance, signal one, bind a low port (ROADMAP PS3b). Three kinds of processes:

| Process | User | Started by |
| --- | --- | --- |
| `bulkhead-priv CONFIG` | root, with the capabilities below only (Linux) | systemd or rc |
| `bulkhead node` | the node's user, no capabilities | the helper, with its socket at `BULKHEAD_PRIV_FD` |
| each instance (`bulkhead-runner`, then the app) | its app's user, from the helper's range | the helper, at the node's request |

Set up:

- [`examples/systemd/bulkhead-priv.service`](../examples/systemd/bulkhead-priv.service) (in place of `bulkhead.service`) and [`examples/freebsd/bulkhead`](../examples/freebsd/bulkhead) (rc.d) start the helper; their headers list the install steps. `scripts/priv-e2e-systemd.sh` and `scripts/priv-e2e-rcd.sh` run them end to end, as root on a test machine.
- The helper's configuration, [`examples/systemd/priv.conf`](../examples/systemd/priv.conf), is root's, mode 0600; the helper refuses a file anyone else may write. It names the node's program, arguments, user and group (as numbers), `bulkhead-runner` (the only program an instance starts as), the range of app users, its state directory, the directories an instance's directories may be under (`binds`: `data_dir` and the volumes' parents), the ceilings on `memory_max` and `pids_max`, the low ports it binds (`ports`), and on Linux the cgroup systemd delegates to it.
- Node config sets `[priv]`, leaves `[native] cgroup` unset, and names the helper's runner in `[native] runner`: the node finds its own drivers (`bulkhead-resolve`, `bulkhead-ingress`, `bulkhead-sql`, `bulkhead-admin`, `bulkhead-metrics`) beside it, in the directory every instance's root shows.
- **Low ports** (ROADMAP PS3b). The node holds no capabilities, so the system refuses it a port below 1024. It binds every TCP listener itself first (`[http]`, `[admin]`, `[metrics]`, `[listeners]`, `[drivers.<app>.listeners]`, the Modbus server, an app's listener under `bulkhead:sockets`); when the system refuses it the port, it asks the helper, which binds the address if `ports` lists its port, and passes the listener back (the node logs `asking the privileged helper to bind a port`). A port missing from `ports` fails with the helper's reason (`port 80 is not one the helper binds`). The helper binds TCP only: a UDP port below 1024 (an app's UDP socket, the cluster's QUIC port) stays refused under the helper; use a port of 1024 or above.
- `bulkhead status APP` shows `runs as: its own user` under the helper, `the node's user` without.

What the helper does for each instance:

- **Its user.** An app gets the lowest free user of the range on its first start and keeps it, across restarts of the helper (`state/uids`); a user never serves a second app. The node names the app; the helper picks the user.
- **Its directories** (ROADMAP PS2). When the node writes an app's files (a deploy, an upgrade, the `priv/` files again at each node start), it asks the helper to give them to the app, once, not at each instance start: `compiled/<app>` and `priv/<app>` go to the app's group (its user's own number) with group read, and nothing for others; a read-write volume's top directory becomes the app's user's and group's, mode 0770, so what the app makes there is its own and the node keeps no write to it. Only what the node's user owns is changed, through descriptors opened without following links. Files the node wrote into a volume before stay the node's. An instance's start binds its directories as they are and changes nothing; a volume is one app's, since a second app's share would take it.
- **Linux:** a cgroup per instance under the delegated one (`apps/<app>/i<n>`: `memory.max`, no swap, `pids.max`; the `cpu` controller stays off, since it cost every call 2.4 to 3.3 µs on the Pi, ROADMAP PS6b; the helper itself in `helper`, the node in `node`), whose `cgroup.kill` the node may write to end the instance and all it forked; new mount, PID, network, IPC and UTS namespaces; and a minimal root: read-only `/usr`, the library directories, `/etc/ld.so.cache` and `/etc/localtime`, the runner's directory and the instance's directories at their own paths (a writable one without `exec`), five device files, a private `/tmp` (`tmp_size`) and its own `/proc`. Landlock still limits writes to the read-write volumes, so `/tmp` is not writable under it.
- **FreeBSD:** a jail of its own, `bulkhead-<helper's pid>-i<n>`, with no IPv4 or IPv6, on a root under `state/jails/i<n>`: a 1 MiB tmpfs owned by root with read-only nullfs views of `/lib`, `/usr/lib`, `/libexec` and the runner's directory, the instance's directories at their own paths (a writable one without `exec`), a devfs with the helper's ruleset (`devfs_ruleset`, default 4290, defined by the helper at its start with `/sbin/devfs`: `null`, `zero`, `random`, `urandom`, `pvclock`) and a private `/tmp` (`tmp_size`); `rctl` rules on the jail (`vmemoryuse` and `memoryuse` at `memory_max`, `maxproc` and `nthr` at `pids_max`; `kern.racct.enable=1`, in `/boot/loader.conf`), a session and process group of its own and `RLIMIT_AS`. `SIGKILL` removes the jail, which ends everything in it. A dying jail holds its root for a moment, so the helper retries a busy unmount and forces it after 5 s; at its start it unmounts whatever a killed helper left under `state/jails`.
- **Its end.** The helper reports each exit to the node, and whether the memory limit caused it, before it removes the instance's cgroup or rules. When the node exits or its socket closes, the helper kills every instance, waits up to 5 s, and exits with the node's status.

Signals: the helper passes `SIGTERM` and `SIGINT` to the node as `SIGTERM`, and `SIGHUP` as `SIGHUP`; the node stops its apps and exits, then the helper does. systemd's `KillMode=mixed` signals the helper alone.

The helper's own fence on Linux, once the node runs: `no_new_privs`; only `CAP_CHOWN`, `CAP_DAC_OVERRIDE`, `CAP_DAC_READ_SEARCH`, `CAP_FOWNER`, `CAP_KILL`, `CAP_SETGID`, `CAP_SETUID`, `CAP_NET_BIND_SERVICE` and `CAP_SYS_ADMIN`, in every set and the bounding set; and a seccomp allow-list (SECURITY.md, "The privileged helper"). A call off the list ends the helper, and with it the node's connection and every instance, after the line `bulkhead-priv: system call N refused by the helper's seccomp filter` (exit status 159); `N` is the call's number on the machine's architecture (`ausyscall N`, from the audit tools, names it). Report it as a bug. To find every such call on a test machine without refusing them, start the helper with `BULKHEAD_PRIV_SECCOMP_LOG=1`: the kernel then logs each one (`type=1326 … syscall=N` in `dmesg`) and allows it. Never set it in production. On FreeBSD it sets `no_new_privs` and stays root. The unit's own fences leave out `RestrictNamespaces` (its filter makes `clone3` fail), `ProtectHostname` (an instance sets its own host name), `ProtectControlGroups` and `ProtectKernelTunables` (the helper writes its cgroups) and `RestrictSUIDSGID` (set-group-id bits the helper used to set, before PS2; it may now be set).

Costs, on a Raspberry Pi 5 (BASELINE.md, 2026-11-11): a start takes about 1.3 ms more, a call from the host 2.7 µs more (20.3 against 17.6 µs at p50), calls in flight 10 to 15% fewer a second; requests and bucket reads inside an app cost the same.
