//! hickory's I/O over streams the node opens. The driver cannot make a
//! socket (the runner's filter refuses `socket` and `connect`): when
//! hickory wants a connection to a nameserver, [`NodeStreams`] asks the
//! worker, which asks the callback thread, which asks the node, whose
//! connect rules allow the nameservers alone. UDP is refused here: hickory
//! is configured for TCP only, and a UDP socket the node passed could send
//! anywhere.
//!
//! The callback thread keeps the SDK's stream, by which the node counts
//! and closes it; hickory gets a duplicate. When hickory drops its
//! [`Stream`], the stream's key goes back so the callback thread drops
//! the SDK's too and the node closes the connection.
//!
//! [`OneStream`] keeps one connection per nameserver, its queries
//! multiplexed: hickory opens a connection per lookup that finds none
//! established, so a burst of first lookups would otherwise ask the node
//! for a stream each, past the driver's `max_sockets`.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_io::{AsyncRead, AsyncWrite};
use futures_util::FutureExt;
use futures_util::future::Shared;
use hickory_resolver::config::ConnectionConfig;
use hickory_resolver::net::NetError;
use hickory_resolver::net::runtime::iocompat::AsyncIoTokioAsStd;
use hickory_resolver::net::runtime::{DnsTcpStream, RuntimeProvider, TokioHandle, TokioTime};
use hickory_resolver::net::xfer::DnsExchange;
use hickory_resolver::{ConnectionProvider, PoolContext};
use tokio::sync::{mpsc, oneshot};

/// How long a stream may take when hickory names no limit.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// A stream the node opened, for hickory.
#[derive(Debug)]
pub struct Opened {
    /// Its key at the callback thread, which holds the SDK's stream.
    pub key: u64,
    /// A duplicate of the stream's descriptor.
    pub stream: std::net::TcpStream,
}

/// hickory wants a stream to `addr`.
#[derive(Debug)]
pub struct Wanted {
    /// The nameserver.
    pub addr: SocketAddr,
    /// Where the stream goes, or why there is none.
    pub reply: oneshot::Sender<io::Result<Opened>>,
}

/// hickory's runtime: tokio, with streams from the node.
#[derive(Clone)]
pub struct NodeStreams {
    handle: TokioHandle,
    wanted: mpsc::UnboundedSender<Wanted>,
    closed: mpsc::UnboundedSender<u64>,
    /// Whether the last stream to each nameserver is still open.
    live: Arc<Mutex<HashMap<SocketAddr, Arc<AtomicBool>>>>,
}

impl std::fmt::Debug for NodeStreams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeStreams").finish_non_exhaustive()
    }
}

impl NodeStreams {
    /// Streams asked for on `wanted`; each one's key sent on `closed` when
    /// hickory drops it.
    #[must_use]
    pub fn new(wanted: mpsc::UnboundedSender<Wanted>, closed: mpsc::UnboundedSender<u64>) -> Self {
        Self {
            handle: TokioHandle::default(),
            wanted,
            closed,
            live: Arc::default(),
        }
    }

    /// Whether the last stream opened to `addr` is still open.
    fn is_live(&self, addr: SocketAddr) -> bool {
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&addr)
            .is_some_and(|alive| alive.load(Ordering::SeqCst))
    }
}

impl RuntimeProvider for NodeStreams {
    type Handle = TokioHandle;
    type Timer = TokioTime;
    type Udp = tokio::net::UdpSocket;
    type Tcp = Stream;

    fn create_handle(&self) -> Self::Handle {
        self.handle.clone()
    }

