#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

fn route(host: Option<&str>, prefix: &str) -> HttpRoute {
    HttpRoute {
        host: host.map(str::to_string),
        path_prefix: prefix.to_string(),
        hash_header: None,
        max_body: 1024,
        timeout: DEFAULT_TIMEOUT,
    }
}

#[test]
fn prefix_matches____segment_boundaries____only_whole_segments() {
    assert!(prefix_matches("/orders", "/orders"));
    assert!(prefix_matches("/orders", "/orders/7"));
    assert!(!prefix_matches("/orders", "/ordersx"));
    assert!(prefix_matches("/", "/anything"));
    assert!(prefix_matches("/api/", "/api/x"));
    assert!(!prefix_matches("/orders", "/"));
}

#[test]
fn pick____several_routes____longest_prefix_then_named_host_wins() {
    let table = BTreeMap::from([
        ("root".to_string(), vec![route(None, "/")]),
        ("orders".to_string(), vec![route(None, "/orders")]),
        (
            "shop".to_string(),
            vec![route(Some("shop.example"), "/orders")],
        ),
    ]);

    let any = pick(&table, Some("other.example"), "/orders/1").unwrap().0;
    let named = pick(&table, Some("shop.example"), "/orders/1").unwrap().0;
    let fallback = pick(&table, None, "/health").unwrap().0;

    assert_eq!(any, "orders");
    assert_eq!(named, "shop");
    assert_eq!(fallback, "root");
}

#[test]
fn conflict____same_host_and_prefix_of_another_app____that_app_and_route() {
    let table = BTreeMap::from([(
        "shop".to_string(),
        vec![route(Some("shop.example"), "/orders")],
    )]);
    let wanted = [route(None, "/x"), route(Some("shop.example"), "/orders")];

    let found = conflict(&table, "other", &wanted);

    assert_eq!(found, Some(("shop", &wanted[1])));
}

#[test]
fn conflict____own_routes_or_another_host____none() {
    let table = BTreeMap::from([(
        "shop".to_string(),
        vec![route(Some("shop.example"), "/orders")],
    )]);
    let own = [route(Some("shop.example"), "/orders")];
    let other_host = [
        route(Some("other.example"), "/orders"),
        route(None, "/orders"),
    ];

    let replacing = conflict(&table, "shop", &own);
    let beside = conflict(&table, "other", &other_host);

    assert_eq!(replacing, None);
    assert_eq!(beside, None);
}

const FORMATS: &str = include_str!("../../../docs/GUEST_FORMATS.md");

#[test]
fn routes_json____documented_example____same_table() {
    let fence = "```json routes\n";
    let start = FORMATS.find(fence).unwrap() + fence.len();
    let example: serde_json::Value =
        serde_json::from_str(&FORMATS[start..start + FORMATS[start..].find("```").unwrap()])
            .unwrap();
    let table = BTreeMap::from([(
        "shop".to_string(),
        vec![HttpRoute {
            host: None,
            path_prefix: "/shop".into(),
            hash_header: Some("x-customer".into()),
            max_body: 1_048_576,
            timeout: Duration::from_secs(30),
        }],
    )]);

    let json: serde_json::Value = serde_json::from_str(&routes_json(&table)).unwrap();

    assert_eq!(json, example);
}
