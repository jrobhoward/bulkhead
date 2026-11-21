//! S43: bulk transfers between two nodes over an emulated WAN (ROADMAP
//! DR3b). See README.md.
//!
//! A client and a server quinn endpoint, as the node configures them,
//! talk through a UDP proxy that delays, jitters and drops datagrams as the
//! soak's `Mode::Wan` does. The client asks for chunks, as the DR node
//! asks for bundle and snapshot chunks, and times them; small pings
//! during a transfer measure how long other traffic waits behind it.
//!
//! `s43 [filter]` runs the cases whose name contains `filter`;
//! `TOTAL_MIB` (default 4) is each transfer's size.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use quinn::congestion::{BbrConfig, ControllerFactory, CubicConfig, NewRenoConfig};
use quinn::{Endpoint, RecvStream, SendStream, TransportConfig};
use tokio::net::UdpSocket;
use tokio::sync::oneshot;

const CHUNK: u8 = 1;
const PING: u8 = 2;
const PRIORITY: u8 = 3;

/// The soak's WAN, each way.
#[derive(Clone, Copy, Debug)]
struct Wan {
    delay_ms: u64,
    jitter_ms: u64,
    per_mille: u64,
}

impl std::fmt::Display for Wan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}+{}ms {}%",
            self.delay_ms,
            self.jitter_ms,
            self.per_mille as f64 / 10.0
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cc {
    Cubic,
    NewReno,
    Bbr,
}

impl Cc {
    fn factory(self) -> Arc<dyn ControllerFactory + Send + Sync> {
        match self {
            Cc::Cubic => Arc::new(CubicConfig::default()),
            Cc::NewReno => Arc::new(NewRenoConfig::default()),
            Cc::Bbr => Arc::new(BbrConfig::default()),
        }
    }
}

/// Where pings go during a transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pings {
    None,
    /// On the transfer's stream, behind its chunks (as today).
    SameStream,
    /// On a stream of their own; the transfer's at a lower priority.
    OwnStream,
}

#[derive(Clone, Debug)]
struct Case {
    name: String,
    wan: Wan,
    cc: Cc,
    chunk: u32,
    inflight: usize,
    pings: Pings,
}

/// Start the WAN proxy in front of `server`; the address to dial.
async fn proxy(server: SocketAddr, wan: Wan, seed: u64) -> Result<SocketAddr> {
    let public = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
    let upstream = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
    upstream.connect(server).await?;
    let addr = public.local_addr()?;
    let rng = Arc::new(Mutex::new(fastrand::Rng::with_seed(seed)));
    let client: Arc<Mutex<Option<SocketAddr>>> = Arc::default();
    // Client to server.
    {
        let (public, upstream, rng, client) =
            (public.clone(), upstream.clone(), rng.clone(), client.clone());
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65_536];
            loop {
                let Ok((n, from)) = public.recv_from(&mut buf).await else {
                    continue;
                };
                *client.lock().unwrap() = Some(from);
                let bytes = buf[..n].to_vec();
                if let Some(wait) = fate(&rng, wan) {
                    let upstream = upstream.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(wait).await;
                        let _ = upstream.send(&bytes).await;
                    });
                }
            }
        });
    }
    // Server to client.
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65_536];
        loop {
            let Ok(n) = upstream.recv(&mut buf).await else {
                continue;
            };
            let Some(to) = *client.lock().unwrap() else {
                continue;
            };
            let bytes = buf[..n].to_vec();
            if let Some(wait) = fate(&rng, wan) {
                let public = public.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(wait).await;
                    let _ = public.send_to(&bytes, to).await;
                });
            }
        }
    });
    Ok(addr)
}

/// A datagram's delay, or `None` if dropped.
fn fate(rng: &Mutex<fastrand::Rng>, wan: Wan) -> Option<Duration> {
    let mut rng = rng.lock().unwrap();
    if rng.u64(0..1000) < wan.per_mille {
        return None;
    }
    Some(Duration::from_millis(
        wan.delay_ms + rng.u64(0..=wan.jitter_ms),
    ))
}

/// The node's transport settings (`transport/quic.rs`), with `cc`.
fn transport(cc: Cc) -> Arc<TransportConfig> {
    let idle = Duration::from_secs(5);
    let mut t = TransportConfig::default();
    t.max_idle_timeout(Some(idle.try_into().unwrap()))
        .keep_alive_interval(Some(idle / 3))
        .congestion_controller_factory(cc.factory());
    Arc::new(t)
}

fn configs(cc: Cc) -> Result<(quinn::ServerConfig, quinn::ClientConfig)> {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let cert = ck.cert.der().clone();
    let key = rustls_pki_types::PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der());
    let mut server = quinn::ServerConfig::with_single_cert(vec![cert.clone()], key.into())?;
    server.transport_config(transport(cc));
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert)?;
    let mut client = quinn::ClientConfig::with_root_certificates(Arc::new(roots))?;
    client.transport_config(transport(cc));
    Ok((server, client))
}

