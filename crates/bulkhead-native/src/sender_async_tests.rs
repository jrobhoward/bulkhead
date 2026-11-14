#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::os::fd::AsFd;
use std::time::Duration;

use super::*;
use crate::channel::{AppEnd, Channel, bell, door, wake_reader};
use crate::protocol::FromApp;

/// Play the node: read `n` calls from the app, wait `delay` (the app's
/// pump sleeps on its door meanwhile), then answer them last first, each
/// with its own message's body. The door is handed back, to close once
/// the test is done: its end of file tells the app the node has gone.
fn node(
    channel: Channel,
    door_write: std::os::fd::OwnedFd,
    n: usize,
    delay: Duration,
) -> std::os::fd::OwnedFd {
    let mut buf = Vec::new();
    let mut calls = Vec::new();
    let mut chunk = [0u8; 4096];
    while calls.len() < n {
        let got = channel.to_node().read(&mut chunk).unwrap();
        buf.extend_from_slice(&chunk[..got]);
        while buf.len() >= 4 {
            let len = protocol::frame_len(buf[..4].try_into().unwrap()).unwrap();
            if buf.len() < 4 + len {
                break;
            }
            match protocol::decode::<FromApp>(&buf[4..4 + len]).unwrap() {
                FromApp::Request {
                    id,
                    request: Request::CallApp { msg, .. },
                } => calls.push((id, msg)),
                other => panic!("not a call: {other:?}"),
            }
            buf.drain(..4 + len);
        }
        if got == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    std::thread::sleep(delay);
    for (id, msg) in calls.into_iter().rev() {
        let frame = protocol::encode(&ToApp::Answer {
            id,
            answer: Answer::Called(Ok(msg)),
        });
        channel.to_app().write(&frame).unwrap();
        wake_reader(channel.to_app().reader_waiting(), door_write.as_fd());
    }
    door_write
}

fn text(s: &str) -> Envelope {
    Envelope {
        content_type: "text/plain".into(),
        schema: None,
        body: s.as_bytes().to_vec(),
    }
}

#[test]
fn async_sender____calls_answered_out_of_order_after_it_slept____each_gets_its_own() {
    let (channel, memfd) = Channel::create().unwrap();
    let (_bell_read, bell_write) = bell().unwrap();
    let (door_read, door_write) = door().unwrap();
    let end = AppEnd::sender(
        memfd,
        bell_write,
        door_read,
        Spin(Duration::from_micros(50)),
    )
    .unwrap();
    let node = std::thread::spawn(move || node(channel, door_write, 3, Duration::from_millis(50)));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();

    let answers = local.block_on(&rt, async {
        let sender = AsyncSender::new(Sender::for_tests(end)).unwrap();
        tokio::task::spawn_local(sender.clone().pump());
        let calls = ["a", "b", "c"].map(|s| sender.call_app("app", text(s), 1_000));
        let mut answers = Vec::new();
        for call in calls {
            answers.push(call.await.unwrap().body);
        }
        (answers, sender.in_flight())
    });

    node.join().unwrap();
    assert_eq!(answers.0, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    assert_eq!(answers.1, 0);
}

#[test]
fn async_sender____a_call_dropped_before_its_answer____the_answer_is_dropped_and_others_come() {
    let (channel, memfd) = Channel::create().unwrap();
    let (_bell_read, bell_write) = bell().unwrap();
    let (door_read, door_write) = door().unwrap();
    let end = AppEnd::sender(memfd, bell_write, door_read, Spin(Duration::ZERO)).unwrap();
    let node = std::thread::spawn(move || node(channel, door_write, 2, Duration::from_millis(20)));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();

    let (kept, left) = local.block_on(&rt, async {
        let sender = AsyncSender::new(Sender::for_tests(end)).unwrap();
        tokio::task::spawn_local(sender.clone().pump());
        let dropped = sender.call_app("app", text("dropped"), 1_000);
        let kept = sender.call_app("app", text("kept"), 1_000);
        drop(dropped);
        let kept = kept.await.unwrap().body;
        tokio::time::sleep(Duration::from_millis(20)).await;
        (kept, sender.in_flight())
    });

    node.join().unwrap();
    assert_eq!(kept, b"kept".to_vec());
    assert_eq!(left, 0);
}
