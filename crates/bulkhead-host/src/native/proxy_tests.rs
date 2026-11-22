#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

#[test]
fn request____a_name_and_an_ipv6_address____the_target_in_both_lines() {
    assert_eq!(
        request("api.example.com", 443),
        "CONNECT api.example.com:443 HTTP/1.1\r\nHost: api.example.com:443\r\n\r\n"
    );
    assert_eq!(authority("fd00::1", 8080), "[fd00::1]:8080");
}

#[rstest]
#[case("HTTP/1.1 200 Connection established\r\n\r\n", true)]
#[case("HTTP/1.0 200 OK\r\nProxy-Agent: x\r\n\r\n", true)]
#[case("HTTP/1.1 204 No Content\r\n\r\n", true)]
#[case("HTTP/1.1 403 Forbidden\r\n\r\n", false)]
#[case("HTTP/1.1 407 Proxy Authentication Required\r\n\r\n", false)]
#[case("SSH-2.0-OpenSSH\r\n\r\n", false)]
#[case("HTTP/2 200\r\n\r\n", false)]
fn check_reply____a_head____2xx_grants(#[case] head: &str, #[case] granted: bool) {
    assert_eq!(check_reply(head.as_bytes()).is_ok(), granted);
}

#[test]
fn check_reply____refused____the_status_line_says_why() {
    let why = check_reply(b"HTTP/1.1 403 Forbidden\r\n\r\n").unwrap_err();

    assert_eq!(why, "HTTP/1.1 403 Forbidden");
}
