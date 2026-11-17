# Observability

What an operator can see on a running node: instance stats, events and log records as they happen, direct calls to one instance, metrics for Prometheus, and probes a tracer attaches to without a restart.

## Instances

`bulkhead ps` (or `GET /v1/instances`) lists every instance:

| Column | Meaning |
| --- | --- |
| `PID` | `node.serial.incarnation@creation`. The serial stays across restarts of the instance; the incarnation grows by one each time; the creation counts the node's starts, so a pid from before a node restart never names a later instance. The instance id is the creation in its high 24 bits and the serial in its low 40. |
| `APP`, `VERSION`, `STATE` | What it runs, and its lifecycle state (`active`, `upgrading`, `stopping`, ...). |
| `QUEUED` | Messages waiting in its mailbox. |
| `DROPPED` | Casts and timer messages dropped because its mailbox was full. |
| `MEMORY` | The app's cgroup's `memory.current`, or without `[native] cgroup` the resident set of its process and that process's direct children, shared pages (the channel's rings, the program's libraries) included, read when asked. |
| `RESTARTS` | Times its supervisor restarted it (the incarnation minus one). |
| `LAST EXIT` | Why the previous incarnation exited, as its supervisor saw it. |

`bulkhead --json ps` also lists each instance's links and monitors as its node records them: `links` (pids, on any node), `monitoring` (monitors it holds: `reference` and target `pid`) and `monitored_by` (monitors others hold on it: `reference` and watcher `pid`). For a link or monitor between nodes, each node lists its own side, so once the cluster is quiet the two sides match (spike S12).

`bulkhead state <pid>` shows an instance's state through its `save-snapshot`.

Connection handlers do not appear in `ps` (an instance per connection, with no mailbox); a native app with listeners does, its instances holding the connections. `bulkhead status <app>` lists either's listeners: address, connections open now, and totals accepted, refused (over `max_connections`), crashed and closed by the idle timeout.

## Events and logs as they happen

`bulkhead events` prints the node's events from the moment it connects, until stopped; `--json` prints each as a JSON object on a line of its own (`GET /v1/events`). Every event has `time_ms`, `kind` and `event`; the kinds and their fields are an interface:

