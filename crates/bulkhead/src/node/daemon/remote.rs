//! The admin API for operators on other machines: the same
//! routes as the admin socket, over TCP with TLS 1.3 and pinned keys.
//!
//! The node presents its key (`[admin] key`, or `[cluster] key`); an
//! operator connects only with a key the operators file lists, and has
//! every right the admin socket gives. Each request must also carry the
//! operator's signature (`Bulkhead-Auth`, `bulkhead_keys::signature`),
//! which the node checks itself, with its own reading of the operators
//! file: whatever terminates TLS (since ROADMAP DA7 a driver) can then not
//! act for an operator. Each connection is logged with the operator's
//! name, each request that changes something (anything but `GET`) is
//! written to the audit log with the name its signature proves, and each
//! refused one too. The operators file is read again on SIGHUP and
//! `bulkhead reload-tls`; connections already open keep going.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use bulkhead_host::transport::quic::{PublicKey, admin_server_tls, parse_operators, presented_key};
use bulkhead_keys::signature::{self, Nonces, Signed};
use http_body_util::{BodyExt, Full, Limited};
use serde_json::json;

use super::Node;
use crate::node::error::NodeError;

/// How long a client may take to finish the TLS handshake.
const HANDSHAKE: Duration = Duration::from_secs(10);

/// Operators by name, as last read.
pub(crate) type Operators = Arc<RwLock<BTreeMap<String, Vec<PublicKey>>>>;

/// Read the operators file.
pub(crate) fn read_operators(path: &Path) -> Result<BTreeMap<String, Vec<PublicKey>>, NodeError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| NodeError::Invalid(format!("admin operators {}: {e}", path.display())))?;
    parse_operators(&text)
        .map_err(|e| NodeError::Invalid(format!("admin operators {}: {e}", path.display())))
}

/// The name of the operator whose key `key` is.
fn operator_of(operators: &Operators, key: &PublicKey) -> Option<String> {
    operators
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .find(|(_, keys)| keys.contains(key))
        .map(|(name, _)| name.clone())
}

impl Node {
    /// Read the operators file again, if there is a remote admin API.
    pub(super) fn reload_operators(&self) -> Result<(), NodeError> {
        let Some(admin) = &self.config.admin else {
            return Ok(());
        };
        let fresh = read_operators(&admin.operators)?;
        *self
            .operators
            .write()
            .unwrap_or_else(PoisonError::into_inner) = fresh;
        Ok(())
    }

    /// Start the remote admin API on `[admin] address`, if configured; it
    /// stops with the node.
    ///
    /// # Errors
    ///
    /// [`NodeError::Invalid`] if the key or operators file cannot be read,
    /// or the address cannot be bound.
    pub async fn start_remote_admin(self: &Arc<Self>) -> Result<(), NodeError> {
        let Some(admin) = &self.config.admin else {
            return Ok(());
        };
        let key = admin
            .key
            .as_ref()
            .or(self.config.cluster.key.as_ref())
            .ok_or_else(|| NodeError::Invalid("[admin] needs a key, or [cluster] key".into()))?;
        self.reload_operators()?;
        let operators = self.operators.clone();
        let accepts = Arc::new(move |k: &PublicKey| operator_of(&operators, k).is_some());
        let (certified, public) = bulkhead_keys::load_key(key)
            .map_err(|e| NodeError::Invalid(format!("[admin] key: {e}")))?;
        let tls =
            admin_server_tls(certified, accepts).map_err(|e| NodeError::Invalid(e.to_string()))?;
        let auth = Arc::new(Auth {
            node_key: public,
            nonces: Nonces::default(),
        });
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        let listener = tokio::net::TcpListener::bind(admin.address)
            .await
            .map_err(|e| NodeError::Invalid(format!("admin: {}: {e}", admin.address)))?;
        tracing::info!(address = %admin.address, "remote admin API listening");
        tokio::spawn(serve(self.clone(), listener, acceptor, auth));
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

async fn serve(
    node: Arc<Node>,
    listener: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    auth: Arc<Auth>,
) {
    let mut stop = node.stop.subscribe();
    loop {
        let (stream, peer) = tokio::select! {
            _ = stop.wait_for(|s| *s) => return,
            accepted = listener.accept() => match accepted {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(error = %e, "remote admin accept failed");
                    continue;
                }
            },
        };
        let (node, acceptor, auth) = (node.clone(), acceptor.clone(), auth.clone());
        tokio::spawn(async move {
            let tls = match tokio::time::timeout(HANDSHAKE, acceptor.accept(stream)).await {
                Ok(Ok(tls)) => tls,
                Ok(Err(e)) => {
                    tracing::info!(%peer, error = %e, "remote admin handshake refused");
                    return;
                }
                Err(_) => {
                    tracing::info!(%peer, "remote admin handshake timed out");
                    return;
                }
            };
            let key = presented_key(tls.get_ref().1.peer_certificates());
            let Some(operator) = key.and_then(|k| operator_of(&node.operators, &k)) else {
                // The verifier accepted the key, but a reload removed it since.
                return;
            };
            tracing::info!(%peer, operator, "remote admin connection");
            let service = hyper::service::service_fn(move |req: hyper::Request<_>| {
                let (node, auth) = (node.clone(), auth.clone());
                async move { Ok::<_, std::convert::Infallible>(handle_signed(&node, &auth, req).await) }
            });
            let io = hyper_util::rt::TokioIo::new(tls);
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await
            {
                tracing::debug!(error = %e, "remote admin connection ended");
            }
        });
    }
}
