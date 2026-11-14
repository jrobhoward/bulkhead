//! Spike S41: DA1's driver with the node's own ingress stack inside it.
//!
//! - The listening sockets node config `[drivers.driver]` binds and passes
//!   (`bulkhead_native::driver`), accepted on by worker 0 alone (the
//!   runner's seccomp filter allows `accept4` on the passed descriptor
//!   numbers only, so a duplicate cannot accept), which hands each
//!   connection to the workers in turn.
//! - `threads` worker threads (init arguments, default 4), each a tokio
//!   current-thread runtime with a sender channel of its own
//!   (`bulkhead_native::sender_async`): a task per connection, served by
//!   hyper's HTTP/1.1 server (rustls for `https`), each request a call to
//!   the app `echo` by name, awaited.
//!
//! `/local` is answered by the driver itself, with no call, as DA1's is.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use bulkhead_native::sender_async::AsyncSender;
use bulkhead_sdk::http::{Request, Response};
use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Reply};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;

const MAX_BODY: usize = 4 << 20;
const CALL_TIMEOUT_MS: u32 = 10_000;
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);

struct Driver;

fn tls_config() -> Result<Arc<rustls::ServerConfig>, String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let cert = bulkhead_sdk::files::path("/driver/c.pem");
    let key = bulkhead_sdk::files::path("/driver/k.pem");
    let certs = CertificateDer::pem_file_iter(&cert)
        .map_err(|e| format!("{}: {e}", cert.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let key = PrivateKeyDer::from_pem_file(&key).map_err(|e| format!("{}: {e}", key.display()))?;
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(|e| e.to_string())?;
    Ok(Arc::new(config))
}

impl App for Driver {
    fn init(_ctx: &mut Ctx, args: &[u8]) -> Result<Self, String> {
        let args = serde_json::from_slice::<serde_json::Value>(args).unwrap_or_default();
        let threads = args["threads"].as_u64().unwrap_or(4) as usize;
        let listeners: Vec<(String, std::net::TcpListener)> = bulkhead_native::driver::listeners()
            .into_iter()
            .map(|(name, fd)| (name, std::net::TcpListener::from(fd)))
            .collect();
        if listeners.is_empty() {
            return Err("no listeners: not configured as a driver".into());
        }
        let tls = tokio_rustls::TlsAcceptor::from(tls_config()?);
        let mut inboxes = Vec::new();
        let mut workers = Vec::new();
        for _ in 0..threads.max(1) {
            let sender = bulkhead_native::app::Ctx::current()
                .sender()
                .map_err(|e| format!("{e:?}"))?;
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            inboxes.push(tx);
            workers.push((sender, rx));
        }
        let mut listeners = Some(listeners);
        for (i, (sender, rx)) in workers.into_iter().enumerate() {
            let tls = tls.clone();
            let accepting = (i == 0).then(|| (listeners.take().unwrap(), inboxes.clone()));
            std::thread::spawn(move || worker(sender, rx, accepting, tls));
        }
        Ok(Driver)
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, _msg: Envelope) -> Reply {
        Reply::Now(Envelope::text("a driver"))
    }
}

/// A connection handed to a worker: the stream and whether it is TLS.
type Handed = (std::net::TcpStream, bool);

fn worker(
    sender: bulkhead_native::sender::Sender,
    mut inbox: tokio::sync::mpsc::UnboundedReceiver<Handed>,
    accepting: Option<(
        Vec<(String, std::net::TcpListener)>,
        Vec<tokio::sync::mpsc::UnboundedSender<Handed>>,
    )>,
    tls: tokio_rustls::TlsAcceptor,
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let sender = AsyncSender::new(sender).unwrap();
        tokio::task::spawn_local(sender.clone().pump());
        if let Some((listeners, inboxes)) = accepting {
            let next = std::rc::Rc::new(std::cell::Cell::new(0usize));
            for (name, l) in listeners {
                l.set_nonblocking(true).unwrap();
                let l = TcpListener::from_std(l).unwrap();
                tokio::task::spawn_local(accept(l, name == "https", inboxes.clone(), next.clone()));
            }
        }
        while let Some((stream, is_tls)) = inbox.recv().await {
            stream.set_nonblocking(true).unwrap();
            let stream = tokio::net::TcpStream::from_std(stream).unwrap();
            let sender = sender.clone();
            let tls = is_tls.then(|| tls.clone());
            tokio::task::spawn_local(async move {
                match tls {
                    None => serve(stream, "http", sender).await,
                    Some(tls) => {
                        if let Ok(Ok(s)) =
                            tokio::time::timeout(HEADER_TIMEOUT, tls.accept(stream)).await
                        {
                            serve(s, "https", sender).await;
                        }
                    }
                }
            });
        }
    });
}

/// Accept on `l` and hand each connection to the next worker in turn.
async fn accept(
    l: TcpListener,
    is_tls: bool,
    inboxes: Vec<tokio::sync::mpsc::UnboundedSender<Handed>>,
    next: std::rc::Rc<std::cell::Cell<usize>>,
) {
    loop {
        let (stream, _) = match l.accept().await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let i = next.get();
        next.set((i + 1) % inboxes.len());
        let _ = inboxes[i].send((stream.into_std().unwrap(), is_tls));
    }
}

async fn serve<IO>(io: IO, scheme: &'static str, sender: AsyncSender)
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + 'static,
{
    let service = hyper::service::service_fn(move |req| {
        let sender = sender.clone();
        async move { Ok::<_, Infallible>(handle(req, scheme, &sender).await) }
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_TIMEOUT)
        .serve_connection(TokioIo::new(io), service)
        .await;
}

fn reply(status: u16, body: Vec<u8>, headers: &[(String, String)]) -> hyper::Response<Full<Bytes>> {
    let mut r = hyper::Response::new(Full::new(Bytes::from(body)));
    *r.status_mut() = hyper::StatusCode::from_u16(status).unwrap_or(hyper::StatusCode::BAD_GATEWAY);
    for (k, v) in headers {
        if let (Ok(k), Ok(v)) = (
            hyper::header::HeaderName::from_bytes(k.as_bytes()),
            hyper::header::HeaderValue::from_str(v),
        ) {
            r.headers_mut().append(k, v);
        }
    }
    r
}

async fn handle(
    req: hyper::Request<hyper::body::Incoming>,
    scheme: &str,
    sender: &AsyncSender,
) -> hyper::Response<Full<Bytes>> {
    if req.uri().path() == "/local" {
        return reply(200, b"ok".to_vec(), &[]);
    }
    let (parts, body) = req.into_parts();
    let body = match Limited::new(body, MAX_BODY).collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => return reply(413, b"body too large\n".to_vec(), &[]),
    };
    let request = Request {
        scheme: scheme.to_string(),
        method: parts.method.as_str().to_string(),
        path: parts.uri.path().to_string(),
        query: parts.uri.query().map(str::to_string),
        headers: parts
            .headers
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect(),
        body: body.to_vec(),
        key: None,
    };
    match sender
        .call_app("echo", request.to_envelope().into(), CALL_TIMEOUT_MS)
        .await
    {
        Ok(answer) => match Response::from_envelope(&answer.into()) {
            Some(resp) => reply(resp.status, resp.body, &resp.headers),
            None => reply(502, b"not a response\n".to_vec(), &[]),
        },
        Err(e) => reply(502, format!("{e:?}\n").into_bytes(), &[]),
    }
}

bulkhead_sdk::app!(Driver);

fn main() -> std::process::ExitCode {
    bulkhead_main()
}
