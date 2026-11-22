# Tutorial: writing an app

A chat room, written from an empty crate up: an app that answers calls,
then one that logs and counts, then two apps talking, then state kept in a
bucket and watched, then tests. The operator's side (pairs, failover,
upgrades) is the other tutorial, [`examples/tutorial`](../tutorial).

Each chapter is a script that builds what it needs, starts a node in a
temporary directory, runs the commands it describes and checks their
output, pausing between steps; it stops the node at the end or on Ctrl-C.
Run them from the repository root. They need Linux or FreeBSD, bash 5,
and a Rust toolchain; the first build takes a few minutes.

```bash
examples/app-tutorial/01-node.sh
examples/app-tutorial/02-room.sh
examples/app-tutorial/03-room.sh
examples/app-tutorial/04-chat.sh
examples/app-tutorial/05-chat.sh
examples/app-tutorial/06-test.sh
AUTO=1 examples/app-tutorial/02-room.sh      # no pauses
PROFILE=release examples/app-tutorial/02-room.sh
```

Each chapter's code is a crate of its own (`02-room`, `03-room`,
`04-room` and `04-history`, `05-room` and `05-history`), complete, so
`diff -r examples/app-tutorial/04-room examples/app-tutorial/05-room`
shows what a chapter adds. `crates/bulkhead/tests/app_tutorial.rs` runs
every script, so the output shown here is what the scripts check.

## Chapter 1: a node

`01-node.sh` writes the smallest useful node config and starts the node:

```toml
node = 1
data_dir = "/tmp/.../data"
admin_socket = "/tmp/bulkhead-apptut-1234.sock"
trusted_keys = ["RWT..."]          # whose bundles it runs: signer.pub
policy = "/tmp/.../policy.toml"     # what it grants each app
```

```text
$ bulkhead node --config node.toml
$ bulkhead list
APP               VERSION   STATUS   INSTANCES  PREVIOUS
bulkhead-resolve  built-in  running  1          -
```

`bulkhead node` runs in the foreground and logs to stderr (`RUST_LOG`,
default `info`). Every other command talks to it over its admin socket
(`--socket`, or `BULKHEAD_SOCKET`). The one line is the node's own
resolver of host names, a driver it starts itself.

SIGTERM, or Ctrl-C (SIGINT) in a terminal, stops it: each app's
`terminate` runs, then the node exits with status 0. What it keeps is
under `data_dir`: the deployed bundles (`bundles/`), what runs
(`state.json`, once something is deployed), the keyvalue data (`kv/`),
and `creation`, raised on every start and part of every pid (the `@2` in
`<1.3.1@2>`). OPERATIONS.md lists the rest. Started again on the same
`data_dir`, the node starts what was running.

## Chapter 2: a first app

An app is a Rust crate on `bulkhead-sdk`, built as an executable:

```toml
[package]
name = "room"
version = "0.1.0"
edition = "2024"

[dependencies]
bulkhead-sdk = { path = "../bulkhead/crates/bulkhead-sdk" }
```

`src/lib.rs` holds the app: a type whose value is one instance's state,
and the callbacks the node runs on it, one at a time
([`02-room/src/lib.rs`](02-room/src/lib.rs)):

```rust
use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Reply};

struct Room {
    posts: Vec<String>,
}

impl App for Room {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        Ok(Self { posts: Vec::new() })
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let text = msg.as_text().unwrap_or("");
        let answer = if let Some(post) = text.strip_prefix("post ") {
            self.posts.push(post.to_string());
            format!("#{}", self.posts.len())
        } else if text == "recent" {
            self.posts.join("\n")
        } else {
            "say `post <text>` or `recent`".to_string()
        };
        Reply::Now(Envelope::text(answer))
    }
}

bulkhead_sdk::app!(Room);
```

`app!` generates `bulkhead_main()`, which `src/main.rs` calls:

```rust
fn main() -> std::process::ExitCode {
    room::bulkhead_main()
}
```

The manifest, `bulkhead.toml`, names the app and says what it needs
([MANIFEST.md](../../docs/MANIFEST.md) has every field):

```toml
[app]
name = "room"
version = "0.1.0"

[appserver.limits]
memory_max = "32MiB"
```

A node runs only bundles signed by a key it trusts. `bulkhead keygen`
makes one (`signer.key`, `signer.pub`; the script's node trusts its
public half), `bulkhead bundle` packs the program and the manifest and
signs them, and `bulkhead deploy` hands the bundle to the node:

