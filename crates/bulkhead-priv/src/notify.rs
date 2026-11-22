//! systemd's notification socket (`sd_notify`, ROADMAP B6): with
//! `WatchdogSec=` and `NotifyAccess=main` in the unit, the helper sends
//! `WATCHDOG=1` each time the node says it is alive, so systemd restarts
//! the service when the helper itself stops answering. The socket is
//! opened before the helper fences itself; without `NOTIFY_SOCKET` (rc on
//! FreeBSD, a test) nothing is sent.

use std::os::unix::net::UnixDatagram;

/// The socket systemd named, and an unbound datagram socket to send on.
#[derive(Debug)]
pub struct Notify {
    socket: UnixDatagram,
    to: Target,
}

#[derive(Debug)]
enum Target {
    Path(std::path::PathBuf),
    #[cfg(target_os = "linux")]
    Abstract(Vec<u8>),
}

impl Notify {
    /// From `NOTIFY_SOCKET`, if set: a path, or on Linux an abstract name
    /// (`@name`).
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let name = std::env::var_os("NOTIFY_SOCKET")?;
        let bytes = name.as_encoded_bytes();
        let to = match bytes.first()? {
            b'/' => Target::Path(name.clone().into()),
            #[cfg(target_os = "linux")]
            b'@' => Target::Abstract(bytes[1..].to_vec()),
            _ => return None,
        };
        let socket = UnixDatagram::unbound().ok()?;
        Some(Self { socket, to })
    }

    /// Send `message` (`WATCHDOG=1`); a failure is ignored, as systemd's
    /// own client does.
    pub fn send(&self, message: &str) {
        match &self.to {
            Target::Path(path) => {
                let _ = self.socket.send_to(message.as_bytes(), path);
            }
            #[cfg(target_os = "linux")]
            Target::Abstract(name) => {
                use std::os::linux::net::SocketAddrExt;
                if let Ok(addr) = std::os::unix::net::SocketAddr::from_abstract_name(name) {
                    let _ = self.socket.send_to_addr(message.as_bytes(), &addr);
                }
            }
        }
    }
}
