//! The node's remote admin API as a driver app (ROADMAP DA7): TLS 1.3
//! with raw public keys terminated outside the node.
//!
//! The node binds `[admin] address` and passes the listener (`admin`), and
//! gives the driver its admin key and the operators file under `/driver/`.
//! The driver accepts a client only if its key is an operator's, then asks
//! the node for a stream into its admin API (`Request::AdminStream`) and
//! copies bytes both ways, parsing and changing none. The node checks each
//! request's operator signature itself (`bulkhead_keys::signature`), so the
//! driver, compromised, can watch, delay or drop requests but act for no
//! operator. The node asks it to read the operators file again on SIGHUP
//! and `bulkhead reload-tls` ([`RELOAD_SCHEMA`]).

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use bulkhead_keys::{PublicKey, parse_operators};
use bulkhead_native::sender::Sender;
use bulkhead_native::sender_async::AsyncSender;
use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Reply};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

/// The name of the listener the node passes.
pub const LISTENER: &str = "admin";

/// The schema of the node's call asking the driver to read the operators
/// file again; answered with [`Reloaded`] as JSON.
pub const RELOAD_SCHEMA: &str = "bulkhead/admin-reload";

/// How long a client may take to finish the TLS handshake.
const HANDSHAKE: Duration = Duration::from_secs(10);

/// The driver's init arguments (JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The admin key's path under `/driver/`.
    pub key: String,
    /// The operators file's path under `/driver/`.
    pub operators: String,
    /// Connections open at once; more are closed on accept.
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
}

fn default_max_connections() -> usize {
    64
}

impl Config {
    /// The node's arguments.
    ///
    /// # Errors
    ///
    /// Not this format, or no connection allowed.
    pub fn from_args(args: &[u8]) -> Result<Self, String> {
        let config: Self =
            serde_json::from_slice(args).map_err(|e| format!("init arguments: {e}"))?;
        if config.max_connections == 0 {
            return Err("init arguments: max_connections must be at least 1".into());
        }
        Ok(config)
    }
}

/// The answer to a reload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reloaded {
    /// Operators listed now.
    pub operators: usize,
    /// Why the file was not read, if it was not (the old list is kept).
    pub failed: Option<String>,
}

/// Operators by name, as last read.
type Operators = Arc<RwLock<BTreeMap<String, Vec<PublicKey>>>>;

/// The admin driver.
#[derive(Debug)]
pub struct Admin {
    config: Config,
    operators: Operators,
}

fn read_operators(path: &str) -> Result<BTreeMap<String, Vec<PublicKey>>, String> {
    let bytes = bulkhead_sdk::files::read(path).map_err(|e| format!("{path}: {e}"))?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{path}: not UTF-8"))?;
    parse_operators(&text).map_err(|e| format!("{path}: {e}"))
}

impl App for Admin {
    fn init(_ctx: &mut Ctx, args: &[u8]) -> Result<Self, String> {
        let config = Config::from_args(args)?;
        let pem =
            bulkhead_sdk::files::read(&config.key).map_err(|e| format!("{}: {e}", config.key))?;
        // Read from bytes: under the helper the file is the driver's
        // group's to read, which a key file's mode check would refuse.
        let (certified, _) =
            bulkhead_keys::load_key_pem(&pem).map_err(|e| format!("{}: {e}", config.key))?;
        let operators: Operators = Arc::new(RwLock::new(read_operators(&config.operators)?));
        let listed = operators.clone();
        let accepts = Arc::new(move |key: &PublicKey| {
            listed
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .values()
                .any(|keys| keys.contains(key))
        });
        let tls = bulkhead_keys::admin_server_tls(certified, accepts).map_err(|e| e.to_string())?;
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        let socket = bulkhead_native::driver::listeners()
            .into_iter()
            .find(|(name, _)| name == LISTENER)
            .map(|(_, fd)| std::net::TcpListener::from(fd))
            .ok_or_else(|| format!("no listener named {LISTENER} passed"))?;
        let sender = bulkhead_native::app::Ctx::current()
            .sender()
            .map_err(|e| format!("sender channel: {e:?}"))?;
        let slots = config.max_connections;
        std::thread::Builder::new()
            .name("admin".into())
            .spawn(move || worker(sender, socket, acceptor, slots))
            .map_err(|e| format!("worker thread: {e}"))?;
        Ok(Self { config, operators })
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        if msg.schema.as_deref() != Some(RELOAD_SCHEMA) {
            return Reply::Now(Envelope::text("bulkhead-admin"));
        }
        let reloaded = match read_operators(&self.config.operators) {
            Ok(fresh) => {
                let n = fresh.len();
                *self
                    .operators
                    .write()
                    .unwrap_or_else(PoisonError::into_inner) = fresh;
                Reloaded {
                    operators: n,
                    failed: None,
                }
            }
            Err(e) => Reloaded {
                operators: self
                    .operators
                    .read()
                    .unwrap_or_else(PoisonError::into_inner)
                    .len(),
                failed: Some(e),
            },
        };
        match Envelope::json(&reloaded) {
            Ok(env) => Reply::Now(env),
            Err(e) => Reply::Now(Envelope::text(e.to_string())),
        }
    }
}

