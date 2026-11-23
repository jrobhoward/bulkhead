#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

#[test]
fn from_args____documented_example____every_field() {
    let args = br#"{"threads": 2, "listeners": [{"name": "web", "max_connections": 8,
        "header_timeout_ms": 100, "body_timeout_ms": 200, "max_response": 300,
        "standby": {"redirect": {"partner_url": "https://node2:8443"}},
        "tls": [{"cert": "/driver/web.crt", "key": "/driver/web.key", "hosts": ["shop.example"]}],
        "tls_reload_s": 60}]}"#;

    let config = Config::from_args(args).unwrap();

    assert_eq!(config.threads, 2);
    let l = &config.listeners[0];
    assert_eq!(
        (
            l.max_connections,
            l.header_timeout_ms,
            l.body_timeout_ms,
            l.max_response
        ),
        (8, 100, 200, 300)
    );
    assert_eq!(
        l.standby,
        Standby::Redirect {
            partner_url: "https://node2:8443".into()
        }
    );
    assert_eq!(l.tls[0].hosts, vec!["shop.example".to_string()]);
    assert_eq!(l.tls_reload_s, 60);
}

#[test]
fn from_args____names_only____the_node_ingress_defaults() {
    let args = br#"{"listeners": [{"name": "web"}, {"name": "api", "standby": "forward"}]}"#;

    let config = Config::from_args(args).unwrap();

    assert_eq!(config.threads, 4);
    let l = &config.listeners[0];
    assert_eq!(l.max_connections, 1_024);
    assert_eq!(l.header_timeout(), Duration::from_secs(10));
    assert_eq!(l.body_timeout(), Duration::from_secs(30));
    assert_eq!(l.max_response, 16 << 20);
    assert_eq!(l.standby, Standby::Refuse);
    assert!(l.tls.is_empty());
    assert!(l.http2, "HTTP/2 by ALPN on a TLS listener, by default");
    assert_eq!(l.max_streams, 100);
    assert_eq!(config.listeners[1].standby, Standby::Forward);
}

#[rstest]
#[case::no_listeners(r#"{"listeners": []}"#)]
#[case::seven_listeners(r#"{"listeners": [{"name":"a"},{"name":"b"},{"name":"c"},{"name":"d"},{"name":"e"},{"name":"f"},{"name":"g"}]}"#)]
#[case::name_twice(r#"{"listeners": [{"name": "a"}, {"name": "a"}]}"#)]
#[case::no_threads(r#"{"threads": 0, "listeners": [{"name": "a"}]}"#)]
#[case::zero_timeout(r#"{"listeners": [{"name": "a", "body_timeout_ms": 0}]}"#)]
#[case::redirect_without_url(
    r#"{"listeners": [{"name": "a", "standby": {"redirect": {"partner_url": ""}}}]}"#
)]
#[case::unknown_key(r#"{"listeners": [{"name": "a", "acceptors": 4}]}"#)]
#[case::not_json("[http.web]")]
fn from_args____invalid____refused(#[case] args: &str) {
    let result = Config::from_args(args.as_bytes());

    assert!(result.is_err(), "{args} was taken");
}
