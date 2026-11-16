//! The remote admin API: TLS 1.3 with pinned keys both ways, terminated by
//! the node's admin driver (`bulkhead-admin`, ROADMAP DA7), each request
//! signed by the operator's key and checked by the node, the same routes as
//! the admin socket, changes audited with the operator's name, and the
//! operators file read again, by both, on reload.

#![cfg(unix)]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bulkhead::api::Client;
use bulkhead_host::transport::quic::{PublicKey, admin_client_tls, generate_key};
use common::TestNode;

/// A new private key in `dir/<name>.key`, readable by its owner only, and
/// its public key.
fn key(dir: &Path, name: &str) -> (PathBuf, PublicKey) {
    let (pem, public) = generate_key().unwrap();
    let path = dir.join(format!("{name}.key"));
    std::fs::write(&path, pem).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    (path, public)
}

fn free_tcp() -> String {
    format!("127.0.0.1:{}", common::ports::free_port().unwrap())
}

fn operators(path: &Path, entries: &[(&str, &PublicKey)]) {
    let mut text = String::from("[operators]\n");
    for (name, key) in entries {
        text.push_str(&format!("{name} = [\"{key}\"]\n"));
    }
    std::fs::write(path, text).unwrap();
}

struct Remote {
    node: TestNode,
    address: String,
    node_key: PublicKey,
    alice: PathBuf,
    operators: PathBuf,
    keys: tempfile::TempDir,
}

impl Remote {
    async fn start() -> Self {
        let keys = tempfile::tempdir().unwrap();
        let (node_path, node_key) = key(keys.path(), "node");
        let (alice, alice_key) = key(keys.path(), "alice");
        let operators_path = keys.path().join("operators.toml");
        operators(&operators_path, &[("alice", &alice_key)]);
        let address = free_tcp();
        let node = TestNode::start(&format!(
            "[admin]\naddress = \"{address}\"\noperators = {operators_path:?}\nkey = {node_path:?}\n"
        ))
        .await;
        node.node().start_remote_admin().await.unwrap();
        Self {
            node,
            address,
            node_key,
            alice,
            operators: operators_path,
            keys,
        }
    }

    fn client(&self, operator: &Path, node_key: PublicKey) -> Client {
        let tls = admin_client_tls(operator, node_key.clone()).unwrap();
        let signer = bulkhead_keys::signature::Signer::from_file(operator).unwrap();
        Client::remote(
            self.address.clone(),
            Arc::new(tls),
            Arc::new(signer),
            node_key,
        )
    }

