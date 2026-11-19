#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;
use crate::node::store::NewSideState;

fn sha(c: char) -> String {
    std::iter::repeat_n(c, 64).collect()
}

fn app(current: &str, previous: Option<&str>) -> AppState {
    AppState {
        current: current.into(),
        previous: previous.map(Into::into),
        running: true,
        new_side: None,
        quarantined: None,
    }
}

fn state(apps: &[(&str, AppState)]) -> State {
    State {
        order: apps.iter().map(|(n, _)| (*n).to_string()).collect(),
        apps: apps
            .iter()
            .map(|(n, a)| ((*n).to_string(), a.clone()))
            .collect(),
    }
}

/// `state` shipped, every bundle of digest `c` and 10 bytes.
fn shipped_of(state: &State, c: char) -> Shipped {
    shipped(state, &HashMap::new(), |_, _| {
        Some(BundleRef {
            sha: sha(c),
            size: 10,
        })
    })
}

#[test]
fn shipped____current_previous_and_new_side____each_bundle_and_the_grant() {
    let mut orders = app("1.1.0", Some("1.0.0"));
    orders.new_side = Some(NewSideState {
        version: "1.2.0".into(),
        percent: 10,
    });
    let st = state(&[("orders", orders)]);
    let info = HashMap::from([(
        "orders".to_string(),
        DeployInfo {
            granted: vec!["bucket:points".into()],
            ..DeployInfo::default()
        },
    )]);

    let s = shipped(&st, &info, |_, v| {
        (v != "1.0.0").then(|| BundleRef {
            sha: sha('a'),
            size: 1,
        })
    });

    let a = &s.apps["orders"];
    assert_eq!(
        a.bundles.keys().collect::<Vec<_>>(),
        ["1.1.0", "1.2.0"],
        "a version whose bundle is gone is left out"
    );
    assert_eq!(a.granted, ["bucket:points"]);
    assert_eq!(s.order, ["orders"]);
}

#[test]
fn digest____same_state____same_digest_and_another_for_a_change() {
    let one = shipped_of(&state(&[("orders", app("1.0.0", None))]), 'a');
    let again = shipped_of(&state(&[("orders", app("1.0.0", None))]), 'a');
    let other = shipped_of(&state(&[("orders", app("1.0.0", None))]), 'b');

    assert_eq!(digest(&one), digest(&again));
    assert_ne!(digest(&one), digest(&other));
    assert!(is_sha(&digest(&one)));
}

#[test]
fn check____a_well_formed_state____accepted() {
    let s = shipped_of(&state(&[("orders", app("1.1.0", Some("1.0.0")))]), 'a');

    assert_eq!(check(&s, 10), Ok(()));
}

#[test]
fn check____a_name_that_is_a_path____refused() {
    let s = shipped_of(&state(&[("../etc", app("1.0.0", None))]), 'a');

    assert!(check(&s, 10).unwrap_err().contains("not an app name"));
}

#[test]
fn check____a_version_that_is_a_path____refused() {
    let s = shipped_of(&state(&[("orders", app("../../x", None))]), 'a');

    assert!(check(&s, 10).unwrap_err().contains("not an app name"));
}

#[test]
fn check____order_not_the_apps____refused() {
    let mut s = shipped_of(&state(&[("orders", app("1.0.0", None))]), 'a');
    s.order.push("orders".into());

    assert!(check(&s, 10).unwrap_err().contains("deploy order"));
}

#[test]
fn check____current_version_not_shipped____refused() {
    let mut s = shipped_of(&state(&[("orders", app("1.0.0", None))]), 'a');
    s.apps.get_mut("orders").unwrap().bundles.clear();

    assert!(check(&s, 10).unwrap_err().contains("no bundle"));
}

#[test]
fn check____bundle_over_the_most____refused() {
    let s = shipped_of(&state(&[("orders", app("1.0.0", None))]), 'a');

    assert!(check(&s, 9).unwrap_err().contains("over the 9"));
}

#[test]
fn check____malformed_digest____refused() {
    let mut s = shipped_of(&state(&[("orders", app("1.0.0", None))]), 'a');
    s.apps
        .get_mut("orders")
        .unwrap()
        .bundles
        .get_mut("1.0.0")
        .unwrap()
        .sha = "../x".into();

    assert!(check(&s, 10).unwrap_err().contains("malformed digest"));
}

#[test]
fn needs____some_held_with_the_same_digest____only_the_others() {
    let s = shipped_of(
        &state(&[
            ("orders", app("1.1.0", Some("1.0.0"))),
            ("points", app("2.0.0", None)),
        ]),
        'a',
    );

    let n = needs(&s, |app, v| match (app, v) {
        ("orders", "1.0.0") => Some(sha('a')),
        ("points", "2.0.0") => Some(sha('b')),
        _ => None,
    });

    let got: Vec<(&str, &str)> = n
        .iter()
        .map(|n| (n.app.as_str(), n.version.as_str()))
        .collect();
    assert_eq!(got, [("orders", "1.1.0"), ("points", "2.0.0")]);
}

#[test]
fn gone____app_removed_and_version_dropped____both_listed() {
    let held = state(&[
        ("orders", app("1.1.0", Some("1.0.0"))),
        ("old", app("1.0.0", None)),
    ]);
    let s = shipped_of(&state(&[("orders", app("1.2.0", Some("1.1.0")))]), 'a');

    let (apps, versions) = gone(&held, &s);

    assert_eq!(apps, ["old"]);
    assert_eq!(versions, [("orders".to_string(), "1.0.0".to_string())]);
}

#[test]
fn grants_differ____same_grants____none() {
    let g = vec!["bucket:points".to_string(), "timers".to_string()];

    assert_eq!(grants_differ(&g, &g), None);
}

#[test]
fn grants_differ____lacking_and_extra____both_named() {
    let primary = vec!["secret:stripe".to_string(), "timers".to_string()];
    let here = vec!["timers".to_string(), "http_client".to_string()];

    assert_eq!(
        grants_differ(&primary, &here).unwrap(),
        "not granted here: secret:stripe; granted here only: http_client"
    );
}

#[test]
fn Digests____file_changed____digest_follows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.rbp");
    std::fs::write(&path, b"abc").unwrap();
    let d = Digests::default();

    let first = d.of(&path).unwrap();
    std::fs::write(&path, b"abcd").unwrap();
    let second = d.of(&path).unwrap();

    assert_eq!(
        first.sha,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(first.size, 3);
    assert_ne!(second.sha, first.sha);
    assert_eq!(second.size, 4);
}

#[test]
fn read_chunk____past_the_first_mib____the_rest_and_the_length() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.rbp");
    let len = usize::try_from(CHUNK).unwrap() + 5;
    std::fs::write(&path, vec![7u8; len]).unwrap();

    let (first, total) = read_chunk(&path, 0).unwrap();
    let (rest, _) = read_chunk(&path, CHUNK).unwrap();

    assert_eq!(total, CHUNK + 5);
    assert_eq!(first.len(), usize::try_from(CHUNK).unwrap());
    assert_eq!(rest.len(), 5);
}

#[test]
fn DeployMsg____chunk____round_trips_as_cbor_bytes() {
    let msg = DeployMsg::Chunk {
        app: "orders".into(),
        version: "1.0.0".into(),
        sha: sha('a'),
        offset: 0,
        total: 3,
        bytes: vec![1, 2, 3],
    };

    let body = to_cbor(&msg).unwrap();

    assert_eq!(from_cbor::<DeployMsg>(&body).unwrap(), msg);
    assert!(
        body.windows(4).any(|w| w == [0x43, 1, 2, 3]),
        "a byte string"
    );
}
