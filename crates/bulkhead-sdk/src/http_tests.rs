#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

fn text(s: &str) -> Value {
    Value::Text(s.to_string())
}

/// `value` as an envelope of `schema`, as `to_envelope` wrote one before
/// it serialized from the request itself (ROADMAP PF1).
fn encode_value(value: &Value, schema: &str) -> Envelope {
    let mut body = Vec::new();
    ciborium::into_writer(value, &mut body).unwrap();
    Envelope::new("application/cbor", body).with_schema(schema)
}

/// A request as a CBOR value, built as `to_envelope` built it before.
fn request_value(r: &Request) -> Value {
    let headers = r
        .headers
        .iter()
        .map(|(k, v)| Value::Array(vec![text(k), text(v)]))
        .collect();
    let mut map = vec![
        (text("scheme"), text(&r.scheme)),
        (text("method"), text(&r.method)),
        (text("path"), text(&r.path)),
        (
            text("query"),
            r.query.clone().map_or(Value::Null, Value::Text),
        ),
        (text("headers"), Value::Array(headers)),
        (text("body"), Value::Bytes(r.body.clone())),
    ];
    if let Some(key) = &r.key {
        map.push((text("key"), text(key)));
    }
    if let Some(c) = &r.client_cert {
        map.push((
            text("client_cert"),
            Value::Map(vec![
                (text("sha256"), text(&c.sha256)),
                (text("der"), Value::Bytes(c.der.clone())),
            ]),
        ));
    }
    Value::Map(map)
}

#[rstest::rstest]
#[case::plain(None, None, None)]
#[case::query_and_key(Some("a=1"), Some("k"), None)]
#[case::client_cert(None, None, Some(ClientCert { sha256: "ab".into(), der: vec![1, 2, 3] }))]
fn Request____to_envelope____the_bytes_of_the_value_encoding(
    #[case] query: Option<&str>,
    #[case] key: Option<&str>,
    #[case] client_cert: Option<ClientCert>,
) {
    let req = Request {
        scheme: "https".into(),
        method: "POST".into(),
        path: "/orders".into(),
        query: query.map(str::to_string),
        headers: vec![("a".into(), "b".into()), ("c".into(), "d".into())],
        body: vec![7; 70_000],
        key: key.map(str::to_string),
        client_cert,
    };

    let env = req.to_envelope();

    assert_eq!(env, encode_value(&request_value(&req), REQUEST_SCHEMA));
}

#[test]
fn Response____to_envelope____the_bytes_of_the_value_encoding() {
    let r = Response::new(201).header("x-a", "b").body(vec![9; 70_000]);
    let value = Value::Map(vec![
        (text("status"), Value::Integer(201.into())),
        (
            text("headers"),
            Value::Array(vec![Value::Array(vec![text("x-a"), text("b")])]),
        ),
        (text("body"), Value::Bytes(vec![9; 70_000])),
    ]);

    let env = r.to_envelope();

    assert_eq!(env, encode_value(&value, RESPONSE_SCHEMA));
}

#[test]
fn Response____from_reply_owned_of_another_format____body_moved() {
    let msg = Envelope::new("text/csv", b"a,b".to_vec());

    let r = Response::from_reply_owned(msg).unwrap();

    assert_eq!(r.status, 200);
    assert_eq!(r.headers, vec![("content-type".into(), "text/csv".into())]);
    assert_eq!(r.body, b"a,b");
}

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
    encode_value(&Value::Map(map), RESPONSE_SCHEMA)
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