```text
$ cargo build -p tut-room-2
$ bulkhead bundle 02-room/bulkhead.toml target/debug/tut-room-2 --key signer.key
$ bulkhead deploy 02-room-bulkhead.rbp
deployed room 0.1.0 (running)
$ bulkhead call room "post hello"
#1
$ bulkhead call room recent
hello
again
```

Each instance is a process of its own, started through `bulkhead-runner`,
which fences it (its own files only, no sockets of its own) before the
program runs. `bulkhead kill` ends one; the app's supervisor starts the
next incarnation of the same instance (`1.3.1` to `1.3.2`), from `init`,
so the posts are gone:

```text
$ bulkhead kill 1.3.1@1
$ bulkhead call room recent

```

Restarted, the node starts the app again from its stored bundle.

What this shows: an app is a state type and its callbacks; an instance's
memory lasts as long as the instance.

## Chapter 3: seeing inside

[`03-room`](03-room/src/lib.rs) logs with the SDK's macros and counts posts
in a metric:

```rust
use bulkhead_sdk::metrics::Counter;
use bulkhead_sdk::{debug, info, warn};

// in init:
let posted = Counter::new("posts_total", "Posts made in the room", &[])
    .map_err(|e| format!("metric: {e}"))?;
info!("room open");

// on a post:
self.posted.inc();
info!("post #{}: {post}", self.posts.len());
```

Logging and metrics are host services, so the manifest asks for them and
the operator's policy grants them:

```toml
[appserver.observability]
log_level = "info"

[appserver.capabilities."bulkhead:logging"]

[appserver.capabilities."bulkhead:metrics"]
prefix = "room"                    # exported as room_<name>
```

```toml
# policy.toml
[apps.room]
grant = ["bulkhead:logging", "bulkhead:metrics"]
```

An ungranted service answers `access-denied` (and an ungranted logger
drops records). The node keeps each app's recent records:

```text
$ bulkhead logs room
1795309916964 INFO  <1.3.1@1> [] room open
1795309916984 INFO  <1.3.1@1> [] post #1: hello
1795309916999 WARN  <1.3.1@1> [] not understood: "shout"
$ bulkhead log-level room debug
$ bulkhead logs room -n 1
1795309989114 DEBUG <1.3.1@1> [] recent: 2 posts
$ bulkhead metrics | grep room_
room_posts_total{app="room"} 2
```

`bulkhead logs room -f` follows them; `bulkhead log-level room reset`
returns to the manifest's level. `bulkhead status room` shows the app, its
grant and its instances; `bulkhead ps` every instance with its mailbox,
memory and restarts; `bulkhead events` exits, restarts and deploys as they
happen. A program's stdout and stderr go to the same log. With root, the
node's probes trace calls and callbacks live
([`examples/tracing`](../tracing)).

## Chapter 4: two apps talking

A second app, [`history`](04-history/src/lib.rs), keeps the last posts.
The room reaches it three ways ([`04-room`](04-room/src/lib.rs)):

- **Publish.** `history` joins the process group `chat` in `init`; the
  room publishes each post to it, which arrives at every member as a cast
  (`handle_cast`). `publish` returns how many members it reached.

  ```rust
  groups::join("chat")?;                                       // history's init
  groups::publish("chat", Envelope::text(format!("said {text}")))?;  // the room
  ```

- **Call by app name.** `recent` asks `history` and waits for its answer;
  the node picks the instance.

  ```rust
  ctx.call_app("history", Envelope::text("recent"), Duration::from_secs(2))
  ```

- **Cast.** `clear` is sent with no answer. `whereis` with an app's name
  finds one of its instances.

  ```rust
  if let Some(pid) = ctx.whereis("history") {
      ctx.cast(pid, Envelope::text("clear"));
  }
  ```

The groups an app joins or publishes to are in its manifest
(`[appserver.capabilities."bulkhead:groups"]`, `join = ["chat"]` or
`publish = ["chat"]`), and its messaging ACL says whom it may send to and
who may send to it:

```toml
# history's bulkhead.toml
[appserver.messaging_acl]
accept_from = ["room"]
```

`lurker.toml` deploys the room's program under another name. `history`
does not accept it: its call fails, and its publish and casts are
dropped (the publish reaches 0 members):

```text
$ bulkhead call room "post hello"
#1 to 1
$ bulkhead call lurker recent
history: access denied
$ bulkhead call lurker "post spam"
#1 to 0
$ bulkhead call room recent
hello
again
```