/// Answer each stream's asks in order, as a node writes frames.
async fn serve(endpoint: Endpoint) {
    while let Some(incoming) = endpoint.accept().await {
        tokio::spawn(async move {
            let Ok(conn) = incoming.await else { return };
            while let Ok((send, recv)) = conn.accept_bi().await {
                tokio::spawn(answer(send, recv));
            }
        });
    }
}

async fn answer(mut send: SendStream, mut recv: RecvStream) {
    let mut ask = [0u8; 9];
    while recv.read_exact(&mut ask).await.is_ok() {
        let id = u32::from_be_bytes(ask[1..5].try_into().unwrap());
        let arg = u32::from_be_bytes(ask[5..9].try_into().unwrap());
        let size = match ask[0] {
            CHUNK => arg as usize,
            PING => 16,
            PRIORITY => {
                let _ = send.set_priority(arg as i32 - 1000);
                0
            }
            _ => return,
        };
        let mut frame = Vec::with_capacity(8 + size);
        frame.extend_from_slice(&(size as u32).to_be_bytes());
        frame.extend_from_slice(&id.to_be_bytes());
        frame.resize(8 + size, 0);
        if send.write_all(&frame).await.is_err() {
            return;
        }
    }
}

/// A client stream: asks written under a lock, answers matched by id.
struct Stream {
    send: tokio::sync::Mutex<SendStream>,
    waiting: Mutex<HashMap<u32, oneshot::Sender<usize>>>,
    next: Mutex<u32>,
}

impl Stream {
    async fn open(conn: &quinn::Connection) -> Result<Arc<Self>> {
        let (send, mut recv) = conn.open_bi().await?;
        let s = Arc::new(Self {
            send: tokio::sync::Mutex::new(send),
            waiting: Mutex::default(),
            next: Mutex::new(0),
        });
        let reader = Arc::downgrade(&s);
        tokio::spawn(async move {
            let mut head = [0u8; 8];
            let mut body = Vec::new();
            while recv.read_exact(&mut head).await.is_ok() {
                let size = u32::from_be_bytes(head[0..4].try_into().unwrap()) as usize;
                let id = u32::from_be_bytes(head[4..8].try_into().unwrap());
                body.resize(size, 0);
                if recv.read_exact(&mut body).await.is_err() {
                    return;
                }
                let Some(s) = reader.upgrade() else { return };
                if let Some(tx) = s.waiting.lock().unwrap().remove(&id) {
                    let _ = tx.send(size);
                }
            }
        });
        Ok(s)
    }

    async fn ask(&self, kind: u8, arg: u32) -> Result<usize> {
        let id = {
            let mut n = self.next.lock().unwrap();
            *n += 1;
            *n
        };
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().unwrap().insert(id, tx);
        let mut ask = [0u8; 9];
        ask[0] = kind;
        ask[1..5].copy_from_slice(&id.to_be_bytes());
        ask[5..9].copy_from_slice(&arg.to_be_bytes());
        self.send.lock().await.write_all(&ask).await?;
        Ok(rx.await?)
    }

    /// Set the server's side of this stream to `priority`.
    async fn priority(&self, priority: i32) -> Result<()> {
        let mut ask = [0u8; 9];
        ask[0] = PRIORITY;
        ask[5..9].copy_from_slice(&((priority + 1000) as u32).to_be_bytes());
        self.send.lock().await.write_all(&ask).await?;
        Ok(())
    }
}

struct Outcome {
    secs: f64,
    kib_s: f64,
    /// Ping round trips during the transfer, in ms.
    pings: Vec<f64>,
}

async fn run(case: &Case, total: u64, seed: u64) -> Result<Outcome> {
    let (server_cfg, client_cfg) = configs(case.cc)?;
    let server = Endpoint::server(server_cfg, "127.0.0.1:0".parse()?)?;
    let server_addr = server.local_addr()?;
    tokio::spawn(serve(server.clone()));
    let dial = proxy(server_addr, case.wan, seed).await?;
    let mut client = Endpoint::client("127.0.0.1:0".parse()?)?;
    client.set_default_client_config(client_cfg);
    let conn = client
        .connect(dial, "localhost")?
        .await
        .context("connecting")?;

    let bulk = Stream::open(&conn).await?;
    let pinger = match case.pings {
        Pings::None => None,
        Pings::SameStream => Some(bulk.clone()),
        Pings::OwnStream => {
            bulk.priority(-1).await?;
            Some(Stream::open(&conn).await?)
        }
    };
    // Warm up: one ping each, so the streams exist at the server.
    bulk.ask(PING, 0).await?;
    if let Some(p) = &pinger {
        p.ask(PING, 0).await?;
    }

    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ping_task = pinger.map(|p| {
        let done = done.clone();
        tokio::spawn(async move {
            let mut rtts = Vec::new();
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                let at = Instant::now();
                if p.ask(PING, 0).await.is_err() {
                    break;
                }
                if !done.load(std::sync::atomic::Ordering::Relaxed) {
                    rtts.push(at.elapsed().as_secs_f64() * 1e3);
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            rtts
        })
    });

    let chunks = total.div_ceil(u64::from(case.chunk));
    let left = Arc::new(std::sync::atomic::AtomicU64::new(chunks));
    let began = Instant::now();
    let mut workers = Vec::new();
    for _ in 0..case.inflight {
        let (bulk, left, chunk) = (bulk.clone(), left.clone(), case.chunk);
        workers.push(tokio::spawn(async move {
            loop {
                let prev = left.fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |n| n.checked_sub(1),
                );
                if prev.is_err() {
                    return Ok::<_, anyhow::Error>(());
                }
                bulk.ask(CHUNK, chunk).await?;
            }
        }));
    }
    for w in workers {
        w.await??;
    }
    let secs = began.elapsed().as_secs_f64();
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let pings = match ping_task {
        Some(t) => t.await?,
        None => Vec::new(),
    };
    conn.close(0u32.into(), b"done");
    client.wait_idle().await;
    server.close(0u32.into(), b"done");
    Ok(Outcome {
        secs,
        kib_s: (chunks * u64::from(case.chunk)) as f64 / 1024.0 / secs,
        pings,
    })
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i]
}

