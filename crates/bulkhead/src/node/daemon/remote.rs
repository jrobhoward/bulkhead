//! The admin API for operators on other machines: the same routes as the
//! admin socket, over TCP with TLS 1.3 and pinned keys.
//!
//! Since ROADMAP DA7 the node terminates no TLS for it: the driver
//! `bulkhead-admin`, a native app of the node's own (`ADMIN_APP`), holds
//! the admin key (`[admin] key`, a key of its own, never the cluster key)
//! and the operators file, accepts only an operator's key, and carries each
//! connection to the node through a stream into its admin API
//! (`Request::AdminStream`). Each request on it must carry the operator's
//! signature (`Bulkhead-Auth`, `bulkhead_keys::signature`), which the node
//! checks itself, with its own reading of the operators file, so the
//! driver cannot act for an operator. Each request that changes something
//! (anything but `GET`) is written to the audit log with the name its
//! signature proves, and each refused one too. The operators file is read
//! again, by the node and the driver, on SIGHUP and `bulkhead reload-tls`;
//! connections already open keep going.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use bulkhead_bundle::manifest::NativeProgram;
use bulkhead_host::transport::quic::{PublicKey, parse_operators};
use bulkhead_host::{AppDeployment, DriverListener, DriverProfile, NativeCode};
use bulkhead_keys::signature::{self, Nonces, Signed};
use http_body_util::{BodyExt, Full, Limited};
use serde_json::json;

use super::{Node, builtin};
use crate::node::config::{ADMIN_APP, NodeConfig};
use crate::node::error::NodeError;

/// The schema of the node's call asking the driver to read the operators
/// file again (`bulkhead_admin::RELOAD_SCHEMA`).
const RELOAD_SCHEMA: &str = "bulkhead/admin-reload";

/// How long the node waits for the driver's answer to a reload.
const RELOAD_TIMEOUT: Duration = Duration::from_secs(10);

/// Operators by name, as last read.
pub(crate) type Operators = Arc<RwLock<BTreeMap<String, Vec<PublicKey>>>>;

/// Read the operators file.
pub(crate) fn read_operators(path: &Path) -> Result<BTreeMap<String, Vec<PublicKey>>, NodeError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| NodeError::Invalid(format!("admin operators {}: {e}", path.display())))?;
    parse_operators(&text)
        .map_err(|e| NodeError::Invalid(format!("admin operators {}: {e}", path.display())))
}

/// The guest path of `file` under `/driver/`, by its name.
fn under_driver(file: &Path) -> String {
    format!(
        "/driver/{}",
        file.file_name().unwrap_or_default().to_string_lossy()
    )
}

/// `[admin] key`: required, and not the cluster key, since the driver
/// holds it.
fn admin_key(config: &NodeConfig) -> Result<&PathBuf, NodeError> {
    let admin = config
        .admin
        .as_ref()
        .ok_or_else(|| NodeError::Internal("no [admin]".into()))?;
    admin.key.as_ref().ok_or_else(|| {
        NodeError::Invalid(
            "[admin] needs a key of its own (key = \"...\", made with `bulkhead cluster \
             keygen`): the admin driver holds it, and the cluster key stays in the node"
                .into(),
        )
    })
}

/// The public key of the private key in `path`, read without the file
/// mode's check: under the helper the admin key is the driver's group's
/// to read.
fn public_of(path: &Path) -> Result<PublicKey, NodeError> {
    let pem = std::fs::read(path)
        .map_err(|e| NodeError::Invalid(format!("[admin] key {}: {e}", path.display())))?;
    bulkhead_keys::load_key_pem(&pem)
        .map(|(_, public)| public)
        .map_err(|e| NodeError::Invalid(format!("[admin] key {}: {e}", path.display())))
}

