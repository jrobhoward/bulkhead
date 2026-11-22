//! A wedged node restarted (ROADMAP B6). Under the privileged helper a
//! task on the node's runtime tells the helper it is alive every
//! `bulkhead_priv_wire::ALIVE_EVERY_S` seconds, and the helper kills a node
//! silent past its `node_watchdog_s`. Without the helper, under a systemd
//! unit with `WatchdogSec=` (`NOTIFY_SOCKET` and `WATCHDOG_USEC` set), the
//! same task sends systemd `WATCHDOG=1` twice a period. A task that cannot
//! run means every worker is stuck: the node is wedged.

use std::os::unix::net::UnixDatagram;
use std::time::Duration;

use tokio::sync::watch;

/// Start the task until `stop`: pings to `helper`, or to systemd's
/// watchdog when there is no helper and the unit asks for one.
pub(crate) fn start(helper: Option<bulkhead_host::PrivHelper>, mut stop: watch::Receiver<bool>) {
    let (every, ping): (Duration, Box<dyn Fn() + Send>) = match helper {
        Some(helper) => (
            Duration::from_secs(bulkhead_priv_wire::ALIVE_EVERY_S),
            Box::new(move || helper.alive()),
        ),
        None => {
            let Some((every, notify)) = systemd() else {
                return;
            };
            (every, Box::new(notify))
        }
    };
    tokio::spawn(async move {
        loop {
            ping();
            tokio::select! {
                _ = stop.wait_for(|s| *s) => return,
                () = tokio::time::sleep(every) => {}
            }
        }
    });
}

/// systemd's watchdog, if the unit set one: half its period, and a
/// function sending `WATCHDOG=1`.
fn systemd() -> Option<(Duration, impl Fn() + Send + 'static)> {
    let usec: u64 = std::env::var("WATCHDOG_USEC").ok()?.parse().ok()?;
    let socket = std::env::var_os("NOTIFY_SOCKET")?;
    let path = std::path::PathBuf::from(&socket);
    let sender = UnixDatagram::unbound().ok()?;
    #[cfg(target_os = "linux")]
    let abstract_name = socket
        .as_encoded_bytes()
        .strip_prefix(b"@")
        .map(<[u8]>::to_vec);
    let send = move || {
        #[cfg(target_os = "linux")]
        if let Some(name) = &abstract_name {
            use std::os::linux::net::SocketAddrExt;
            if let Ok(addr) = std::os::unix::net::SocketAddr::from_abstract_name(name) {
                let _ = sender.send_to_addr(b"WATCHDOG=1", &addr);
            }
            return;
        }
        let _ = sender.send_to(b"WATCHDOG=1", &path);
    };
    tracing::info!(
        every_ms = usec / 2000,
        "systemd's watchdog: pinged every half period"
    );
    Some((Duration::from_micros(usec / 2), send))
}
