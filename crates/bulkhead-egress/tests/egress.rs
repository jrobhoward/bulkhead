//! The egress driver as a node runs it: the node's requests as calls,
//! every connection a stream the runtime opened where the app may send
//! requests, against local HTTP and HTTPS servers.

#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use bulkhead_native::protocol::{self as p, HttpError, HttpRequest};
use common::{args, get, serve, server, tls};
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn egress____two_requests_to_one_server____one_pooled_connection_the_node_opened() {
    let (address, accepted) = server(None).await;
    let s = serve(&args(json!({})), &[format!("http://{address}")]).await;

    let first = s
        .send(get(&format!("http://{address}/hello")))
        .await
        .unwrap();
    let second = s
        .send(HttpRequest {
            method: "POST".into(),
            body: p::Bytes(b"payload".to_vec()),
            ..get(&format!("http://{address}/echo"))
        })
        .await
        .unwrap();

    assert_eq!((first.status, first.body.0.as_slice()), (200, &b"hi"[..]));
    assert_eq!(second.body.0, b"POST payload");
    assert_eq!(accepted.load(Ordering::Relaxed), 1, "pooled");
    let header = |name: &str| first.headers.iter().any(|(n, _)| n == name);
    assert!(header("x-kept"));
    assert!(!header("keep-alive"), "hop-by-hop headers are not answered");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while s.counter("connections_total") == 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(s.counter("connections_total"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn egress____a_name_the_app_allows____resolved_and_connected_by_the_node() {
    let (address, _) = server(None).await;
    let s = serve(
        &args(json!({})),
        &[format!("http://api.test:{}", address.port())],
    )
    .await;

    let got = s
        .send(get(&format!("http://api.test:{}/hello", address.port())))
        .await
        .unwrap();

    assert_eq!(got.status, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn egress____a_destination_the_app_does_not_allow____prohibited_and_nothing_sent() {
    let (allowed, _) = server(None).await;
    let (other, accepted) = server(None).await;
    let s = serve(&args(json!({})), &[format!("http://{allowed}")]).await;

    let got = s.send(get(&format!("http://{other}/hello"))).await;

    assert_eq!(got, Err(HttpError::Prohibited));
    assert_eq!(accepted.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn egress____https____trusted_with_the_given_roots_only() {
    let (ca, config) = tls();
    let (address, _) = server(Some(config)).await;
    let allow = [format!("https://{address}")];
    let trusting = serve(&args(json!({ "roots_pem": ca })), &allow).await;
    let untrusting = serve(&args(json!({})), &allow).await;

    let good = trusting
        .send(get(&format!("https://{address}/hello")))
        .await
        .unwrap();
    let bad = untrusting
        .send(get(&format!("https://{address}/hello")))
        .await;

    assert_eq!(good.body.0, b"hi");
    assert!(matches!(bad, Err(HttpError::Tls(_))), "{bad:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn egress____slow_headers_or_a_stalled_body____timeout_and_the_driver_serves_on() {
    let (address, _) = server(None).await;
    let s = serve(&args(json!({})), &[format!("http://{address}")]).await;
    let short = Duration::from_millis(300);

    let slow = s
        .send_with(get(&format!("http://{address}/slow")), short, short)
        .await;
    let stalled = s
        .send_with(get(&format!("http://{address}/stall")), short, short)
        .await;
    let fine = s.send(get(&format!("http://{address}/hello"))).await;

    assert_eq!(slow, Err(HttpError::Timeout));
    assert_eq!(stalled, Err(HttpError::Timeout));
    assert_eq!(fine.unwrap().status, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn egress____a_body_over_the_cap____refused() {
    let (address, _) = server(None).await;
    let s = serve(&args(json!({})), &[format!("http://{address}")]).await;

    let got = s.send(get(&format!("http://{address}/big"))).await;

    assert_eq!(
        got,
        Err(HttpError::Other(format!(
            "response body over {} bytes",
            1 << 20
        )))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn egress____nothing_listening____unreachable() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    let s = serve(&args(json!({})), &[format!("http://{address}")]).await;

    let got = s.send(get(&format!("http://{address}/hello"))).await;

    assert!(matches!(got, Err(HttpError::Unreachable(_))), "{got:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn egress____a_forbidden_header____invalid_and_nothing_sent() {
    let (address, accepted) = server(None).await;
    let s = serve(&args(json!({})), &[format!("http://{address}")]).await;

    let got = s
        .send(HttpRequest {
            headers: vec![("host".into(), p::Bytes(b"elsewhere".to_vec()))],
            ..get(&format!("http://{address}/hello"))
        })
        .await;

    assert!(matches!(got, Err(HttpError::Invalid(_))), "{got:?}");
    assert_eq!(accepted.load(Ordering::Relaxed), 0);
}