/// `[admin]` bound for the driver: its listener, the key and operators
/// files, and the right to open admin streams; `None` without `[admin]`.
///
/// # Errors
///
/// No `[admin] key`, the key the cluster's, the two files of one name, or
/// the address not bound.
pub(super) fn bind(config: &NodeConfig) -> Result<Option<DriverProfile>, NodeError> {
    let Some(admin) = &config.admin else {
        return Ok(None);
    };
    let key = admin_key(config)?;
    if let Some(cluster) = &config.cluster.key
        && (cluster == key || public_of(cluster).ok() == Some(public_of(key)?))
    {
        return Err(NodeError::Invalid(
            "[admin] key is the cluster key: the admin driver holds it, so it must be a key of \
             its own"
                .into(),
        ));
    }
    if key.file_name() == admin.operators.file_name() {
        return Err(NodeError::Invalid(
            "[admin] key and operators have the same file name; the driver reads each by its \
             file name"
                .into(),
        ));
    }
    let socket = std::net::TcpListener::bind(admin.address)
        .map_err(|e| NodeError::Invalid(format!("admin: {}: {e}", admin.address)))?;
    Ok(Some(DriverProfile {
        listeners: vec![DriverListener {
            name: "admin".into(),
            socket: Arc::new(socket),
        }],
        files: vec![key.clone(), admin.operators.clone()],
        admin: true,
        ..DriverProfile::default()
    }))
}

/// The driver's manifest: one instance, its files by guest path, restarts
/// that never give up.
fn manifest(config: &NodeConfig) -> Result<bulkhead_bundle::manifest::AppManifest, NodeError> {
    let admin = config
        .admin
        .as_ref()
        .ok_or_else(|| NodeError::Internal("no [admin]".into()))?;
    let spec = json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "instances": { "count": 1 },
        "lifecycle": { "start_args": {
            "key": under_driver(admin_key(config)?),
            "operators": under_driver(&admin.operators),
        } },
        "supervision": builtin::supervision(),
        "limits": { "memory_max": "64MiB" },
        "messaging_acl": { "accept_from": [ADMIN_APP], "may_call": [ADMIN_APP] },
    });
    builtin::manifest(ADMIN_APP, "The node's remote admin API", &spec)
}

/// The node's server for the driver's streams, kept for the node's life
/// while the runtime holds it weakly.
pub(super) struct KeptAdminStreams(
    #[allow(dead_code, reason = "held, never read: it keeps the closure alive")]
    bulkhead_host::AdminStreams,
);

impl std::fmt::Debug for KeptAdminStreams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeptAdminStreams")
    }
}

impl Node {
    /// Read the operators file again, if there is a remote admin API: the
    /// node's own reading, and the driver's.
    pub(super) async fn reload_operators(&self) -> Result<(), NodeError> {
        let Some(admin) = &self.config.admin else {
            return Ok(());
        };
        let fresh = read_operators(&admin.operators)?;
        *self
            .operators
            .write()
            .unwrap_or_else(PoisonError::into_inner) = fresh;
        let Some(driver) = self.apps.app(ADMIN_APP) else {
            return Ok(());
        };
        let msg = bulkhead_host::Envelope {
            content_type: "application/json".into(),
            schema: Some(RELOAD_SCHEMA.into()),
            body: b"{}".to_vec(),
        };
        let reply = driver
            .call(msg, RELOAD_TIMEOUT)
            .await
            .map_err(|e| NodeError::Invalid(format!("admin: the driver did not answer: {e}")))?;
        let answer: serde_json::Value = serde_json::from_slice(&reply.body)
            .map_err(|e| NodeError::Invalid(format!("admin: the driver's answer: {e}")))?;
        match answer.get("failed").and_then(serde_json::Value::as_str) {
            Some(why) => Err(NodeError::Invalid(format!("admin: the driver: {why}"))),
            None => Ok(()),
        }
    }

