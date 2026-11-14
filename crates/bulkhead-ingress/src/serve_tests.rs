#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use bulkhead_native::protocol::ExitReason;
use rstest::rstest;

use super::*;

fn reply(r: &Response) -> Envelope {
    r.to_envelope().into()
}

fn text_of(r: Reply) -> String {
    let body = r.into_body();
    let bytes = futures_body(body);
    String::from_utf8(bytes).unwrap()
}

/// A `Full` body's bytes, without a runtime.
fn futures_body(body: Full<Bytes>) -> Vec<u8> {
    use http_body_util::BodyExt;
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(body.collect()).unwrap().to_bytes().to_vec()
}

#[test]
fn host_of____header_with_port_any_case____lowercase_without_port() {
    let header = HeaderValue::from_static("Shop.Example:8443");

    let host = host_of(Some(&header), &Uri::from_static("/x"));
    let from_target = host_of(None, &Uri::from_static("http://api.example/x"));
    let none = host_of(None, &Uri::from_static("/x"));

    assert_eq!(host.as_deref(), Some("shop.example"));
    assert_eq!(from_target.as_deref(), Some("api.example"));
    assert_eq!(none, None);
}

#[test]
fn redirect____path_and_query____307_under_the_partner_url() {
    let r = redirect("https://node2:8443", &Uri::from_static("/shop/cart?id=7"));

    assert_eq!(r.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(r.headers()[LOCATION], "https://node2:8443/shop/cart?id=7");
}

#[test]
fn from_call____app_response____its_status_headers_and_body() {
    let app = Response::new(201).header("x-a", "1").body("made");

    let r = from_call(Ok(reply(&app)), 1 << 20);

    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(r.headers()["x-a"], "1");
    assert_eq!(text_of(r), "made");
}

#[test]
fn from_call____reply_over_max_response____502() {
    let app = Response::new(200).body(vec![0u8; 100]);

    let r = from_call(Ok(reply(&app)), 10);

    assert_eq!(r.status(), StatusCode::BAD_GATEWAY);
}

#[test]
fn from_call____header_http_does_not_allow____502() {
    let app = Response::new(200).header("bad header", "x");

    let r = from_call(Ok(reply(&app)), 1 << 20);

    assert_eq!(r.status(), StatusCode::BAD_GATEWAY);
}

#[rstest]
#[case::noproc(CallError::Noproc, 503)]
#[case::noconnection(CallError::Noconnection, 503)]
#[case::timeout(CallError::Timeout, 504)]
#[case::too_large(CallError::TooLarge, 413)]
#[case::access_denied(CallError::AccessDenied, 403)]
#[case::exited(CallError::Exited(ExitReason::Normal), 502)]
fn from_call____call_error____its_status(#[case] e: CallError, #[case] code: u16) {
    let r = from_call(Err(e), 1 << 20);

    assert_eq!(r.status().as_u16(), code);
}

#[test]
fn from_call____overloaded____503_with_retry_after() {
    let r = from_call(Err(CallError::Overloaded), 1 << 20);

    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(r.headers()[RETRY_AFTER], "1");
}
