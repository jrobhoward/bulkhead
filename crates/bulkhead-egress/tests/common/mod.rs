//! The driver run as the node runs it, on a runtime of the test's own:
//! `bulkhead-egress` built and started as the app `bulkhead-egress-shop`
//! beside an instance of `shop` (the native fixture, granted
//! `bulkhead:http-egress` with an `allow` list, which is what lets the
//! node connect the driver), and the node's requests sent as calls.
//! Local HTTP and HTTPS servers answer them.

#![allow(dead_code)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use bulkhead_bundle::manifest::{
    AppInfo, AppManifest, AppSpec, Capability, NativeProgram, ProgramRef, Provenance,
};
use bulkhead_host::{
    AppDeployment, Apps, Envelope, InstanceHandle, InstanceSpec, NativeCode, Resolver, Runtime,
    RuntimeConfig,
};
use bulkhead_native::protocol::{self as p, HttpError, HttpRequest, HttpResponse};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame, Incoming};
use hyper::{Request, Response};
use serde_json::{Value, json};

/// The app whose driver this is.
pub const APP: &str = "shop";

/// The driver's app name, as the node gives it.
pub const DRIVER: &str = "bulkhead-egress-shop";

/// The schema of the node's requests.
pub const SCHEMA: &str = "bulkhead/egress";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// `bulkhead-runner`, `bulkhead-egress` and the native fixture, built on
/// first use where the node's tests build them.
pub fn programs() -> &'static (PathBuf, PathBuf, PathBuf) {
    static BUILT: OnceLock<(PathBuf, PathBuf, PathBuf)> = OnceLock::new();
    BUILT.get_or_init(|| {
        let native = workspace_root().join("target/fixtures/native");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .current_dir(workspace_root())
            .args(["build", "-p", "bulkhead-runner", "-p", "bulkhead-egress"])
            .args(["-p", "bulkhead-fixture-native"])
            .arg("--target-dir")
            .arg(&native)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "building the runner and the egress driver failed"
        );
        let debug = native.join("debug");
        (
            debug.join("bulkhead-runner"),
            debug.join("bulkhead-egress"),
            debug.join("bulkhead-fixture-native"),
        )
    })
}

/// The init arguments, with `extra` fields added or replaced.
pub fn args(extra: Value) -> Value {
    let mut args = json!({
        "app": APP,
        "connect_timeout_ms": 2000,
        "pool_idle_timeout_ms": 90000,
        "pool_max_idle_per_host": 8,
        "max_response_body": 1 << 20,
    });
    if let (Some(args), Value::Object(extra)) = (args.as_object_mut(), extra) {
        args.extend(extra);
    }
    args
}

/// The node's manifest for the driver, with `args` as its init arguments.
pub fn manifest(args: &Value) -> AppManifest {
    let spec = json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "instances": { "count": 1 },
        "lifecycle": { "start_args": args },
        "supervision": { "intensity": 100, "backoff": { "initial_ms": 1, "max_ms": 50 } },
        "limits": { "memory_max": "128MiB" },
        "messaging_acl": { "accept_from": [DRIVER], "may_call": [DRIVER] },
        "capabilities": {
            "bulkhead:metrics": { "prefix": "bulkhead_egress" },
        },
    });
    AppManifest {
        app: AppInfo {
            name: DRIVER.into(),
            version: "1.0.0".parse().unwrap(),
            description: None,
            authors: vec![],
            license: None,
            repository: None,
        },
        program: ProgramRef {
            path: String::new(),
            checksum: String::new(),
        },
        provenance: Provenance {
            signer_key: String::new(),
            build_info: None,
            sbom: None,
        },
        spec: AppSpec::from_json_value(&spec).unwrap(),
    }
}

/// A runtime with `shop` and its driver running.
pub struct Served {
    pub rt: Runtime,
    pub apps: Apps,
    pub shop: InstanceHandle,
}

/// Start `shop`, allowed `allow`, and its driver with `args`; names from
/// a table (`api.test` is loopback), loopback names allowed. Call from a
/// multi-threaded tokio runtime.
pub async fn serve(args: &Value, allow: &[String]) -> Served {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let (runner, program, fixture) = programs();
    let mut native = bulkhead_host::NativeConfig::default();
    native.runner = Some(runner.clone());
    let mut egress = bulkhead_host::Egress::default();
    egress.allow_loopback = true;
    egress.connect_timeout = Duration::from_secs(2);
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        native,
        egress,
        resolver: Resolver::Table(BTreeMap::from([(
            "api.test".to_string(),
            vec!["127.0.0.1".parse().unwrap()],
        )])),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let fixture = rt.load_native(NativeCode {
        path: fixture.clone(),
        kind: NativeProgram::Executable,
    });
    let mut spec = InstanceSpec {
        memory_max: 64 << 20,
        callback_deadline: Duration::from_secs(5),
        init_timeout: Duration::from_secs(5),
        granted: [Capability::HttpOutgoing].into_iter().collect(),
        ..InstanceSpec::new(APP, "1.0.0")
    };
    spec.services.egress.allow = allow.to_vec();
    let shop = rt.spawn(&fixture, spec).unwrap();
    let apps = rt.start_apps(Vec::new()).await.unwrap();
    let code = rt.load_native(NativeCode {
        path: program.clone(),
        kind: NativeProgram::Executable,
    });
    apps.add(AppDeployment::new(manifest(args), code))
        .await
        .unwrap();
    Served { rt, apps, shop }
}

