#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::net::TcpListener;

use super::*;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn connect_tcp____the_node_opens_one____stream_given_and_its_key_back_once_dropped() {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = server.local_addr().unwrap();
    let (wanted_tx, mut wanted_rx) = mpsc::unbounded_channel();
    let (closed_tx, mut closed_rx) = mpsc::unbounded_channel();
    let provider = NodeStreams::new(wanted_tx, closed_tx);

    let (asked, key, closed) = runtime().block_on(async move {
        // The callback thread's part, played here.
        let node = tokio::spawn(async move {
            let w: Wanted = wanted_rx.recv().await.unwrap();
            let stream = std::net::TcpStream::connect(w.addr).unwrap();
            w.reply.send(Ok(Opened { key: 7, stream })).unwrap();
            w.addr
        });
        let stream = provider.connect_tcp(addr, None, None).await.unwrap();
        let key = stream.key();
        drop(stream);
        let closed = closed_rx.recv().await;
        (node.await.unwrap(), key, closed)
    });

    assert_eq!((asked, key, closed), (addr, 7, Some(7)));
}

#[test]
fn connect_tcp____the_node_refuses____the_error_given() {
    let (wanted_tx, mut wanted_rx) = mpsc::unbounded_channel();
    let (closed_tx, _closed_rx) = mpsc::unbounded_channel();
    let provider = NodeStreams::new(wanted_tx, closed_tx);
    let addr: SocketAddr = "192.0.2.1:53".parse().unwrap();

    let got = runtime().block_on(async move {
        tokio::spawn(async move {
            let w: Wanted = wanted_rx.recv().await.unwrap();
            w.reply
                .send(Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "access-denied",
                )))
                .unwrap();
        });
        provider.connect_tcp(addr, None, None).await
    });

    assert_eq!(got.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
}

#[test]
fn connect_tcp____no_answer_in_time____timed_out() {
    let (wanted_tx, _wanted_rx) = mpsc::unbounded_channel();
    let (closed_tx, _closed_rx) = mpsc::unbounded_channel();
    let provider = NodeStreams::new(wanted_tx, closed_tx);
    let addr: SocketAddr = "192.0.2.1:53".parse().unwrap();

    let got = runtime().block_on(provider.connect_tcp(addr, None, Some(Duration::from_millis(20))));

    assert_eq!(got.unwrap_err().kind(), io::ErrorKind::TimedOut);
}

#[test]
fn bind_udp____any____refused() {
    let (wanted_tx, _wanted_rx) = mpsc::unbounded_channel();
    let (closed_tx, _closed_rx) = mpsc::unbounded_channel();
    let provider = NodeStreams::new(wanted_tx, closed_tx);
    let any: SocketAddr = "0.0.0.0:0".parse().unwrap();

    let got = runtime().block_on(provider.bind_udp(any, any));

    assert_eq!(got.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
}