    fn connect_tcp(
        &self,
        server_addr: SocketAddr,
        _bind_addr: Option<SocketAddr>,
        timeout: Option<Duration>,
    ) -> Pin<Box<dyn Send + Future<Output = Result<Self::Tcp, io::Error>>>> {
        let (wanted, closed, live) = (self.wanted.clone(), self.closed.clone(), self.live.clone());
        Box::pin(async move {
            let (reply, answer) = oneshot::channel();
            wanted
                .send(Wanted {
                    addr: server_addr,
                    reply,
                })
                .map_err(|_| io::Error::other("the driver is stopping"))?;
            let opened = tokio::time::timeout(timeout.unwrap_or(CONNECT_TIMEOUT), answer)
                .await
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::TimedOut, "no stream from the node in time")
                })?
                .map_err(|_| io::Error::other("the driver is stopping"))??;
            // From here the key goes back whatever happens.
            let Opened { key, stream } = opened;
            let alive = Arc::new(AtomicBool::new(true));
            live.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(server_addr, alive.clone());
            let guard = Closing { key, closed, alive };
            stream.set_nonblocking(true)?;
            let io = AsyncIoTokioAsStd(tokio::net::TcpStream::from_std(stream)?);
            Ok(Stream { io, guard })
        })
    }

    fn bind_udp(
        &self,
        _local_addr: SocketAddr,
        _server_addr: SocketAddr,
    ) -> Pin<Box<dyn Send + Future<Output = Result<Self::Udp, io::Error>>>> {
        Box::pin(async {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "no UDP: the node passes TCP streams only",
            ))
        })
    }
}

/// Sends a stream's key back when dropped.
#[derive(Debug)]
struct Closing {
    key: u64,
    closed: mpsc::UnboundedSender<u64>,
    alive: Arc<AtomicBool>,
}

impl Drop for Closing {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        let _ = self.closed.send(self.key);
    }
}

/// A stream to a nameserver, as hickory uses it.
pub struct Stream {
    io: AsyncIoTokioAsStd<tokio::net::TcpStream>,
    guard: Closing,
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream")
            .field("stream", &self.io.0)
            .field("key", &self.guard.key)
            .finish()
    }
}

impl Stream {
    /// Its key at the callback thread.
    #[must_use]
    pub fn key(&self) -> u64 {
        self.guard.key
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_read(cx, buf)
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_close(cx)
    }
}

impl DnsTcpStream for Stream {
    type Time = TokioTime;
}

/// A connection to a nameserver, being made or made.
type Connecting =
    Shared<Pin<Box<dyn Future<Output = Result<DnsExchange<NodeStreams>, NetError>> + Send>>>;

/// hickory's connections: one per nameserver, shared by every lookup
/// while its stream is open, a new one once it closed or failed.
#[derive(Clone)]
pub struct OneStream {
    streams: NodeStreams,
    open: Arc<Mutex<HashMap<SocketAddr, Connecting>>>,
}

impl std::fmt::Debug for OneStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OneStream").finish_non_exhaustive()
    }
}

impl OneStream {
    /// Connections over `streams`.
    #[must_use]
    pub fn new(streams: NodeStreams) -> Self {
        Self {
            streams,
            open: Arc::default(),
        }
    }
}

impl ConnectionProvider for OneStream {
    type Conn = DnsExchange<NodeStreams>;
    type FutureConn = Connecting;
    type RuntimeProvider = NodeStreams;

    fn new_connection(
        &self,
        ip: IpAddr,
        config: &ConnectionConfig,
        cx: &PoolContext,
    ) -> Result<Self::FutureConn, NetError> {
        let addr = SocketAddr::new(ip, config.port);
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        let usable = open.get(&addr).filter(|c| match c.peek() {
            None => true,
            Some(Ok(_)) => self.streams.is_live(addr),
            Some(Err(_)) => false,
        });
        if let Some(connecting) = usable {
            return Ok(connecting.clone());
        }
        let connecting =
            ConnectionProvider::new_connection(&self.streams, ip, config, cx)?.shared();
        open.insert(addr, connecting.clone());
        Ok(connecting)
    }

    fn runtime_provider(&self) -> &Self::RuntimeProvider {
        &self.streams
    }
}

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;
