#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

#[rstest]
#[case("post hello", Command::Post("hello"))]
#[case("post  two spaces", Command::Post(" two spaces"))]
#[case("recent", Command::Recent)]
#[case("clear", Command::Clear)]
#[case("seen", Command::Seen)]
#[case("post", Command::Unknown)]
#[case("", Command::Unknown)]
fn command____text____what_it_asks(#[case] text: &str, #[case] expected: Command<'_>) {
    let found = command(text);

    assert_eq!(found, expected);
}

#[test]
fn describe____a_new_post____its_key_and_text() {
    let found = describe("p000003", Some(b"hi all"));

    assert_eq!(found.as_deref(), Some("new post p000003: hi all"));
}

#[test]
fn describe____a_deleted_post____says_removed() {
    let found = describe("p000003", None);

    assert_eq!(found.as_deref(), Some("post p000003 removed"));
}

#[test]
fn describe____the_counter____not_a_post() {
    let found = describe("seq", Some(b"3"));

    assert_eq!(found, None);
}