`bulkhead call` itself is not checked: the admin API is the operator.
Killing `history`'s instance loses what it held, as in chapter 2.

## Chapter 5: state that outlives an instance

[`05-history`](05-history/src/lib.rs) keeps the posts in the node's shared
bucket `posts`, on disk, one key per post, numbered by a counter in the
same bucket:

```rust
let n = self.posts.increment("seq", 1)?;
self.posts.set(&key(n), text.as_bytes())?;      // p000001, p000002, ...
```

`recent` reads the last ten with a reverse scan
(`watch::scan("posts").prefix("p").reverse().limit(10)`). The bucket is
the operator's: node config defines it, policy grants it, and each
manifest names it with its access:

```toml
# node.toml
[buckets.posts]
durability = "disk"

# policy.toml
[apps.history]
grant = ["bulkhead:logging", "bulkhead:groups", "bulkhead:keyvalue"]
buckets = ["posts"]

# history's bulkhead.toml
[appserver.capabilities."bulkhead:keyvalue".shared]
posts = "rw"
```

[`05-room`](05-room/src/lib.rs) watches the bucket and logs each new
post. A watch's notices arrive in `handle_info`:

```rust
fn handle_info(&mut self, _ctx: &mut Ctx, info: Info) {
    let Some(change) = Change::from_info(&info) else { return };
    let value = change.current().ok().flatten().map(|e| e.value);
    if let Some(line) = describe(&change.key, value.as_deref()) {
        info!("{line}");
    }
    self.cursor.set_value("revision", &change.revision)?;   // logged on error
}
```

After each notice the room saves the bucket's revision in a bucket of its
own (`cursor`, declared under `buckets` in its manifest), and `init`
resumes the watch after it, falling back to a watch from now when the
bucket has compacted past it:

```rust
let started = match after.map(|rev| watch::resume("posts", "p", true, rev)) {
    Some(Ok(started)) => started,
    Some(Err(WatchError::TooOld(_))) => watch::watch_prefix("posts", "p")?,
    Some(Err(e)) => return Err(e.to_string()),
    None => watch::watch_prefix("posts", "p")?,
};
```

```text
$ bulkhead logs room
... watching posts after revision 0
... new post p000001: hello
... new post p000002: again
$ bulkhead kill <history's pid>
$ bulkhead call room recent
hello
again
$ bulkhead stop room
$ bulkhead cast history "said while you were away"
$ bulkhead start room
$ bulkhead call room seen
1
$ bulkhead logs room -n 2
... watching posts after revision 4
... new post p000003: while you were away
```

The posts survive `history`'s restart and the node's. The room's new
instance saw the one post written while it was stopped, and not the two
before.

What this shows: what an app must keep goes in a bucket (or a database),
not in an instance; a watch with a saved revision turns a bucket into a
log the app can follow across restarts.

## Chapter 6: testing it

- **The logic, without a node.** The chapter 5 crates keep parsing and key
  making in plain functions (`command`, `key`, `describe`), and their
  `lib_tests.rs` test them; `cargo test -p tut-room-5 -p tut-history-5`
  runs them on the host target, with no node. The `Ctx` methods and the
  services go to the node, so tests do not call them; keeping callbacks a
  thin shell over plain functions puts most of an app under unit tests.
- **The app on a node.** [`crates/bulkhead/tests/app_tutorial.rs`](../../crates/bulkhead/tests/app_tutorial.rs)
  has one: it writes a node config and a policy into a temporary
  directory, opens a `bulkhead::node::Node` and serves its admin API in
  the test's runtime, bundles and deploys both apps with the CLI, and
  checks a post through `room` (`seen`) and `history` (`recent`, across a
  restart of its instance). `cargo test -p bulkhead --test app_tutorial`
  runs it, and every chapter's script.

`bulkhead state <pid>` shows an SDK app's state through its
`save_snapshot`, and WRITING_APPS.md ("Testing") has more.

## Next

- [WRITING_APPS.md](../../docs/WRITING_APPS.md): the SDK in full (timers,
  links and monitors, registered names, HTTP routes, SQL, upgrades with
  state, senders for an app's own threads).
- [MANIFEST.md](../../docs/MANIFEST.md): every manifest field, node config
  and policy.
- [`examples/tutorial`](../tutorial): running apps on a pair, failover,
  hot upgrades and disaster recovery.