/// The worker thread: accepts, shakes hands, and carries each connection
/// to the node. A sender that cannot become async ends the process; the
/// node restarts it.
fn worker(
    sender: Sender,
    socket: std::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    max_connections: usize,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            bulkhead_sdk::error!("worker runtime: {e}");
            std::process::exit(70);
        }
    };
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async move {
        let sender = match AsyncSender::new(sender) {
            Ok(s) => s,
            Err(e) => {
                bulkhead_sdk::error!("worker sender: {e}");
                std::process::exit(70);
            }
        };
        tokio::task::spawn_local(sender.clone().pump());
        let listener = match socket
            .set_nonblocking(true)
            .and_then(|()| tokio::net::TcpListener::from_std(socket))
        {
            Ok(l) => l,
            Err(e) => {
                bulkhead_sdk::error!("listener: {e}");
                std::process::exit(70);
            }
        };
        let slots = Arc::new(Semaphore::new(max_connections));
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(e) => {
                    bulkhead_sdk::warn!("accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                continue;
            };
            let (acceptor, sender) = (acceptor.clone(), sender.clone());
            tokio::task::spawn_local(async move {
                let _ = carry(stream, peer, &acceptor, &sender).await;
                drop(permit);
            });
        }
    });
}

/// One connection: the handshake within [`HANDSHAKE`], then its bytes
/// carried to and from a stream into the node's admin API until either
/// side closes.
async fn carry(
    stream: tokio::net::TcpStream,
    peer: std::net::SocketAddr,
    acceptor: &tokio_rustls::TlsAcceptor,
    sender: &AsyncSender,
) -> Result<(), Infallible> {
    let mut tls = match tokio::time::timeout(HANDSHAKE, acceptor.accept(stream)).await {
        Ok(Ok(tls)) => tls,
        Ok(Err(e)) => {
            bulkhead_sdk::info!("{peer}: handshake refused: {e}");
            return Ok(());
        }
        Err(_) => {
            bulkhead_sdk::info!("{peer}: handshake timed out");
            return Ok(());
        }
    };
    let node = match sender.admin_stream().await {
        Ok(fd) => fd,
        Err(e) => {
            bulkhead_sdk::warn!("{peer}: no stream into the node: {e:?}");
            return Ok(());
        }
    };
    let node = std::os::unix::net::UnixStream::from(node);
    let mut node = match node
        .set_nonblocking(true)
        .and_then(|()| tokio::net::UnixStream::from_std(node))
    {
        Ok(s) => s,
        Err(e) => {
            bulkhead_sdk::warn!("{peer}: the node's stream: {e}");
            return Ok(());
        }
    };
    if let Err(e) = tokio::io::copy_bidirectional(&mut tls, &mut node).await {
        bulkhead_sdk::debug!("{peer}: connection ended: {e}");
    }
    Ok(())
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
