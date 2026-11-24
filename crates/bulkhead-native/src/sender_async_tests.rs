#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::os::fd::AsFd;
use std::time::Duration;

use super::*;
use crate::channel::{AppEnd, Channel, bell, door, wake_reader, wake_writer};
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
        trace: None,
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

#[test]
fn async_sender____casts_queued_and_refused____each_awaits_its_own_answer() {
    let (channel, memfd) = Channel::create().unwrap();
    let (_bell_read, bell_write) = bell().unwrap();
    let (door_read, door_write) = door().unwrap();
    let end = AppEnd::sender(memfd, bell_write, door_read, Spin(Duration::ZERO)).unwrap();
    // The node: the first cast queued, the second refused (a full mailbox).
    let node = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut answered = 0;
        while answered < 2 {
            let got = channel.to_node().read(&mut chunk).unwrap();
            buf.extend_from_slice(&chunk[..got]);
            while buf.len() >= 4 {
                let len = protocol::frame_len(buf[..4].try_into().unwrap()).unwrap();
                if buf.len() < 4 + len {
                    break;
                }
                // A cast travels as a hot frame.
                let Ok(crate::hot::FromAppHot::Frame(FromApp::Request {
                    id,
                    request: Request::Cast { .. },
                })) = crate::hot::parse_from_app(&buf[4..4 + len])
                else {
                    panic!("not a cast");
                };
                let answer = if answered == 0 {
                    Answer::Unit
                } else {
                    Answer::CastFailed(CastError::Overloaded)
                };
                let frame = protocol::encode(&ToApp::Answer { id, answer });
                channel.to_app().write(&frame).unwrap();
                wake_reader(channel.to_app().reader_waiting(), door_write.as_fd());
                answered += 1;
                buf.drain(..4 + len);
            }
            if got == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        door_write
    });
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    let to = Pid {
        node: 1,
        id: 7,
        incarnation: 1,
    };

    let (first, second) = local.block_on(&rt, async {
        let sender = AsyncSender::new(Sender::for_tests(end)).unwrap();
        tokio::task::spawn_local(sender.clone().pump());
        let first = sender.cast(to, text("one")).await;
        let second = sender.cast(to, text("two")).await;
        (first, second)
    });

    let _door = node.join().unwrap();
    assert_eq!(first, Ok(()));
    assert_eq!(second, Err(CastError::Overloaded));
}

/// What the app sent, as the node read it after `hold` (the ring full
/// meanwhile): each frame's kind, in order, with a call's body length;
/// calls answered with their own message. The node wakes the app for room
/// through its door, as a node does for a writer waiting there.
fn reading_node(
    channel: Channel,
    door_write: std::os::fd::OwnedFd,
    n: usize,
    hold: Duration,
) -> (Vec<String>, std::os::fd::OwnedFd) {
    std::thread::sleep(hold);
    let mut buf = Vec::new();
    let mut seen = Vec::new();
    let mut chunk = vec![0u8; 65536];
    while seen.len() < n {
        let got = channel.to_node().read(&mut chunk).unwrap();
        buf.extend_from_slice(&chunk[..got]);
        wake_writer(channel.to_node().writer_waiting(), door_write.as_fd());
        while buf.len() >= 4 {
            let len = protocol::frame_len(buf[..4].try_into().unwrap()).unwrap();
            if buf.len() < 4 + len {
                break;
            }
            match protocol::decode::<FromApp>(&buf[4..4 + len]).unwrap() {
                FromApp::Request {
                    id,
                    request: Request::CallApp { msg, .. },
                } => {
                    seen.push(format!("call {}", msg.body.len()));
                    let frame = protocol::encode(&ToApp::Answer {
                        id,
                        answer: Answer::Called(Ok(text("done"))),
                    });
                    channel.to_app().write(&frame).unwrap();
                    wake_reader(channel.to_app().reader_waiting(), door_write.as_fd());
                }
                FromApp::Metrics(_) => seen.push("metrics".into()),
                other => panic!("unexpected: {other:?}"),
            }
            buf.drain(..4 + len);
        }
        if got == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    (seen, door_write)
}

#[test]
fn async_sender____a_call_larger_than_the_ring____the_thread_runs_on_while_it_drains() {
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
    let node = std::thread::spawn(move || {
        reading_node(channel, door_write, 1, Duration::from_millis(200))
    });
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();

    let (returned_in, ticks, answer) = local.block_on(&rt, async {
        let sender = AsyncSender::new(Sender::for_tests(end)).unwrap();
        tokio::task::spawn_local(sender.clone().pump());
        // Another task on the thread, ticking while the node reads nothing.
        let ticker = tokio::task::spawn_local(async {
            let mut ticks = 0;
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(5)).await;
                ticks += 1;
            }
            ticks
        });
        tokio::task::yield_now().await;
        let started = std::time::Instant::now();
        let big = Envelope {
            body: vec![7; 1 << 20],
            ..text("")
        };
        let call = sender.call_app("app", big, 5_000);
        let returned_in = started.elapsed();
        let ticks = tokio::time::timeout(Duration::from_millis(150), ticker)
            .await
            .ok()
            .map(Result::unwrap);
        (returned_in, ticks, call.await.unwrap().body)
    });

    let (seen, _door) = node.join().unwrap();
    assert!(returned_in < Duration::from_millis(50), "{returned_in:?}");
    assert_eq!(ticks, Some(10));
    assert_eq!(answer, b"done".to_vec());
    assert_eq!(seen, vec![format!("call {}", 1 << 20)]);
}

#[test]
fn async_sender____frames_after_a_large_one____sent_in_order() {
    let (channel, memfd) = Channel::create().unwrap();
    let (_bell_read, bell_write) = bell().unwrap();
    let (door_read, door_write) = door().unwrap();
    let end = AppEnd::sender(memfd, bell_write, door_read, Spin(Duration::ZERO)).unwrap();
    let node =
        std::thread::spawn(move || reading_node(channel, door_write, 3, Duration::from_millis(50)));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();

    let answers = local.block_on(&rt, async {
        let sender = AsyncSender::new(Sender::for_tests(end)).unwrap();
        tokio::task::spawn_local(sender.clone().pump());
        let big = Envelope {
            body: vec![7; 300_000],
            ..text("")
        };
        let first = sender.call_app("app", big, 5_000);
        sender.metric_add(0, 1.0);
        let second = sender.call_app("app", text("small"), 5_000);
        (first.await.unwrap().body, second.await.unwrap().body)
    });

    let (seen, _door) = node.join().unwrap();
    assert_eq!(answers, (b"done".to_vec(), b"done".to_vec()));
    assert_eq!(
        seen,
        vec!["call 300000".to_string(), "metrics".into(), "call 5".into()]
    );
}