    /// A `GET /v1/apps` over TLS as `operator` with `auth` as its
    /// signature header, if any: its status.
    async fn raw(&self, operator: &Path, auth: Option<&str>) -> u16 {
        use http_body_util::Empty;
        let tls = admin_client_tls(operator, self.node_key.clone()).unwrap();
        let stream = tokio::net::TcpStream::connect(&self.address).await.unwrap();
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from("bulkhead").unwrap();
        let tls = tokio_rustls::TlsConnector::from(Arc::new(tls))
            .connect(name, stream)
            .await
            .unwrap();
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(tls))
                .await
                .unwrap();
        tokio::spawn(connection);
        let mut request = hyper::Request::get("/v1/apps").header("host", "bulkhead");
        if let Some(auth) = auth {
            request = request.header(bulkhead_keys::signature::HEADER, auth);
        }
        let response = sender
            .send_request(request.body(Empty::<bytes::Bytes>::new()).unwrap())
            .await
            .unwrap();
        response.status().as_u16()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_admin____listed_operator____same_routes_and_changes_audited() {
    let mut r = Remote::start().await;
    let alice = r.client(&r.alice, r.node_key.clone());

    let apps = alice.list().await;
    let stop = alice.stop("nothing-here").await;

    assert!(apps.unwrap().is_empty());
    assert!(stop.is_err(), "no such app");
    let audited: Vec<_> = r
        .node
        .audit()
        .into_iter()
        .filter(|a| a["event"] == "remote_admin")
        .collect();
    assert_eq!(audited.len(), 1, "{audited:?}");
    assert_eq!(audited[0]["operator"], "alice");
    assert_eq!(audited[0]["method"], "POST");
    r.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_admin____events____streamed_over_tls() {
    let mut r = Remote::start().await;
    let alice = r.client(&r.alice, r.node_key.clone());
    let mut events = alice.events().await.unwrap();

    let _ = alice.stop("nothing-here").await;
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    assert_eq!(
        (first.kind.as_str(), first.event.as_str()),
        ("audit", "remote_admin")
    );
    assert_eq!(first.fields["operator"], "alice");
    r.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_admin____unlisted_operator_or_wrong_node_key____refused() {
    let mut r = Remote::start().await;
    let (mallory, _) = key(r.keys.path(), "mallory");
    let (_, other_node) = key(r.keys.path(), "other");

    let unlisted = r.client(&mallory, r.node_key.clone()).list().await;
    let wrong_node = r.client(&r.alice, other_node).list().await;

    assert!(unlisted.is_err(), "{unlisted:?}");
    assert!(wrong_node.is_err(), "{wrong_node:?}");
    r.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_admin____operator_removed_and_reloaded____refused_from_then_on() {
    let mut r = Remote::start().await;
    let before = r.client(&r.alice, r.node_key.clone()).list().await;

    operators(&r.operators, &[]);
    r.node.node().reload_tls().await.unwrap();
    let after = r.client(&r.alice, r.node_key.clone()).list().await;

    assert!(before.is_ok());
    assert!(after.is_err(), "{after:?}");
    r.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell____connect_to_a_contact____commands_go_to_that_node() {
    use tokio::io::AsyncWriteExt;

    let mut r = Remote::start().await;
    let id = r.node.client.members().await.unwrap()[0].node;
    let contacts = r.keys.path().join("contacts.toml");
    std::fs::write(
        &contacts,
        format!(
            "key = {:?}\n[[node]]\nname = \"alpha\"\nid = {id}\naddress = \"{}\"\nnode_key = \"{}\"\n",
            r.alice, r.address, r.node_key
        ),
    )
    .unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_bulkhead"))
        .args(["--socket", "/nonexistent/admin.sock", "shell", "--contacts"])
        .arg(&contacts)
        .env("XDG_CONFIG_HOME", r.keys.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();

    stdin
        .write_all(b"ls\ncontacts\nconnect alpha\nmembers\nstop nothing-here\n")
        .await
        .unwrap();
    drop(stdin);
    let out = child.wait_with_output().await.unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("cannot reach the node"),
        "local first: {stderr}"
    );
    assert!(stdout.contains(&format!("alpha  {id}")), "{stdout}");
    assert!(stdout.contains("connected to alpha"), "{stdout}");
    assert!(stdout.contains("CONTACT"), "{stdout}");
    let audited = r.node.audit();
    assert!(
        audited
            .iter()
            .any(|a| a["event"] == "remote_admin" && a["operator"] == "alice"),
        "{audited:?}"
    );
    r.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_admin____listed_operator_without_a_signature____401_and_audited() {
    let mut r = Remote::start().await;

    let status = r.raw(&r.alice.clone(), None).await;

    assert_eq!(status, 401);
    let refused: Vec<_> = r
        .node
        .audit()
        .into_iter()
        .filter(|a| a["event"] == "remote_admin_refused")
        .collect();
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(
        refused[0]["error"].as_str().unwrap().contains("unsigned"),
        "{refused:?}"
    );
    r.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_admin____a_signed_request_sent_twice____the_second_refused() {
    use bulkhead_keys::signature::{Signed, Signer, body_hash, now_ms};
    let mut r = Remote::start().await;
    let signer = Signer::from_file(&r.alice).unwrap();
    let header = signer.header_at(
        &Signed {
            node: &r.node_key,
            method: "GET",
            target: "/v1/apps",
            body: body_hash(b""),
        },
        now_ms(),
        [9; 16],
    );

    let first = r.raw(&r.alice.clone(), Some(&header)).await;
    let second = r.raw(&r.alice.clone(), Some(&header)).await;

    assert_eq!((first, second), (200, 401));
    r.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_admin____signed_for_another_path____refused() {
    use bulkhead_keys::signature::{Signed, Signer, body_hash, now_ms};
    let mut r = Remote::start().await;
    let signer = Signer::from_file(&r.alice).unwrap();
    let header = signer.header_at(
        &Signed {
            node: &r.node_key,
            method: "GET",
            target: "/v1/instances",
            body: body_hash(b""),
        },
        now_ms(),
        [8; 16],
    );

    let status = r.raw(&r.alice.clone(), Some(&header)).await;

    assert_eq!(status, 401);
    r.node.shutdown().await;
}

// Native apps run on Linux and FreeBSD (`bulkhead-runner`).
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_admin____deploy_a_bundle____its_body_signed_and_the_app_runs() {
    let mut r = Remote::start().await;
    let alice = r.client(&r.alice, r.node_key.clone());
    let bundle = common::app_bundle(
        &r.node.key,
        "echoer",
        "1.0.0",
        &common::native_app(),
        common::fast(serde_json::json!({})),
    );

    let deployed = alice.deploy(bundle).await;
    let apps = alice.list().await.unwrap();

    assert!(deployed.is_ok(), "{deployed:?}");
    assert!(apps.iter().any(|a| a.name == "echoer"), "{apps:?}");
    r.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_admin____driver_killed____restarted_and_operators_served_again() {
    let mut r = Remote::start().await;
    let alice = r.client(&r.alice, r.node_key.clone());
    alice.list().await.unwrap();
    let driver = r
        .node
        .client
        .ps()
        .await
        .unwrap()
        .into_iter()
        .find(|i| i.app == "bulkhead-admin")
        .unwrap()
        .pid;

    r.node.client.kill(&driver).await.unwrap();
    let mut after = alice.list().await;
    for _ in 0..100 {
        if after.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        after = alice.list().await;
    }

    assert!(after.is_ok(), "{after:?}");
    r.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_admin____the_cluster_key_as_admin_key____refused_at_start() {
    let keys = tempfile::tempdir().unwrap();
    let (node_key, _) = key(keys.path(), "node");
    let (_, alice) = key(keys.path(), "alice");
    let ops = keys.path().join("operators.toml");
    operators(&ops, &[("alice", &alice)]);
    let toml = format!(
        "data_dir = {:?}\n[admin]\naddress = \"{}\"\noperators = {ops:?}\nkey = {node_key:?}\n\
         [cluster]\nkey = {node_key:?}\n",
        keys.path().join("data"),
        free_tcp(),
    );

    let opened = bulkhead::node::Node::open(common::node_config(&toml), common::ring()).await;

    let Err(e) = opened else {
        panic!("a node opened with the cluster key as its admin key");
    };
    assert!(e.to_string().contains("cluster key"), "{e}");
}