fn cases() -> Vec<Case> {
    const MIB: u32 = 1 << 20;
    let drill = Wan {
        delay_ms: 50,
        jitter_ms: 20,
        per_mille: 50,
    };
    let wans = [
        ("lan", Wan { delay_ms: 0, jitter_ms: 0, per_mille: 0 }),
        ("drill", drill),
        ("nojitter5", Wan { delay_ms: 50, jitter_ms: 0, per_mille: 50 }),
        ("jitter1", Wan { delay_ms: 50, jitter_ms: 20, per_mille: 10 }),
        ("jitter01", Wan { delay_ms: 50, jitter_ms: 20, per_mille: 1 }),
        ("jitter0", Wan { delay_ms: 50, jitter_ms: 20, per_mille: 0 }),
        ("clean", Wan { delay_ms: 50, jitter_ms: 0, per_mille: 0 }),
    ];
    let mut out = Vec::new();
    // Congestion control by WAN: 1 MiB chunks, one at a time, as today.
    for (w, wan) in wans {
        for (c, cc) in [("cubic", Cc::Cubic), ("newreno", Cc::NewReno), ("bbr", Cc::Bbr)] {
            out.push(Case {
                name: format!("cc/{w}/{c}"),
                wan,
                cc,
                chunk: MIB,
                inflight: 1,
                pings: Pings::None,
            });
        }
    }
    // Chunk size and asks in flight, on the drill's WAN.
    for cc in [Cc::Cubic, Cc::Bbr] {
        for (cn, chunk) in [("1m", MIB), ("256k", MIB / 4), ("64k", MIB / 16)] {
            for inflight in [1, 4] {
                out.push(Case {
                    name: format!("chunk/{cc:?}/{cn}x{inflight}").to_lowercase(),
                    wan: drill,
                    cc,
                    chunk,
                    inflight,
                    pings: Pings::None,
                });
            }
        }
    }
    // Head-of-line: pings during a transfer, on the drill's WAN.
    for cc in [Cc::Cubic, Cc::Bbr] {
        for (pn, pings, chunk) in [
            ("same1m", Pings::SameStream, MIB),
            ("same256k", Pings::SameStream, MIB / 4),
            ("same64k", Pings::SameStream, MIB / 16),
            ("own1m", Pings::OwnStream, MIB),
        ] {
            out.push(Case {
                name: format!("hol/{cc:?}/{pn}").to_lowercase(),
                wan: drill,
                cc,
                chunk,
                inflight: 1,
                pings,
            });
        }
    }
    out
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let filter = std::env::args().nth(1).unwrap_or_default();
    let total: u64 = std::env::var("TOTAL_MIB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4)
        << 20;
    let seed: u64 = std::env::var("SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(43);
    println!(
        "{:<26} {:<16} {:>8} {:>9} {:>6} {:>9} {:>9} {:>9}",
        "case", "wan", "secs", "KiB/s", "pings", "p50 ms", "p90 ms", "max ms"
    );
    for case in cases().into_iter().filter(|c| c.name.contains(&filter)) {
        let got = tokio::time::timeout(Duration::from_secs(900), run(&case, total, seed)).await;
        match got {
            Ok(Ok(mut o)) => {
                o.pings.sort_by(|a, b| a.total_cmp(b));
                println!(
                    "{:<26} {:<16} {:>8.1} {:>9.0} {:>6} {:>9.0} {:>9.0} {:>9.0}",
                    case.name,
                    case.wan.to_string(),
                    o.secs,
                    o.kib_s,
                    o.pings.len(),
                    pct(&o.pings, 0.5),
                    pct(&o.pings, 0.9),
                    o.pings.last().copied().unwrap_or(f64::NAN),
                );
            }
            Ok(Err(e)) => println!("{:<26} error: {e:#}", case.name),
            Err(_) => println!("{:<26} timed out", case.name),
        }
    }
    Ok(())
}
