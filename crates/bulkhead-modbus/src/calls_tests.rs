#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;

use super::{ClientError, ServedError, client, server};
use crate::config::{
    MAX_READ_BITS, MAX_READ_REGISTERS, MAX_WRITE_BITS, MAX_WRITE_REGISTERS, ServerConfig,
};
use crate::device::{self, DeviceError, DeviceHandle, Reply, Request};
use crate::points::Table;
use crate::server::Served;

type Seen = Arc<Mutex<Vec<Request>>>;

/// Device `pump`, answering with `answer`, recording what it was asked.
fn devices(
    answer: impl Fn(&Request) -> Result<Reply, DeviceError> + Send + 'static,
) -> (BTreeMap<String, DeviceHandle>, Seen) {
    let seen = Seen::default();
    let handle = device::answering(answer, seen.clone());
    (BTreeMap::from([("pump".to_string(), handle)]), seen)
}

fn words(request: &Request) -> Result<Reply, DeviceError> {
    match request {
        Request::ReadWords(_, _, count) => Ok(Reply::Words((1..=*count).collect())),
        // A device answers coils in whole bytes.
        Request::ReadBits(..) => Ok(Reply::Bits(vec![true; 8])),
        _ => Ok(Reply::Done),
    }
}

fn args<T: serde::Serialize>(value: &T) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::into_writer(value, &mut out).unwrap();
    out
}

fn result<T: DeserializeOwned, E: DeserializeOwned>(bytes: &[u8]) -> Result<T, E> {
    ciborium::from_reader(bytes).unwrap()
}

#[tokio::test]
async fn client____read_holding____device_asked_and_answer_returned() {
    let (devices, seen) = devices(words);

    let read = client(
        &devices,
        "client#read-holding",
        &args(&("pump", 10u16, 3u16)),
    )
    .await;

    assert_eq!(result::<Vec<u16>, ClientError>(&read), Ok(vec![1, 2, 3]));
    assert_eq!(
        *seen.lock().unwrap(),
        vec![Request::ReadWords(Table::Holding, 10, 3)]
    );
}

#[tokio::test]
async fn client____device_the_driver_does_not_poll____no_such_device() {
    let (devices, _) = devices(words);

    let read = client(&devices, "client#read-input", &args(&("gone", 0u16, 1u16))).await;

    assert_eq!(
        result::<Vec<u16>, ClientError>(&read),
        Err(ClientError::NoSuchDevice)
    );
}

#[tokio::test]
async fn client____counts_and_lengths_outside_one_to_the_limit____invalid_and_device_not_asked() {
    let (devices, seen) = devices(words);
    let calls = [
        ("client#read-holding", args(&("pump", 0u16, 0u16))),
        (
            "client#read-holding",
            args(&("pump", 0u16, MAX_READ_REGISTERS + 1)),
        ),
        ("client#read-coils", args(&("pump", 0u16, 0u16))),
        (
            "client#read-coils",
            args(&("pump", 0u16, MAX_READ_BITS + 1)),
        ),
        (
            "client#write-holding",
            args(&("pump", 0u16, Vec::<u16>::new())),
        ),
        (
            "client#write-holding",
            args(&("pump", 0u16, vec![0u16; MAX_WRITE_REGISTERS + 1])),
        ),
        (
            "client#write-coils",
            args(&("pump", 0u16, Vec::<bool>::new())),
        ),
        (
            "client#write-coils",
            args(&("pump", 0u16, vec![false; MAX_WRITE_BITS + 1])),
        ),
        ("client#read-holding", args(&("pump", "not a number"))),
    ];

    for (function, a) in calls {
        let got = client(&devices, function, &a).await;
        let decoded: Result<ciborium::Value, ClientError> = result(&got);
        assert!(
            matches!(decoded, Err(ClientError::Invalid(_))),
            "{function}: {decoded:?}"
        );
    }
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn client____exactly_the_maximum____sent_to_the_device() {
    let (devices, seen) = devices(words);

    let read = client(
        &devices,
        "client#read-input",
        &args(&("pump", 0u16, MAX_READ_REGISTERS)),
    )
    .await;
    let wrote = client(
        &devices,
        "client#write-coils",
        &args(&("pump", 5u16, vec![true; MAX_WRITE_BITS])),
    )
    .await;

    assert_eq!(
        result::<Vec<u16>, ClientError>(&read).unwrap().len(),
        usize::from(MAX_READ_REGISTERS)
    );
    assert_eq!(result::<(), ClientError>(&wrote), Ok(()));
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn client____device_answers_whole_bytes____cut_to_the_count() {
    let (devices, _) = devices(words);

    let bits = client(
        &devices,
        "client#read-discrete",
        &args(&("pump", 0u16, 3u16)),
    )
    .await;

    assert_eq!(
        result::<Vec<bool>, ClientError>(&bits),
        Ok(vec![true, true, true])
    );
}

#[tokio::test]
async fn client____exception_and_offline____passed_to_the_app() {
    let (exception, _) = devices(|_| Err(DeviceError::Exception(2)));
    let (offline, _) = devices(|_| Err(DeviceError::Offline("timed out".into())));

    let a = client(
        &exception,
        "client#write-holding",
        &args(&("pump", 0u16, vec![1u16])),
    )
    .await;
    let b = client(&offline, "client#read-coils", &args(&("pump", 0u16, 1u16))).await;

    assert_eq!(
        result::<(), ClientError>(&a),
        Err(ClientError::Exception(2))
    );
    assert_eq!(
        result::<Vec<bool>, ClientError>(&b),
        Err(ClientError::Offline("timed out".into()))
    );
}

#[test]
fn server____set_then_get____values_back_and_ranges_checked() {
    let config: ServerConfig = serde_json::from_value(serde_json::json!({
        "listen": "127.0.0.1:0", "holding": 4, "input": 4, "coils": 4, "discrete": 4
    }))
    .unwrap();
    let served = Served::new(&config);

    let set = server(&served, "server#set-holding", &args(&(1u16, vec![7u16, 8])));
    let got = server(&served, "server#get-holding", &args(&(0u16, 4u16)));
    let past = server(
        &served,
        "server#set-coils",
        &args(&(3u16, vec![true, true])),
    );
    let past_get = server(&served, "server#get-coils", &args(&(4u16, 1u16)));
    let input = server(
        &served,
        "server#set-input",
        &args(&(0u16, vec![1u16, 2, 3, 4])),
    );

    assert_eq!(result::<(), ServedError>(&set), Ok(()));
    assert_eq!(result::<Vec<u16>, ServedError>(&got), Ok(vec![0, 7, 8, 0]));
    assert_eq!(
        result::<(), ServedError>(&past),
        Err(ServedError::OutOfRange)
    );
    assert_eq!(
        result::<Vec<bool>, ServedError>(&past_get),
        Err(ServedError::OutOfRange)
    );
    assert_eq!(result::<(), ServedError>(&input), Ok(()));
}
