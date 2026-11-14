#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;
use crate::key::generate_key;

const NOW: u64 = 1_800_000_000_000;

struct Setup {
    signer: Signer,
    node: PublicKey,
    operators: BTreeMap<String, Vec<PublicKey>>,
    nonces: Nonces,
}

fn setup() -> Setup {
    let (pem, _) = generate_key().unwrap();
    let signer = Signer::from_pem(pem.as_bytes()).unwrap();
    let (_, node) = generate_key().unwrap();
    let operators = BTreeMap::from([("alice".to_string(), vec![signer.public().clone()])]);
    Setup {
        signer,
        node,
        operators,
        nonces: Nonces::default(),
    }
}

fn signed<'a>(node: &'a PublicKey, body: &[u8]) -> Signed<'a> {
    Signed {
        node,
        method: "POST",
        target: "/v1/apps/shop/stop",
        body: body_hash(body),
    }
}

#[test]
fn verify____signed_request____the_operator_named() {
    let s = setup();
    let request = signed(&s.node, b"{}");
    let header = s.signer.header_at(&request, NOW, [7; 16]);

    let got = verify(
        Some(&header),
        &request,
        NOW + 1_000,
        &s.operators,
        &s.nonces,
    )
    .unwrap();

    assert_eq!(got.operator, "alice");
    assert_eq!(&got.key, s.signer.public());
}

#[test]
fn verify____no_header____unsigned() {
    let s = setup();

    let got = verify(None, &signed(&s.node, b""), NOW, &s.operators, &s.nonces);

    assert_eq!(got, Err(Refused::Unsigned));
}

#[test]
fn verify____a_key_no_operator_has____unknown() {
    let s = setup();
    let (pem, _) = generate_key().unwrap();
    let stranger = Signer::from_pem(pem.as_bytes()).unwrap();
    let request = signed(&s.node, b"");
    let header = stranger.header_at(&request, NOW, [1; 16]);

    let got = verify(Some(&header), &request, NOW, &s.operators, &s.nonces);

    assert!(matches!(got, Err(Refused::Unknown(_))), "{got:?}");
}

#[test]
fn verify____signed_over_a_minute_ago____stale() {
    let s = setup();
    let request = signed(&s.node, b"");
    let header = s.signer.header_at(&request, NOW, [2; 16]);

    let got = verify(
        Some(&header),
        &request,
        NOW + 61_000,
        &s.operators,
        &s.nonces,
    );

    assert!(matches!(got, Err(Refused::Stale { .. })), "{got:?}");
}

#[test]
fn verify____the_same_request_twice____replayed() {
    let s = setup();
    let request = signed(&s.node, b"");
    let header = s.signer.header_at(&request, NOW, [3; 16]);
    verify(Some(&header), &request, NOW, &s.operators, &s.nonces).unwrap();

    let again = verify(Some(&header), &request, NOW + 10, &s.operators, &s.nonces);

    assert_eq!(again, Err(Refused::Replayed));
}

#[rstest::rstest]
#[case::another_body(|r: &mut Signed<'_>| r.body = body_hash(b"other"))]
#[case::another_method(|r: &mut Signed<'_>| r.method = "DELETE")]
#[case::another_target(|r: &mut Signed<'_>| r.target = "/v1/apps/bank/stop")]
fn verify____request_changed_after_signing____bad_signature(#[case] change: fn(&mut Signed<'_>)) {
    let s = setup();
    let request = signed(&s.node, b"{}");
    let header = s.signer.header_at(&request, NOW, [4; 16]);
    let mut changed = request;
    change(&mut changed);

    let got = verify(Some(&header), &changed, NOW, &s.operators, &s.nonces);

    assert_eq!(got, Err(Refused::BadSignature));
}

#[test]
fn verify____signed_for_another_node____bad_signature() {
    let s = setup();
    let (_, other) = generate_key().unwrap();
    let header = s.signer.header_at(&signed(&other, b""), NOW, [5; 16]);

    let got = verify(
        Some(&header),
        &signed(&s.node, b""),
        NOW,
        &s.operators,
        &s.nonces,
    );

    assert_eq!(got, Err(Refused::BadSignature));
}

#[test]
fn verify____a_bad_signature____does_not_spend_the_nonce() {
    let s = setup();
    let request = signed(&s.node, b"{}");
    let header = s.signer.header_at(&request, NOW, [6; 16]);
    let mut forged = request;
    forged.target = "/v1/apps/bank/stop";
    let _ = verify(Some(&header), &forged, NOW, &s.operators, &s.nonces);

    let real = verify(Some(&header), &request, NOW, &s.operators, &s.nonces);

    assert!(real.is_ok(), "{real:?}");
}

#[rstest::rstest]
#[case("v2 key=x ts=1 nonce=00 sig=AA")]
#[case("v1 ts=1 nonce=00000000000000000000000000000000 sig=AA")]
#[case("v1 key=ed25519:AAAA ts=x nonce=00 sig=AA")]
fn verify____malformed_header____malformed(#[case] header: &str) {
    let s = setup();

    let got = verify(
        Some(header),
        &signed(&s.node, b""),
        NOW,
        &s.operators,
        &s.nonces,
    );

    assert!(matches!(got, Err(Refused::Malformed(_))), "{got:?}");
}

#[test]
fn BodyHash____in_chunks____the_whole_body_s_hash() {
    let mut h = BodyHash::default();
    h.update(b"hello ");
    h.update(b"world");

    assert_eq!(h.finish(), body_hash(b"hello world"));
}

#[test]
fn Signer____header____fresh_nonce_each_time() {
    let s = setup();
    let request = signed(&s.node, b"");

    let a = s.signer.header(&request).unwrap();
    let b = s.signer.header(&request).unwrap();

    assert_ne!(field(&a, "nonce").unwrap(), field(&b, "nonce").unwrap());
}
