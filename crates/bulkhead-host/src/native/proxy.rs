//! Egress through an HTTP proxy (ROADMAP DA4b): the node, not the driver,
//! asks the proxy for a tunnel to the destination it has allowed, so a
//! driver cannot reach anywhere else through the proxy. The node writes
//! `CONNECT host:port` and reads the proxy's reply head alone, at most
//! [`MAX_HEAD`] bytes, one at a time, so not a byte of the tunnel is read;
//! its status line decides, and the rest is not looked at.

use std::os::fd::OwnedFd;

use bulkhead_native::protocol::NetError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The longest reply head read from the proxy.
pub(crate) const MAX_HEAD: usize = 8 << 10;

/// `host:port` as a `CONNECT` target: an IPv6 address in brackets.
pub(crate) fn authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// The `CONNECT` request for `host` and `port`.
pub(crate) fn request(host: &str, port: u16) -> String {
    let target = authority(host, port);
    format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n")
}

/// Whether the proxy's reply head (up to its blank line) grants the
/// tunnel: a `2xx` status. Its status line as the refusal otherwise.
///
/// # Errors
///
/// Not an HTTP status line, or not `2xx`.
pub(crate) fn check_reply(head: &[u8]) -> Result<(), String> {
    let line = head.split(|b| *b == b'\n').next().unwrap_or_default();
    let line = String::from_utf8_lossy(line).trim_end().to_string();
    let mut words = line.split(' ');
    let version = words.next().unwrap_or_default();
    let status = words.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") || status.len() != 3 {
        return Err(format!("not an HTTP reply: {line:?}"));
    }
    if status.starts_with('2') {
        Ok(())
    } else {
        Err(line)
    }
}

/// Ask the proxy on `stream` (blocking, as `first_to_accept` gives it) for
/// a tunnel to `host` and `port`, before `deadline`; the stream, blocking
/// again, once the proxy grants it.
///
/// # Errors
///
/// The proxy refused, answered something else or too much, closed, or
/// took past the deadline.
pub(crate) async fn tunnel(
    stream: OwnedFd,
    host: &str,
    port: u16,
    deadline: tokio::time::Instant,
) -> Result<OwnedFd, NetError> {
    let other = |e: &dyn std::fmt::Display| NetError::Other(format!("the egress proxy: {e}"));
    let std_stream = std::net::TcpStream::from(stream);
    std_stream.set_nonblocking(true).map_err(|e| other(&e))?;
    let mut s = tokio::net::TcpStream::from_std(std_stream).map_err(|e| other(&e))?;
    let exchange = async {
        s.write_all(request(host, port).as_bytes()).await?;
        let mut head = Vec::with_capacity(256);
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if head.len() >= MAX_HEAD {
                return Err(std::io::Error::other("its reply head is over 8 KiB"));
            }
            if s.read(&mut byte).await? == 0 {
                return Err(std::io::Error::other("it closed the connection"));
            }
            head.push(byte[0]);
        }
        Ok(head)
    };
    let head = tokio::time::timeout_at(deadline, exchange)
        .await
        .map_err(|_| other(&"no reply in time"))?
        .map_err(|e| other(&e))?;
    check_reply(&head).map_err(|why| other(&format!("refused: {why}")))?;
    let std_stream = s.into_std().map_err(|e| other(&e))?;
    std_stream.set_nonblocking(false).map_err(|e| other(&e))?;
    Ok(std_stream.into())
}

#[cfg(test)]
#[path = "proxy_tests.rs"]
mod proxy_tests;