/// A request as the node sends it.
pub fn get(url: &str) -> HttpRequest {
    HttpRequest {
        method: "GET".into(),
        url: url.into(),
        headers: vec![],
        body: p::Bytes(vec![]),
        timeout_ms: None,
    }
}

impl Served {
    /// The driver's answer to `request`, with these timeouts.
    pub async fn send_with(
        &self,
        request: HttpRequest,
        first_byte: Duration,
        between: Duration,
    ) -> Result<HttpResponse, HttpError> {
        let req = bulkhead_egress::EgressRequest {
            request,
            first_byte_ms: u64::try_from(first_byte.as_millis()).unwrap(),
            between_ms: u64::try_from(between.as_millis()).unwrap(),
        };
        let mut body = Vec::new();
        ciborium::into_writer(&req, &mut body).unwrap();
        let app = self.apps.app(DRIVER).unwrap();
        let answer = app
            .call(
                Envelope {
                    content_type: "application/cbor".into(),
                    schema: Some(SCHEMA.into()),
                    body,
                },
                Duration::from_secs(20),
            )
            .await
            .unwrap();
        assert_eq!(answer.schema.as_deref(), Some(SCHEMA));
        ciborium::from_reader(answer.body.as_slice()).unwrap()
    }

    /// The driver's answer to `request`, with 5 s timeouts.
    pub async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.send_with(request, Duration::from_secs(5), Duration::from_secs(5))
            .await
    }

    /// The driver's counter `bulkhead_egress_<name>`, once reported, else 0.
    pub fn counter(&self, name: &str) -> u64 {
        let wanted = format!("bulkhead_egress_{name}{{");
        self.rt
            .render_metrics()
            .lines()
            .filter(|l| l.starts_with(&wanted))
            .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
            .map(|v| v as u64)
            .sum()
    }
}

/// A body that sends one chunk and then nothing, ever.
struct Stall(bool);

impl Body for Stall {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let this = self.get_mut();
        if this.0 {
            Poll::Pending
        } else {
            this.0 = true;
            Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"first")))))
        }
    }
}

type Reply = Response<http_body_util::combinators::BoxBody<Bytes, Infallible>>;

async fn answer(req: Request<Incoming>) -> Result<Reply, Infallible> {
    let path = req.uri().path().to_string();
    let method = req.method().to_string();
    let full = |s: String| Full::new(Bytes::from(s)).boxed();
    Ok(match path.as_str() {
        "/hello" => {
            let mut r = Response::new(full("hi".into()));
            r.headers_mut()
                .insert("x-kept", hyper::header::HeaderValue::from_static("yes"));
            r.headers_mut().insert(
                "keep-alive",
                hyper::header::HeaderValue::from_static("timeout=5"),
            );
            r
        }
        "/echo" => {
            let body = req.into_body().collect().await.unwrap().to_bytes();
            Response::new(full(format!("{method} {}", String::from_utf8_lossy(&body))))
        }
        "/big" => Response::new(full("x".repeat(2 << 20))),
        "/slow" => {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            Response::new(full("late".into()))
        }
        "/stall" => Response::new(Stall(false).boxed()),
        _ => {
            let mut r = Response::new(full("missing".into()));
            *r.status_mut() = hyper::StatusCode::NOT_FOUND;
            r
        }
    })
}

/// A local HTTP server (HTTPS with `tls`); its address and how many
/// connections it accepted.
pub async fn server(tls: Option<Arc<rustls::ServerConfig>>) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = accepted.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            count.fetch_add(1, Ordering::Relaxed);
            let tls = tls.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(answer);
                let http = hyper::server::conn::http1::Builder::new();
                match tls {
                    Some(config) => {
                        let acceptor = tokio_rustls::TlsAcceptor::from(config);
                        if let Ok(stream) = acceptor.accept(stream).await {
                            let io = hyper_util::rt::TokioIo::new(stream);
                            let _ = http.serve_connection(io, service).await;
                        }
                    }
                    None => {
                        let io = hyper_util::rt::TokioIo::new(stream);
                        let _ = http.serve_connection(io, service).await;
                    }
                }
            });
        }
    });
    (address, accepted)
}

/// A CA (PEM) and an HTTPS server configuration whose certificate it
/// signed, for `127.0.0.1`.
pub fn tls() -> (String, Arc<rustls::ServerConfig>) {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = rcgen::CertifiedIssuer::self_signed(ca_params, ca_key).unwrap();
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()])
        .unwrap()
        .signed_by(&leaf_key, &ca)
        .unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![leaf.der().clone()],
        rustls_pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
    )
    .unwrap();
    (ca.pem(), Arc::new(config))
}
