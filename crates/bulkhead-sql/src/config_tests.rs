#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

fn args(extra: &str) -> Vec<u8> {
    format!(
        r#"{{"datasource": "main", "host": "db.internal", "transport": "tcp",
            "dbname": "shop", "user": "shop", "pool_size": 4,
            "connect_timeout_ms": 5000, "acquire_timeout_ms": 2000,
            "statement_timeout_ms": 30000, "max_result_bytes": 1024{extra}}}"#
    )
    .into_bytes()
}

#[test]
fn from_args____the_required_fields____no_password_and_no_tls() {
    let config = Config::from_args(&args("")).unwrap();

    assert_eq!(config.transport, Transport::Tcp);
    assert_eq!(config.password, None);
    assert_eq!(config.tls, Tls::Disable);
    assert_eq!(config.acquire_timeout(), Duration::from_secs(2));
}

#[test]
fn from_args____a_password_file_and_tls____both_read() {
    let config = Config::from_args(&args(
        r#", "password": {"file": "/driver/main.password"},
            "tls": {"require": {"ca_file": "/driver/ca.pem"}}"#,
    ))
    .unwrap();

    assert_eq!(
        config.password,
        Some(Password::File("/driver/main.password".into()))
    );
    assert_eq!(
        config.tls,
        Tls::Require {
            ca_file: "/driver/ca.pem".into()
        }
    );
}

#[rstest]
#[case::unknown_field(r#", "hostaddr": "10.0.0.1""#, "unknown field")]
#[case::unknown_password(r#", "password": {"env": "PGPASSWORD"}"#, "unknown variant")]
fn from_args____malformed____refused(#[case] extra: &str, #[case] says: &str) {
    let got = Config::from_args(&args(extra)).unwrap_err();

    assert!(got.contains(says), "{got}");
}

#[test]
fn from_args____a_pool_of_none____refused() {
    let text = String::from_utf8(args(""))
        .unwrap()
        .replace(r#""pool_size": 4"#, r#""pool_size": 0"#);

    let got = Config::from_args(text.as_bytes()).unwrap_err();

    assert!(got.contains("pool_size is 0"), "{got}");
}

#[rstest]
#[case::default("", 4)]
#[case::fewer(r#", "threads": 2"#, 2)]
#[case::no_more_than_the_pool(r#", "threads": 8"#, 4)]
fn workers____threads_and_pool____the_smaller(#[case] extra: &str, #[case] want: usize) {
    let config = Config::from_args(&args(extra)).unwrap();

    assert_eq!(config.workers(), want);
}

#[rstest]
#[case::none(r#", "threads": 0"#)]
#[case::too_many(r#", "threads": 17"#)]
fn from_args____threads_out_of_range____refused(#[case] extra: &str) {
    let got = Config::from_args(&args(extra)).unwrap_err();

    assert!(got.contains("threads must be 1 to 16"), "{got}");
}

#[test]
fn from_args____tls_over_a_unix_socket____refused() {
    let text = String::from_utf8(args(
        r#", "tls": {"require": {"ca_file": "/driver/ca.pem"}}"#,
    ))
    .unwrap()
    .replace(r#""transport": "tcp""#, r#""transport": "unix""#);

    let got = Config::from_args(text.as_bytes()).unwrap_err();

    assert!(got.contains("TLS over a Unix socket"), "{got}");
}

#[test]
fn password____debug____the_value_not_shown() {
    let shown = format!("{:?}", Password::Value("hunter2".into()));

    assert!(!shown.contains("hunter2"), "{shown}");
}

#[rstest]
#[case(b"secret\n", b"secret")]
#[case(b"secret\r\n", b"secret")]
#[case(b"secret", b"secret")]
#[case(b"\n", b"")]
fn trim_password____trailing_newlines____dropped(#[case] raw: &[u8], #[case] want: &[u8]) {
    assert_eq!(trim_password(raw), want);
}
