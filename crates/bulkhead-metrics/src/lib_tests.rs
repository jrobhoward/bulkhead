#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn from_args____empty____defaults() {
    assert_eq!(Config::from_args(b"").unwrap(), Config::default());
}

#[test]
fn from_args____limits____read() {
    let config = Config::from_args(br#"{"max_connections": 8, "header_timeout_ms": 500}"#).unwrap();

    assert_eq!((config.max_connections, config.header_timeout_ms), (8, 500));
}

#[test]
fn from_args____no_connection_or_unknown_key____refused() {
    assert!(Config::from_args(br#"{"max_connections": 0}"#).is_err());
    assert!(Config::from_args(br#"{"port": 9100}"#).is_err());
}