    /// Start the remote admin API on `[admin] address`, if configured: the
    /// node serves the streams the driver opens into it, then starts the
    /// driver. It stops with the node.
    ///
    /// # Errors
    ///
    /// [`NodeError::Invalid`] if the key or operators file cannot be read,
    /// or the driver did not start.
    pub async fn start_remote_admin(self: &Arc<Self>) -> Result<(), NodeError> {
        if self.config.admin.is_none() {
            return Ok(());
        }
        let fresh = read_operators(
            &self
                .config
                .admin
                .as_ref()
                .ok_or_else(|| NodeError::Internal("no [admin]".into()))?
                .operators,
        )?;
        *self
            .operators
            .write()
            .unwrap_or_else(PoisonError::into_inner) = fresh;
        let auth = Arc::new(Auth {
            node_key: public_of(admin_key(&self.config)?)?,
            nonces: Nonces::default(),
        });
        // Weak: the node keeps this closure, which must not keep the node.
        let node = Arc::downgrade(self);
        let serve: bulkhead_host::AdminStreams = Arc::new(move |stream| {
            let (Some(node), auth) = (node.upgrade(), auth.clone()) else {
                return;
            };
            tokio::spawn(async move {
                let stream = match stream
                    .set_nonblocking(true)
                    .and_then(|()| tokio::net::UnixStream::from_std(stream))
                {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "an admin stream not served");
                        return;
                    }
                };
                let service = hyper::service::service_fn(move |req| {
                    let (node, auth) = (node.clone(), auth.clone());
                    async move {
                        Ok::<_, std::convert::Infallible>(handle_signed(&node, &auth, req).await)
                    }
                });
                if let Err(e) = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .await
                {
                    tracing::debug!(error = %e, "remote admin connection ended");
                }
            });
        });
        self.rt.set_admin_streams(&serve);
        let _ = self.admin_streams.set(KeptAdminStreams(serve));
        let program = builtin::program(&self.config, self.config.native.admin.as_ref(), ADMIN_APP);
        if !program.is_file() {
            return Err(NodeError::Invalid(format!(
                "[admin] needs {ADMIN_APP}, and {} is not it (install it beside \
                 bulkhead-runner, or set [native] admin)",
                program.display()
            )));
        }
        let code = self.rt.load_native(NativeCode {
            path: program,
            kind: NativeProgram::Executable,
        });
        let deployment = AppDeployment::new(manifest(&self.config)?, code);
        builtin::start(&self.rt, &self.apps, deployment).await?;
        tracing::info!("remote admin API driver started");
        Ok(())
    }
}

/// What a remote request's signature is checked against, beside the
/// operators file.
pub(crate) struct Auth {
    /// This node's admin public key, which operators pin and sign for.
    pub(crate) node_key: PublicKey,
    /// Nonces already used.
    pub(crate) nonces: Nonces,
}

/// Serve one remote request: its body read whole (as the admin API reads
/// bodies anyway), its signature checked against it, then handled as the
/// admin socket handles it. A request without a valid signature is
/// answered 401 and recorded in the audit log.
pub(crate) async fn handle_signed(
    node: &Node,
    auth: &Auth,
    req: hyper::Request<hyper::body::Incoming>,
) -> hyper::Response<http_body_util::combinators::BoxBody<bytes::Bytes, std::convert::Infallible>> {
    let (parts, body) = req.into_parts();
    let bytes = match Limited::new(body, crate::node::server::MAX_BODY)
        .collect()
        .await
    {
        Ok(c) => c.to_bytes(),
        Err(e) => {
            return crate::node::server::error(&NodeError::Invalid(format!("request body: {e}")));
        }
    };
    let target = parts
        .uri
        .path_and_query()
        .map_or_else(|| parts.uri.path().to_string(), ToString::to_string);
    let header = parts
        .headers
        .get(signature::HEADER)
        .and_then(|v| v.to_str().ok());
    let signed = Signed {
        node: &auth.node_key,
        method: parts.method.as_str(),
        target: &target,
        body: signature::body_hash(&bytes),
    };
    let operators = node
        .operators
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    match signature::verify(
        header,
        &signed,
        signature::now_ms(),
        &operators,
        &auth.nonces,
    ) {
        Err(e) => {
            tracing::warn!(method = %parts.method, path = %target, error = %e, "remote admin request refused");
            node.audit.record(
                "remote_admin_refused",
                json!({ "method": parts.method.as_str(), "path": target, "error": e.to_string() }),
            );
            crate::node::server::error(&NodeError::Unauthorized(e.to_string()))
        }
        Ok(verified) => {
            if parts.method != hyper::Method::GET {
                node.audit.record(
                    "remote_admin",
                    json!({
                        "operator": verified.operator,
                        "method": parts.method.as_str(),
                        "path": parts.uri.path(),
                    }),
                );
            }
            crate::node::server::handle(node, hyper::Request::from_parts(parts, Full::new(bytes)))
                .await
        }
    }
}
