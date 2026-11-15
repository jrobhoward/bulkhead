//! The datasource's pool: deadpool over connections whose streams the node
//! opens (`Request::Connect`), each logged in with
//! `tokio_postgres::Config::connect_raw`, TLS through
//! tokio-postgres-rustls when required.
//!
//! The node's stream is the one thing the driver connects with; it opens
//! nothing itself. Every connection the pool hands out again is first
//! reset with `ROLLBACK; DISCARD ALL`, so a transaction left open, or a
//! session setting, never carries over to the next statement, which may
//! be another app's.

use std::io;
use std::os::fd::OwnedFd;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bulkhead_native::protocol::SqlError;
use deadpool::managed::{self, Metrics, PoolError, RecycleError, RecycleResult};
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use tokio_postgres::config::SslMode;
use tokio_postgres::tls::MakeTlsConnect;
use tokio_postgres::{Client, NoTls};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::config::{Config, Transport};
use crate::statements::sql_error;

/// A stream the node opened, asked for through [`Connector`].
pub type Opened = Result<OwnedFd, String>;

/// Where the pool asks for a stream: a task holding the driver's sender
/// channel, which the pool's `Send` futures cannot hold themselves.
pub type Connector = mpsc::UnboundedSender<oneshot::Sender<Opened>>;

/// A stream to the server, TCP or its Unix socket.
#[derive(Debug)]
pub enum Stream {
    /// TCP.
    Tcp(tokio::net::TcpStream),
    /// A Unix socket.
    Unix(tokio::net::UnixStream),
}

impl Stream {
    /// `fd`, a connected socket of `transport`, on the current runtime.
    ///
    /// # Errors
    ///
    /// Outside a runtime, or the descriptor cannot be made non-blocking.
    pub fn from_fd(transport: Transport, fd: OwnedFd) -> io::Result<Self> {
        match transport {
            Transport::Tcp => {
                let s = std::net::TcpStream::from(fd);
                s.set_nonblocking(true)?;
                let _ = s.set_nodelay(true);
                Ok(Self::Tcp(tokio::net::TcpStream::from_std(s)?))
            }
            Transport::Unix => {
                let s = std::os::unix::net::UnixStream::from(fd);
                s.set_nonblocking(true)?;
                Ok(Self::Unix(tokio::net::UnixStream::from_std(s)?))
            }
        }
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            Self::Unix(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            Self::Unix(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_flush(cx),
            Self::Unix(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            Self::Unix(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// The pool.
pub type Pool = managed::Pool<Manager>;

/// A connection from the pool.
pub type Conn = managed::Object<Manager>;

/// Opens and resets the pool's connections.
pub struct Manager {
    connector: Connector,
    pg: tokio_postgres::Config,
    transport: Transport,
    tls: Option<MakeRustlsConnect>,
    host: String,
}

impl std::fmt::Debug for Manager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Manager")
            .field("transport", &self.transport)
            .field("host", &self.host)
            .field("tls", &self.tls.is_some())
            .finish_non_exhaustive()
    }
}

impl Manager {
    /// A stream from the node.
    async fn stream(&self) -> Result<Stream, SqlError> {
        let (tx, rx) = oneshot::channel();
        self.connector
            .send(tx)
            .map_err(|_| SqlError::Unavailable("the driver's sender stopped".into()))?;
        let fd = rx
            .await
            .map_err(|_| SqlError::Unavailable("the driver's sender stopped".into()))?
            .map_err(SqlError::Unavailable)?;
        Stream::from_fd(self.transport, fd).map_err(|e| SqlError::Unavailable(e.to_string()))
    }
}

impl managed::Manager for Manager {
    type Type = Client;
    type Error = SqlError;

    async fn create(&self) -> Result<Client, SqlError> {
        let stream = self.stream().await?;
        let client = match &self.tls {
            None => {
                let (client, connection) = self
                    .pg
                    .connect_raw(stream, NoTls)
                    .await
                    .map_err(|e| sql_error(&e))?;
                tokio::spawn(connection);
                client
            }
            Some(make) => {
                let tls = MakeTlsConnect::<Stream>::make_tls_connect(&mut make.clone(), &self.host)
                    .map_err(|e| SqlError::Unavailable(format!("TLS: {e}")))?;
                let (client, connection) = self
                    .pg
                    .connect_raw(stream, tls)
                    .await
                    .map_err(|e| sql_error(&e))?;
                tokio::spawn(connection);
                client
            }
        };
        Ok(client)
    }

    async fn recycle(&self, client: &mut Client, _: &Metrics) -> RecycleResult<SqlError> {
        if client.is_closed() {
            return Err(RecycleError::message("the connection closed"));
        }
        client
            .batch_execute("ROLLBACK; DISCARD ALL")
            .await
            .map_err(|e| RecycleError::Backend(sql_error(&e)))
    }
}

/// The pool for `config`, `password` given to the server if it asks, TLS
/// checked against `ca` (PEM) when required, streams asked of `connector`.
///
/// # Errors
///
/// The CA certificates cannot be read, or the pool cannot be built.
pub fn build(
    config: &Config,
    password: Option<Vec<u8>>,
    ca: Option<&[u8]>,
    connector: Connector,
) -> Result<Pool, String> {
    let mut pg = tokio_postgres::Config::new();
    pg.dbname(&config.dbname)
        .user(&config.user)
        .application_name("bulkhead")
        .options(format!(
            "-c statement_timeout={}",
            config.statement_timeout_ms
        ));
    if let Some(password) = password {
        pg.password(password);
    }
    let tls = match ca {
        None => {
            pg.ssl_mode(SslMode::Disable);
            None
        }
        Some(pem) => {
            pg.ssl_mode(SslMode::Require);
            Some(MakeRustlsConnect::new(tls_config(pem)?))
        }
    };
    let manager = Manager {
        connector,
        pg,
        transport: config.transport,
        tls,
        host: config.host.clone(),
    };
    let timeouts = managed::Timeouts {
        wait: Some(config.acquire_timeout()),
        create: Some(config.connect_timeout()),
        recycle: Some(config.connect_timeout()),
    };
    Pool::builder(manager)
        .max_size(config.pool_size)
        .timeouts(timeouts)
        .runtime(deadpool::Runtime::Tokio1)
        .build()
        .map_err(|e| format!("pool: {e}"))
}

/// A connection, within the acquire timeout.
///
/// # Errors
///
/// None free in time (`limit-reached`), or a new one could not be opened.
pub async fn get(pool: &Pool) -> Result<Conn, SqlError> {
    pool.get().await.map_err(|e| match e {
        PoolError::Timeout(managed::TimeoutType::Wait) => SqlError::LimitReached,
        PoolError::Timeout(_) => SqlError::Unavailable("connecting timed out".into()),
        PoolError::Backend(e) => e,
        PoolError::Closed => SqlError::Unavailable("the pool is closed".into()),
        other => SqlError::Unavailable(format!("{other:?}")),
    })
}

/// rustls's configuration trusting the certificates in `pem`.
///
/// # Errors
///
/// Not PEM certificates, or none.
pub fn tls_config(pem: &[u8]) -> Result<rustls::ClientConfig, String> {
    let fail = |e: &dyn std::fmt::Display| format!("CA file: {e}");
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(pem) {
        roots
            .add(cert.map_err(|e| fail(&e))?)
            .map_err(|e| fail(&e))?;
    }
    if roots.is_empty() {
        return Err(fail(&"no certificates"));
    }
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| fail(&e))?
    .with_root_certificates(roots)
    .with_no_client_auth())
}