| `kind` | `event` | Fields |
| --- | --- | --- |
| `audit` | as `audit.log` (`deploy`, `upgrade`, `kill`, `remote_admin`, `misbehaved`, `quarantine`, ...) | as `audit.log` |
| `instance` | `exit` | `pid`, `app`, `reason` |
| `instance` | `restart` | `pid` (the new incarnation), `previous`, `child` (the supervisor's child id) |
| `member` | `joined`, `left`, `reachable`, `unreachable`, `down`, `path_silent`, `path_heard` | `node`, and `creation`, `session` or `path` |
| `failover` | `leadership` | `role`, `epoch`, `may_run`, `awaiting_confirmation`, `generation` |
| `node` | `lagged` | `missed`: events this follower lost by falling more than 1024 behind |

`bulkhead logs <app> --follow` prints the app's last records (`-n`, 100 by default) and then each new one (`GET /v1/apps/{app}/logs?lines=N&follow=true`). Both streams are chunked responses of JSON lines with an empty line every 15 seconds; they end when the node shuts down.

## Calling one instance

`bulkhead call` and `bulkhead cast` address an app (through its pool's routing), one instance by pid, or the instance holding a registered name:

```sh
bulkhead call orders 'ping'                     # an app
bulkhead call --pid 0.12.3@1 'ping'             # one instance
bulkhead call --name order-router 'ping'        # a registered name
bulkhead cast --name order-router 'refresh'
bulkhead call orders --cbor '{"id": 42}'        # JSON sent as CBOR; a CBOR reply prints as JSON
```

The admin API routes are `POST /v1/apps/{app}/call`, `/v1/instances/{pid}/call` and `/v1/names/{name}/call` (and `cast`), so `curl` works too:

```sh
curl --unix-socket /var/lib/bulkhead/admin.sock -H 'content-type: text/plain' \
  --data 'ping' 'http://bulkhead/v1/instances/0.12.3/call?timeout_ms=1000'
```

A call through the admin API costs about 65 µs against about 4 µs inside the node ([BASELINE.md](BASELINE.md)), so it suits operators and tests, not a data path.

## Metrics

The node renders every app's metrics and its own in the Prometheus text format (version 0.0.4): `bulkhead metrics`, `GET /v1/metrics` on the admin API, and, with `[metrics] address = "127.0.0.1:9100"` in node config, `GET /metrics` on that address for a Prometheus scraper, served by the node's `bulkhead-metrics` driver, which asks the node for the same text on each scrape (ROADMAP DA7; unauthenticated, bind it to loopback or a management network). OpenTelemetry collectors can scrape the same endpoint.

Apps' own metrics (`bulkhead:metrics`) are named `<prefix>_<name>` and carry an `app` label plus their own labels. They are per app: every instance of an app updating the same name and labels updates one series, so counters and histograms add up across a pool and survive restarts.

The host's metrics, every one with an `app` label. The names and labels are an interface, like the probes below:

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `bulkhead_instances` | gauge | | Live instances. |
| `bulkhead_memory_bytes` | gauge | | Memory of live instances, as `ps`'s `MEMORY` column counts it. |
| `bulkhead_mailbox_depth` | gauge | | Messages waiting in live instances' mailboxes. |
| `bulkhead_mailbox_bytes` | gauge | | Bytes of message bodies waiting in live instances' mailboxes (`mailbox_max_bytes` bounds each). |
| `bulkhead_callbacks_total` | counter | `kind` (`call`, `cast`, `info`) | Callbacks run; a native app's `handle_connection` counts as `info`. |
| `bulkhead_callback_guest_seconds` | histogram | | App execution time per callback, less time waiting for the node (10 µs to 1 s buckets). |
| `bulkhead_cast_wait_seconds` | histogram | | Time a native app's `cast_timeout` waited for room in a full mailbox, zero included, by the sender's app (10 µs to 5 s buckets). Its count is the number of waited casts. |
| `bulkhead_mailbox_dropped_total` | counter | | Messages dropped at a full mailbox (by count or by bytes). |
| `bulkhead_instance_exits_total` | counter | `reason` (`normal`, `shutdown`, `killed`, `noproc`, `noconnection`, `timeout`, `crashed`) | Instance exits. |
| `bulkhead_egress_requests_total` | counter | `outcome` (`sent`: a response came; `denied`; `limited`: over `max_concurrent`; `failed`: connection, TLS or timeout) | Outgoing HTTP requests (`bulkhead:http-egress`); listed once an app has made one. |
| `bulkhead_sockets_open` | gauge | | Sockets the app's instances hold open (`bulkhead:sockets`), TCP and UDP, accepted ones included; listed once an app has opened or been refused one. |
| `bulkhead_refusals_total` | counter | `kind` (`access-denied`, `acl`) | Refusals the misbehaviour policy counted (ROADMAP PS5), for apps that met any. |
| `bulkhead_policy_kills_total` | counter | `reason` (`refusals`, `rate`) | Instances the misbehaviour policy killed. |
| `bulkhead_sockets_refused_total` | counter | | Socket addresses the app's policy refused (bind, listen, connect, send, receive) and sockets refused over `max_sockets`. |
| `bulkhead_blobs_bytes` | gauge | | Bytes of the app's blobs the node holds (`bulkhead:blobs`, ROADMAP BL1), against its `max_bytes`; listed once the app has asked for a blob. |
| `bulkhead_blobs_total` | counter | `outcome` (`created`; `sealed`; `refused`: a request refused; `freed`: released by every app, or unsealed when its instance ended; `expired`) | The app's blobs and blob requests. |

With an HTTP ingress, also, counters from its driver app (label `app="bulkhead-ingress"`, added to once a second, so up to a second behind): `bulkhead_http_requests_total` (requests answered), `bulkhead_http_server_errors_total` (answered with a 5xx status), `bulkhead_http_not_found_total` (matched no route), `bulkhead_http_redirected_total` and `bulkhead_http_forwarded_total` (requests a standby redirected or forwarded to its failover partner), `bulkhead_http_refused_connections_total` (closed on accept, over `max_connections`). Before ROADMAP DA2 (2026-11-14) the node's own ingress kept them without a label.

On a member node, also, counters from its resolver of host names (label `app="bulkhead-resolve"`, ROADMAP DA9): `bulkhead_resolve_lookups_total` (lookups the node asked of it), `bulkhead_resolve_lookups_failed_total` (those that found no address: no such name, or no nameserver answered), `bulkhead_resolve_streams_total` (TCP streams to nameservers the node opened for it; one per nameserver while they stay open).

With `[datasources]`, also, counters from each datasource's driver (labels `app="bulkhead-sql-<name>"` and `datasource="<name>"`, ROADMAP DA3): `bulkhead_sql_requests_total` (requests the node forwarded: statements, begins and ends), `bulkhead_sql_requests_failed_total` (those answered with an error), `bulkhead_sql_connections_total` (connections to the database the node opened for its pool; with pooled connections reused, it grows only as the pool fills or replaces one). `bulkhead ps` lists each driver's instance under its app name.

With an app granted `bulkhead:http-egress`, also, a counter from its egress driver (labels `app="bulkhead-egress-<app>"` and `for="<app>"`, ROADMAP DA4): `bulkhead_egress_connections_total` (connections the node opened for the app's requests; with pooled connections reused, it grows only as the pool fills or replaces one). The app's own `bulkhead_egress_requests_total` is the node's, as before. `bulkhead ps` lists the driver under its app name.

With a cluster transport, also, labelled `node` (the other node): `bulkhead_cluster_send_queue_bytes` (gauge: bytes of casts, calls and replies queued for it) and `bulkhead_cluster_send_refused_total` (counter: messages refused past `[cluster] send_buffer_bytes`, and notices refused past the frame cap, since the node started).

On a pair node with `cluster` buckets, gauges for the replicated log: `bulkhead_replication_commit_index` (the last entry applied to this node's copy) and `bulkhead_replication_last_index` (the last entry in its log); on the primary, `bulkhead_replication_partner_lag_entries` (entries the standby has not acknowledged) and `bulkhead_replication_alone` (1 while it commits without the standby, which the witness has recorded out of date).


Gauges are read when the metrics are rendered; counters are kept per app from the node's start, and a counter's series starts once the app's first instance starts.

## Probes

The `bulkhead` binary carries USDT probes on the message path (provider `bulkhead`, from `bulkhead`'s `probes` cargo feature, on by default; without it the probe calls compile to nothing). They use anyprobe: SystemTap SDT on Linux (bpftrace, perf), DTrace USDT on macOS and FreeBSD, ETW on Windows. With no tracer attached a probe costs one enabled check and its arguments are not computed; `baseline` shows no difference beyond run-to-run noise ([BASELINE.md](BASELINE.md)).

An instance is identified by its pid's whole `id` (and `incarnation` where shown): the node's creation shifted left 40 bits, plus the serial `ps` shows. In bpftrace, `arg0 & 0xffffffffff` is the serial and `arg0 >> 40` the creation. A sender of 0 is a message from outside any instance (the admin API). A `&str` argument is two values, a pointer and a length.

| Probe | Arguments |
| --- | --- |
| `call__start` | call id, sender id, target id, body bytes |
| `call__done` | call id, target id, outcome (`ok`, `timeout`, `noproc`, `overloaded`, `too-large`, `access-denied`, `exited`, `noconnection`) |
| `cast` | sender id, target id, body bytes, outcome |
| `cast__wait` | sender id, target id, nanoseconds waited for room, outcome (`ok`, `timeout`, or why it was not queued); a native app's `cast_timeout`, which also fires `cast` |
| `callback__start` | instance id, kind (`call`, `cast`, `info`, or `connection` for a native app's `handle_connection`) |
| `callback__done` | instance id, kind, app execution in nanoseconds (time spent waiting excluded), succeeded |
| `instance__exit` | instance id, incarnation, exit reason |
| `supervisor__restart` | new instance id, incarnation, child id |
| `upgrade__start` | app, target version |
| `upgrade__done` | app, target version, completed |
| `mailbox__drop` | instance id, kind (`cast`, `timer`) |
| `connection__open` | handler instance id, listener name; its end is `instance__exit` |
| `node__down` | node, the session number that follows; the session with that node ended (links and monitors with it got `noconnection`) |

The call id pairs `call__start` with `call__done`; it is 0 if no tracer was attached when the call started. Probe names and arguments are an interface: scripts depend on them, so they change only with a CHANGELOG entry.

List them in a binary with `readelf -n target/release/bulkhead | grep -A3 stapsdt`, or `cargo anyprobe` (from the anyprobe repository), which also writes tracer scripts that decode every argument.

[`examples/tracing`](../examples/tracing/README.md) runs a node with traffic worth tracing (calls between apps, kills and restarts, an overflowing mailbox) and has ready-made bpftrace scripts, and the same as D scripts for DTrace.

bpftrace one-liners, attached to a running node (root; `buf(ptr, len)` reads a `&str`):

```sh
PID=$(pgrep -f 'bulkhead node')

# Call latency per target instance, in microseconds.
sudo bpftrace -p $PID -e '
usdt:*:bulkhead:call__start { @t[arg0] = nsecs; }
usdt:*:bulkhead:call__done /@t[arg0]/ { @us[arg1] = hist((nsecs - @t[arg0]) / 1000); delete(@t[arg0]); }'

# App execution per callback, by instance.
sudo bpftrace -p $PID -e 'usdt:*:bulkhead:callback__done { @guest_us[arg0] = hist(arg3 / 1000); }'

# Who casts to whom.
sudo bpftrace -p $PID -e 'usdt:*:bulkhead:cast { @casts[arg0, arg1] = count(); }'

# Exits and restarts as they happen.
sudo bpftrace -p $PID -e '
usdt:*:bulkhead:instance__exit { printf("exit %d.%d %r\n", arg0, arg1, buf(arg2, arg3)); }
usdt:*:bulkhead:supervisor__restart { printf("restart %d.%d %r\n", arg0, arg1, buf(arg2, arg3)); }'

# Messages dropped by full mailboxes.
sudo bpftrace -p $PID -e 'usdt:*:bulkhead:mailbox__drop { @drops[arg0] = count(); }'
```

On FreeBSD (x86-64) the same probes are DTrace USDT, which DTrace names with `-` for `__` and a provider per process (`bulkhead<pid>`); `copyinstr(ptr, len)` reads a `&str`. The node hands them to DTrace when it starts, through `/dev/dtrace/helper`, so DTrace has to be loaded first (`kldload dtraceall`, or `dtraceall_load="YES"` in `/boot/loader.conf`) and the device open to the node's user (`root:wheel`, mode 0660, by default; a devfs rule gives the node's group access). A node that cannot register logs `probes not registered` at startup, and a tracer sees none of its probes until it restarts. One `dtrace -p` can attach to a process at a time.

```sh
PID=$(pgrep -f 'bulkhead node')
dtrace -l -P "bulkhead$PID"     # the probes the node registered

# Call latency per target instance, in microseconds.
dtrace -q -p $PID -n '
bulkhead$target:::call-start { t[arg0] = timestamp; }
bulkhead$target:::call-done /t[arg0]/ { @us[arg1] = quantize((timestamp - t[arg0]) / 1000); t[arg0] = 0; }'

# Exits as they happen.
dtrace -q -p $PID -n 'bulkhead$target:::instance-exit { printf("exit %d.%d %s\n", arg0, arg1, copyinstr(arg2, arg3)); }'
```

Probes see what the host sees: pids, sizes, kinds, timings and outcomes. They do not see message bodies or app internals.

Verified so far: the probe notes are in the Linux x86-64 binary (`readelf`), and the cost with no tracer attached. Attaching bpftrace to a live node has not been run in this repository's tests; it needs root. On FreeBSD 15 (a VM, 2026-11-17) every D script in `examples/tracing/d` ran against the tracing demo and printed its traffic; that is not in the automated checks either.
