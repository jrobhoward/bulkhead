//! Linux and FreeBSD only: the link is.
#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::time::Duration;

use rstest::rstest;

use bulkhead_native::protocol::{self, FromApp};

use super::{Next, frame_room, hinted_idle, next_frame, too_new, turn};
use crate::native::port::Incoming;

#[test]
fn turn____nothing_between____one_count_apart() {
    let before = turn();

    let after = turn();

    assert_eq!(after, (before.0, before.1 + 1));
}

#[test]
fn turn____another_turn_between____more_than_one_apart() {
    let before = turn();

    turn();
    let after = turn();

    assert_ne!(after, (before.0, before.1 + 1));
}

#[test]
fn turn____another_thread____other_counter() {
    let here = turn();

    let there = std::thread::spawn(turn).join().unwrap();

    assert_ne!(there.0, here.0);
}

#[rstest]
#[case::idle(true, 100, true)]
#[case::called_back_to_back(true, 5, false)]
#[case::more_queued(false, 100, false)]
fn hinted_idle____queue_and_gap____idle_only_when_both_say_so(
    #[case] nothing_queued: bool,
    #[case] since_us: u64,
    #[case] idle: bool,
) {
    let spin = Duration::from_micros(20);

    let hinted = hinted_idle(nothing_queued, Duration::from_micros(since_us), spin);

    assert_eq!(hinted, idle);
}

#[test]
fn too_new____a_frame_newer_than_the_app____refused_naming_both() {
    use bulkhead_native::protocol::{FdRef, NetStream, ToApp};
    let connection = ToApp::Connection {
        listener: "l".into(),
        stream: NetStream {
            socket: 1,
            fd: FdRef(1),
            local: "127.0.0.1:1".parse().unwrap(),
            peer: "127.0.0.1:2".parse().unwrap(),
        },
    };

    let old = too_new(&connection, 10);
    let current = too_new(&connection, 11);
    let ping = too_new(&ToApp::Ping { id: 1 }, 10);

    assert_eq!(
        old.as_deref(),
        Some("the app speaks protocol version 10, which has no connection frame (version 11)")
    );
    assert_eq!(current, None);
    assert_eq!(ping, None);
}

/// Two frames, a pong and a log, as the app writes them.
fn two_frames() -> (Vec<u8>, usize) {
    let first = protocol::encode(&FromApp::Pong { id: 7 });
    let mut bytes = first.clone();
    bytes.extend(protocol::encode(&FromApp::Log {
        level: protocol::Level::Info,
        message: "hello".into(),
    }));
    (bytes, first.len())
}

#[test]
fn next_frame____a_whole_frame____it_and_its_length() {
    let (bytes, first) = two_frames();

    let next = next_frame(&bytes);

    assert!(
        matches!(next, Next::Ready { incoming: Incoming::Frame(FromApp::Pong { id: 7 }), len } if len == first),
        "{next:?}"
    );
}

#[rstest]
#[case::nothing(0)]
#[case::part_of_the_prefix(3)]
#[case::the_prefix_alone(4)]
#[case::all_but_the_last_byte(usize::MAX)]
fn next_frame____a_frame_cut_short____need(#[case] keep: usize) {
    let frame = protocol::encode(&FromApp::Pong { id: 7 });
    let keep = keep.min(frame.len() - 1);

    let next = next_frame(&frame[..keep]);

    assert!(matches!(next, Next::Need), "{next:?}");
}

#[test]
fn next_frame____a_length_over_the_largest_frame____broken() {
    let too_long = u32::try_from(protocol::MAX_FRAME + 1)
        .unwrap()
        .to_be_bytes();

    let next = next_frame(&too_long);

    assert!(matches!(next, Next::Broken(_)), "{next:?}");
}

#[test]
fn next_frame____a_body_that_does_not_decode____broken() {
    let mut bytes = 3u32.to_be_bytes().to_vec();
    bytes.extend([0xff, 0xff, 0xff]);

    let next = next_frame(&bytes);

    assert!(matches!(next, Next::Broken(_)), "{next:?}");
}

#[rstest]
#[case::no_prefix_yet(vec![0, 0], None)]
#[case::prefix_alone(1_000u32.to_be_bytes().to_vec(), Some(1_000))]
#[case::part_of_the_body({ let mut v = 1_000u32.to_be_bytes().to_vec(); v.extend([0; 600]); v }, Some(400))]
#[case::whole_frame_and_more({ let mut v = 4u32.to_be_bytes().to_vec(); v.extend([0; 10]); v }, None)]
#[case::over_the_largest_frame(u32::MAX.to_be_bytes().to_vec(), None)]
fn frame_room____what_is_in____the_rest_of_the_frame(
    #[case] have: Vec<u8>,
    #[case] want: Option<usize>,
) {
    let got = frame_room(&have);

    assert_eq!(got, want);
}
