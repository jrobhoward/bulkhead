#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn from_args____paths____read_with_the_default_cap() {
    let config =
        Config::from_args(br#"{"key": "/driver/admin.key", "operators": "/driver/ops.toml"}"#)
            .unwrap();

    assert_eq!(config.key, "/driver/admin.key");
    assert_eq!(config.max_connections, 64);
}

#[test]
fn from_args____no_connection_or_missing_key____refused() {
    assert!(Config::from_args(br#"{"key": "k", "operators": "o", "max_connections": 0}"#).is_err());
    assert!(Config::from_args(br#"{"operators": "o"}"#).is_err());
}
