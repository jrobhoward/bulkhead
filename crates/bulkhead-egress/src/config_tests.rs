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
    assert_eq!(config.threads, 4);
}

#[test]
fn from_args____threads_out_of_range____refused() {
    for extra in [r#", "threads": 0"#, r#", "threads": 17"#] {
        let err = Config::from_args(args(extra).as_bytes()).unwrap_err();

        assert!(err.contains("threads must be 1 to 16"), "{err}");
    }
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

#[test]
fn identity____none_given____none() {
    let config = Config::from_args(args("").as_bytes()).unwrap();

    let id = config
        .identity(|_| unreachable!("nothing to read"))
        .unwrap();

    assert!(id.is_none());
}

#[test]
fn identity____a_certificate_and_its_key____read_through_the_reader() {
    let made = rcgen::generate_simple_self_signed(vec!["client".to_string()]).unwrap();
    let (cert_pem, key_pem) = (made.cert.pem(), made.signing_key.serialize_pem());
    let config = Config::from_args(
        args(r#", "client_cert": "/driver/c.pem", "client_key": "/driver/k.pem""#).as_bytes(),
    )
    .unwrap();

    let id = config
        .identity(|path| match path {
            "/driver/c.pem" => Ok(cert_pem.clone().into_bytes()),
            "/driver/k.pem" => Ok(key_pem.clone().into_bytes()),
            other => panic!("read {other}"),
        })
        .unwrap();

    let (chain, _key) = id.unwrap();
    assert_eq!(chain.len(), 1);
}

#[test]
fn identity____only_a_certificate_or_not_a_key____refused() {
    let half = Config::from_args(args(r#", "client_cert": "/driver/c.pem""#).as_bytes()).unwrap();
    let both = Config::from_args(
        args(r#", "client_cert": "/driver/c.pem", "client_key": "/driver/k.pem""#).as_bytes(),
    )
    .unwrap();
    let made = rcgen::generate_simple_self_signed(vec!["client".to_string()]).unwrap();
    let cert_pem = made.cert.pem();

    let half = half.identity(|_| Ok(Vec::new())).unwrap_err();
    let bad_key = both
        .identity(|p| {
            Ok(if p.ends_with("c.pem") {
                cert_pem.clone().into_bytes()
            } else {
                b"not a key".to_vec()
            })
        })
        .unwrap_err();

    assert!(half.contains("go together"), "{half}");
    assert!(bad_key.contains("client key"), "{bad_key}");
}
