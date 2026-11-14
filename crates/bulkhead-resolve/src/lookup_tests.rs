#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

#[rstest]
#[case("db.internal")]
#[case("localhost")]
#[case("xn--bcher-kva.example")]
#[case("bücher.example")]
fn name____host_names____accepted(#[case] host: &str) {
    assert!(name(host).is_ok(), "{host}");
}

#[rstest]
#[case("")]
#[case("10.0.0.1")]
#[case("::1")]
#[case("a..b")]
fn name____not_host_names____refused(#[case] host: &str) {
    assert!(name(host).is_err(), "{host}");
}

#[test]
fn name____over_253_bytes____refused() {
    let long = format!("{}.example", "a".repeat(250));

    let refused = name(&long).unwrap_err();

    assert!(refused.contains("253"), "{refused}");
}

#[test]
fn failure____a_timeout____temporary() {
    assert_eq!(failure(&NetError::Timeout), Failure::Temporary);
}

#[test]
fn failure____no_connections____temporary() {
    assert_eq!(failure(&NetError::NoConnections), Failure::Temporary);
}

#[test]
fn answer____found_nothing____not_found() {
    let answer = Answer::found(Vec::new());

    assert_eq!(answer.failure, Some(Failure::NotFound));
}

#[test]
fn answer____addresses____json_the_node_reads() {
    let answer = Answer::found(vec!["10.0.0.1".parse().unwrap(), "::1".parse().unwrap()]);

    let json = serde_json::to_value(&answer).unwrap();

    assert_eq!(json, serde_json::json!({ "addrs": ["10.0.0.1", "::1"] }));
}

#[test]
fn answer____a_failure____json_the_node_reads() {
    let answer = Answer::failed(Failure::NotFound, "NXDOMAIN".into());

    let json = serde_json::to_value(&answer).unwrap();

    assert_eq!(
        json,
        serde_json::json!({ "addrs": [], "failure": "not-found", "detail": "NXDOMAIN" })
    );
}
