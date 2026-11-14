//! The resolver driver as a node runs it: names asked through the
//! runtime (`Runtime::resolve`, as apps' `connect_host`, egress and
//! datasources ask), answered by `bulkhead-resolve` over TCP streams the
//! runtime opens to a test DNS server.

#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use bulkhead_host::ResolveError;
use common::Dns;

const WAIT: Duration = Duration::from_secs(10);

fn ips(found: &[SocketAddr]) -> Vec<IpAddr> {
    let mut ips: Vec<IpAddr> = found.iter().map(SocketAddr::ip).collect();
    ips.sort();
    ips
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolve____a_name_with_a_and_aaaa_records____both_addresses_on_the_port() {
    let dns = Dns::start(&[("svc.test", &["192.0.2.10", "2001:db8::10"])]).await;
    let s = common::serve(&[dns.addr], "", "").await;

    let found = s.rt.resolve("svc.test", 8080, WAIT).await.unwrap();

    assert_eq!(
        ips(&found),
        vec![
            "192.0.2.10".parse::<IpAddr>().unwrap(),
            "2001:db8::10".parse().unwrap()
        ]
    );
    assert!(found.iter().all(|a| a.port() == 8080));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolve____a_name_in_the_hosts_file____answered_without_a_query() {
    let dns = Dns::start(&[]).await;
    let s = common::serve(&[dns.addr], "", "10.1.2.3 db.internal db\n").await;

    let found = s.rt.resolve("db.internal", 5432, WAIT).await.unwrap();

    assert_eq!(ips(&found), vec!["10.1.2.3".parse::<IpAddr>().unwrap()]);
    assert_eq!(dns.queries(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolve____no_such_name____not_found() {
    let dns = Dns::start(&[]).await;
    let s = common::serve(&[dns.addr], "", "").await;

    let got = s.rt.resolve("absent.test", 80, WAIT).await;

    assert_eq!(got, Err(ResolveError::NotFound("absent.test".into())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolve____asked_twice____the_second_from_the_cache() {
    // Both record types: an empty answer without an SOA record, as this
    // test server gives, is not cached.
    let dns = Dns::start(&[("svc.test", &["192.0.2.10", "2001:db8::10"])]).await;
    let s = common::serve(&[dns.addr], "", "").await;
    s.rt.resolve("svc.test", 80, WAIT).await.unwrap();
    let after_first = dns.queries();

    let again = s.rt.resolve("svc.test", 80, WAIT).await.unwrap();

    assert_eq!(ips(&again).len(), 2);
    assert!(after_first > 0);
    assert_eq!(dns.queries(), after_first);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolve____a_short_name____found_under_the_search_domain() {
    let dns = Dns::start(&[("svc.corp.test", &["192.0.2.20"])]).await;
    let s = common::serve(&[dns.addr], "search corp.test\n", "").await;

    let found = s.rt.resolve("svc", 80, WAIT).await.unwrap();

    assert_eq!(ips(&found), vec!["192.0.2.20".parse::<IpAddr>().unwrap()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolve____nameserver_not_listening____temporary_within_its_timeout() {
    // A port nothing listens on.
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let s = common::serve(&[closed], "options timeout:1 attempts:1\n", "").await;
    let start = Instant::now();

    let got = s.rt.resolve("svc.test", 80, WAIT).await;

    assert!(matches!(got, Err(ResolveError::Temporary(_))), "{got:?}");
    assert!(start.elapsed() < WAIT, "{:?}", start.elapsed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolve____not_a_host_name____refused() {
    let dns = Dns::start(&[]).await;
    let s = common::serve(&[dns.addr], "", "").await;

    let got = s.rt.resolve("a..b", 80, WAIT).await;

    assert!(matches!(got, Err(ResolveError::Refused(_))), "{got:?}");
    assert_eq!(dns.queries(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolve____many_at_once____each_answered() {
    let names: Vec<String> = (0..40).map(|i| format!("n{i}.test")).collect();
    let records: Vec<(String, [String; 1])> = names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), [format!("192.0.2.{}", i + 1)]))
        .collect();
    let records: Vec<(&str, Vec<&str>)> = records
        .iter()
        .map(|(n, ips)| (n.as_str(), ips.iter().map(String::as_str).collect()))
        .collect();
    let records: Vec<(&str, &[&str])> = records
        .iter()
        .map(|(n, ips)| (*n, ips.as_slice()))
        .collect();
    let dns = Dns::start(&records).await;
    let s = common::serve(&[dns.addr], "", "").await;

    let lookups = names.iter().map(|n| s.rt.resolve(n, 80, WAIT));
    let found = futures_util::future::join_all(lookups).await;

    for (i, got) in found.into_iter().enumerate() {
        let want: IpAddr = format!("192.0.2.{}", i + 1).parse().unwrap();
        assert_eq!(ips(&got.unwrap()), vec![want]);
    }
    // One stream to the nameserver, its queries multiplexed.
    assert_eq!(common::counter(&s.rt, "streams_total"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolve____nameserver_closes_an_idle_connection____next_lookup_over_a_new_stream() {
    let dns = Dns::start(&[
        ("one.test", &["192.0.2.1", "2001:db8::1"]),
        ("two.test", &["192.0.2.2", "2001:db8::2"]),
    ])
    .await;
    dns.close_when_idle(Duration::from_millis(200));
    let s = common::serve(&[dns.addr], "", "").await;

    let one = s.rt.resolve("one.test", 80, WAIT).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let two = s.rt.resolve("two.test", 80, WAIT).await;

    assert_eq!(ips(&one.unwrap()).len(), 2);
    assert_eq!(ips(&two.unwrap()).len(), 2);
    assert!(common::counter(&s.rt, "streams_total") >= 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolve____lookups____counted_in_the_driver_s_metrics() {
    let dns = Dns::start(&[("svc.test", &["192.0.2.10"])]).await;
    let s = common::serve(&[dns.addr], "", "").await;

    s.rt.resolve("svc.test", 80, WAIT).await.unwrap();
    let _ = s.rt.resolve("absent.test", 80, WAIT).await;

    assert_eq!(common::counter(&s.rt, "lookups_total"), 2);
    assert_eq!(common::counter(&s.rt, "lookups_failed_total"), 1);
    assert!(common::counter(&s.rt, "streams_total") >= 1);
}
