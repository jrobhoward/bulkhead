#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn Request____envelope____round_trips() {
    let req = Request {
        scheme: "https".into(),
        method: "POST".into(),
        path: "/orders".into(),
        query: Some("a=1&b".into()),
        headers: vec![("X-Tenant".into(), "t1".into())],
        body: b"hi".to_vec(),
        key: Some("t1".into()),
        client_cert: Some(ClientCert {
            sha256: "ab".repeat(32),
            der: vec![0x30, 0x03, 1, 2, 3],
        }),
    };

    let back = Request::from_envelope(&req.to_envelope()).unwrap();

    assert_eq!(back, req);
    assert_eq!(back.header("x-tenant"), Some("t1"));
    assert_eq!(back.param("a"), Some("1"));
    assert_eq!(back.param("b"), Some(""));
    assert_eq!(back.text(), Some("hi"));
}

#[test]
fn Request____other_envelope____none() {
    assert!(Request::from_envelope(&Envelope::text("GET /")).is_none());
}

#[test]
fn Response____builders____round_trip() {
    let r = Response::json(201, &serde_json::json!({ "id": 7 }))
        .unwrap()
        .header("x-a", "b");

    let back = Response::from_envelope(&r.clone().into()).unwrap();

    assert_eq!(back, r);
    assert_eq!(back.body, br#"{"id":7}"#);
}

fn reply(status: Option<Value>, headers: Value) -> Envelope {
    let mut map = vec![
        (text("headers"), headers),
        (text("body"), Value::Bytes(b"x".to_vec())),
    ];
    if let Some(s) = status {
        map.push((text("status"), s));
    }
    encode(&Value::Map(map), RESPONSE_SCHEMA)
}

#[test]
fn Response____from_reply_of_another_format____200_with_its_content_type() {
    let msg = Envelope::new("text/csv", b"a,b".to_vec());

    let r = Response::from_reply(&msg).unwrap();

    assert_eq!(r.status, 200);
    assert_eq!(r.headers, vec![("content-type".into(), "text/csv".into())]);
    assert_eq!(r.body, b"a,b");
}

#[test]
fn Response____from_reply_without_a_status____200() {
    let msg = reply(None, Value::Array(vec![]));

    let r = Response::from_reply(&msg).unwrap();

    assert_eq!(r.status, 200);
    assert_eq!(r.body, b"x");
}

#[rstest::rstest]
#[case::status_below_100(Some(Value::Integer(99.into())), Value::Array(vec![]))]
#[case::status_above_999(Some(Value::Integer(1000.into())), Value::Array(vec![]))]
#[case::header_not_a_pair(None, Value::Array(vec![Value::Array(vec![text("x-a")])]))]
#[case::header_value_not_text(None, Value::Array(vec![Value::Array(vec![text("x-a"), Value::Integer(1.into())])]))]
fn Response____from_reply_malformed____none(#[case] status: Option<Value>, #[case] headers: Value) {
    let msg = reply(status, headers);

    let r = Response::from_reply(&msg);

    assert_eq!(r, None);
}

fn route(app: &str, host: Option<&str>, prefix: &str) -> Route {
    Route {
        app: app.into(),
        host: host.map(str::to_string),
        path_prefix: prefix.into(),
        hash_header: None,
        max_body: 1024,
        timeout_ms: 1000,
    }
}

#[test]
fn Route____matches____whole_segments_and_any_case_host() {
    let r = route("a", Some("shop.example"), "/orders");

    assert!(r.matches(Some("SHOP.example"), "/orders/7"));
    assert!(r.matches(Some("shop.example"), "/orders"));
    assert!(!r.matches(Some("shop.example"), "/ordersx"));
    assert!(!r.matches(None, "/orders"));
    assert!(route("b", None, "/api/").matches(None, "/api/x"));
    assert!(!route("c", None, "/orders").matches(None, "/"));
}

#[test]
fn Routes____pick____longest_prefix_then_named_host_wins() {
    let routes = Routes {
        routes: vec![
            route("orders", None, "/orders"),
            route("root", None, "/"),
            route("shop", Some("shop.example"), "/orders"),
        ],
    };

    let any = routes.pick(Some("other.example"), "/orders/1").unwrap();
    let named = routes.pick(Some("shop.example"), "/orders/1").unwrap();
    let fallback = routes.pick(None, "/health").unwrap();
    let none = Routes::default().pick(None, "/").is_none();

    assert_eq!(any.app, "orders");
    assert_eq!(named.app, "shop");
    assert_eq!(fallback.app, "root");
    assert!(none);
}
