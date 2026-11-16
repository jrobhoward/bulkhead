# Greeter: a service as an app

A service an app offers other apps is itself an app: it answers calls, and its manifest says whom it serves. Until ROADMAP DA8 (2026-11-15) this example was an extension, host code built into the node; no code from outside Bulkhead runs in the node any more, so the greeter runs sandboxed like any app.

- `service/`: the app `greeter`. It answers `client::Request::Hello { name }` with its salutation (from `start_args`) and `Request::Count` with its count of greetings. Its `src/client.rs` is the client module: the messages and a typed function for each. Its `bulkhead.toml` serves `greeted` alone (`messaging_acl.accept_from`).
- `app/`: the app `greeted`, which calls the greeter through `example_greeter_service::client` (`hello:<name>`, `count`). Its `bulkhead.toml` may call `greeter` (`messaging_acl.may_call`) and depends on it. `stranger.toml` is the same program under another name, which the greeter refuses.

`crates/bulkhead/tests/examples.rs` bundles and deploys all three and calls them:

```sh
cargo test -p bulkhead --test examples
```

By hand, with a node running (GETTING_STARTED.md):

```sh
cargo build --release -p example-greeter-service -p example-greeter-app
bulkhead bundle examples/greeter/service/bulkhead.toml target/release/example-greeter-service --key bulkhead.key -o greeter.rbp
bulkhead bundle examples/greeter/app/bulkhead.toml target/release/example-greeter-app --key bulkhead.key -o greeted.rbp
bulkhead deploy greeter.rbp
bulkhead deploy greeted.rbp
bulkhead call greeted hello:Ann     # Howdy, Ann
bulkhead call greeted count         # 1
```

## Writing a service app

1. **Messages:** define the requests and answers as serde types with a schema name (`client.rs`), and answer them in `handle_call`.
2. **A client module:** a typed function per request, calling the app by name with `Ctx::call_app`, so the node picks the instance by the app's routing (a pool of greeters works unchanged). Map `CallError::AccessDenied` and `CallError::Noproc` to the client's own errors. Callers depend on the service's crate for this module.
3. **Whom it serves:** list the calling apps in the service's `messaging_acl.accept_from`, and the service in each caller's `may_call`. The node checks both on the service's node, against the caller's app as its node stamps it; a refused call fails with `access-denied`. A caller that needs the service at start lists it under `dependencies`.
4. **What it may use:** the service declares the capabilities it needs (buckets, SQL, egress) in its own manifest, and the operator grants them to it alone. Its callers need none of them.
