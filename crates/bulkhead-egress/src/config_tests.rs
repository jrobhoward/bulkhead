#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

fn args(extra: &str) -> String {
    format!(
        r#"{{"app": "shop", "connect_timeout_ms": 1000, "pool_idle_timeout_ms": 2000,
            "pool_max_idle_per_host": 4, "max_response_body": 1024{extra}}}"#
    )
}

#[test]
fn from_args____the_required_fields____no_roots() {
    let config = Config::from_args(args("").as_bytes()).unwrap();

    assert_eq!(config.app, "shop");
    assert_eq!(config.connect_timeout(), Duration::from_secs(1));
    assert_eq!(config.pool_idle_timeout(), Duration::from_secs(2));
    assert!(config.roots().unwrap().is_empty());
}

#[test]
fn from_args____an_unknown_field____refused() {
    let err = Config::from_args(args(r#", "proxy": "x""#).as_bytes()).unwrap_err();

    assert!(err.contains("unknown field"), "{err}");
}

#[test]
fn roots____two_certificates____both_trusted() {
    let pem = |name: &str| {
        rcgen::generate_simple_self_signed(vec![name.to_string()])
            .unwrap()
            .cert
            .pem()
    };
    let roots = format!("{}{}", pem("a.test"), pem("b.test"));
    let config = Config::from_args(
        args(&format!(
            r#", "roots_pem": {}"#,
            serde_json::to_string(&roots).unwrap()
        ))
        .as_bytes(),
    )
    .unwrap();

    assert_eq!(config.roots().unwrap().len(), 2);
}

#[test]
fn roots____not_a_certificate____refused() {
    let config = Config::from_args(
        args(r#", "roots_pem": "-----BEGIN CERTIFICATE-----\nnot base64!\n-----END CERTIFICATE-----\n""#)
            .as_bytes(),
    )
    .unwrap();

    assert!(config.roots().is_err());
}
