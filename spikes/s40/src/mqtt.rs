//! `s40 mqtt --broker A:1883 --refused A:1884`: rumqttc, a library that
//! opens its own socket, inside a fence: the time to a CONNACK, QoS 0
//! messages through the broker to a subscriber, QoS 1 publishes
//! acknowledged, and a broker the rules do not allow.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS};

const QOS0: usize = 100_000;
const QOS1: usize = 20_000;

fn addr(args: &[String], name: &str, default: &str) -> SocketAddr {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map_or(default, String::as_str)
        .parse()
        .unwrap()
}

fn client(id: &str, at: SocketAddr) -> (AsyncClient, EventLoop) {
    let mut o = MqttOptions::new(id, at.ip().to_string(), at.port());
    o.set_keep_alive(Duration::from_secs(30));
    o.set_max_packet_size(1 << 16, 1 << 16);
    AsyncClient::new(o, 10_000)
}

/// Poll until CONNACK; the time it took.
async fn connected(ev: &mut EventLoop) -> Result<Duration, String> {
    let t = Instant::now();
    loop {
        match ev.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => return Ok(t.elapsed()),
            Ok(_) => {}
            Err(e) => return Err(format!("{e:?}")),
        }
    }
}

pub fn main(args: &[String]) {
    let broker = addr(args, "--broker", "127.0.0.1:1883");
    let refused = addr(args, "--refused", "127.0.0.1:1884");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let (_, mut ev) = client("s40-refused", refused);
        match connected(&mut ev).await {
            Ok(d) => println!("mqtt broker {refused} (not in rules): connected in {d:?}"),
            Err(e) => println!("mqtt broker {refused} (not in rules): refused {e}"),
        }

        let (sub, mut sub_ev) = client("s40-sub", broker);
        let (publ, mut pub_ev) = client("s40-pub", broker);
        let mut times = Vec::new();
        for ev in [&mut sub_ev, &mut pub_ev] {
            match connected(ev).await {
                Ok(d) => times.push(d),
                Err(e) => {
                    println!("mqtt broker {broker}: refused {e}");
                    return;
                }
            }
        }
        println!(
            "mqtt connect to {broker}: {:?} and {:?}",
            times[0], times[1]
        );
        sub.subscribe("s40/t", QoS::AtMostOnce).await.unwrap();
        loop {
            if let Ok(Event::Incoming(Packet::SubAck(_))) = sub_ev.poll().await {
                break;
            }
        }
        let receiver = tokio::spawn(async move {
            let mut got = 0usize;
            let t = Instant::now();
            while got < QOS0 {
                match tokio::time::timeout(Duration::from_secs(2), sub_ev.poll()).await {
                    Ok(Ok(Event::Incoming(Packet::Publish(_)))) => got += 1,
                    Ok(Ok(_)) => {}
                    Ok(Err(_)) | Err(_) => break,
                }
            }
            (got, t.elapsed())
        });
        let payload = vec![7u8; 64];
        let sender = tokio::spawn(async move {
            for _ in 0..QOS0 {
                publ.publish("s40/t", QoS::AtMostOnce, false, payload.clone())
                    .await
                    .unwrap();
            }
            for _ in 0..QOS1 {
                publ.publish("s40/q1", QoS::AtLeastOnce, false, payload.clone())
                    .await
                    .unwrap();
            }
            publ
        });
        // Drive the publisher's loop, counting QoS 1 acknowledgements.
        let t = Instant::now();
        let mut acks = 0usize;
        let mut t1: Option<Instant> = None;
        while acks < QOS1 {
            match tokio::time::timeout(Duration::from_secs(5), pub_ev.poll()).await {
                Ok(Ok(Event::Outgoing(rumqttc::Outgoing::Publish(_)))) if t1.is_none() => {}
                Ok(Ok(Event::Incoming(Packet::PubAck(_)))) => {
                    acks += 1;
                    t1.get_or_insert_with(Instant::now);
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    println!("mqtt publisher: {e:?}");
                    break;
                }
                Err(_) => break,
            }
        }
        let total = t.elapsed();
        let _publ = sender.await.unwrap();
        let (got, e0) = receiver.await.unwrap();
        println!(
            "mqtt QoS 0, 64 B: {got} of {QOS0} received, {:.0} per second",
            got as f64 / e0.as_secs_f64()
        );
        let e1 = t1.map_or(total, |s| s.elapsed());
        println!(
            "mqtt QoS 1, 64 B: {acks} of {QOS1} acknowledged, {:.0} per second",
            acks as f64 / e1.as_secs_f64()
        );
    });
}
