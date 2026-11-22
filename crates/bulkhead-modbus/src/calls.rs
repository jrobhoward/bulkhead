//! Apps' `client#…` and `server#…` calls, forwarded by the node after its
//! checks (the grant, the manifest's `read`, `write` and `serve`), each
//! answered with the function's result in CBOR, as
//! `bulkhead-modbus-client` reads it.

use std::collections::BTreeMap;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config::{MAX_READ_BITS, MAX_READ_REGISTERS, MAX_WRITE_BITS, MAX_WRITE_REGISTERS};
use crate::device::{DeviceError, DeviceHandle, Reply, Request};
use crate::points::{Table, TagValue, Write, encode};
use crate::server::{self, Served, Tables};

/// The `client` functions' errors the driver gives (the node gives
/// `AccessDenied`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum ClientError {
    NoSuchDevice,
    Offline(String),
    Exception(u8),
    Invalid(String),
}

/// The `server` functions' errors the driver gives (the node gives
/// `AccessDenied`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum ServedError {
    NotServing,
    OutOfRange,
}

/// `value` in CBOR.
fn cbor<T: Serialize>(value: &T) -> Vec<u8> {
    let mut out = Vec::new();
    // Plain data into a Vec always encodes.
    let _ = ciborium::into_writer(value, &mut out);
    out
}

fn args<T: DeserializeOwned>(function: &str, args: &[u8]) -> Result<T, ClientError> {
    ciborium::from_reader(args).map_err(|e| ClientError::Invalid(format!("{function}: {e}")))
}

/// Answer a `client#…` call against `devices`.
pub(crate) async fn client(
    devices: &BTreeMap<String, DeviceHandle>,
    function: &str,
    raw: &[u8],
) -> Vec<u8> {
    match function {
        "client#read-holding" | "client#read-input" => {
            let table = if function.ends_with("input") {
                Table::Input
            } else {
                Table::Holding
            };
            let result = match args::<(String, u16, u16)>(function, raw) {
                Ok((device, address, count)) => {
                    read_words(devices, &device, table, address, count).await
                }
                Err(e) => Err(e),
            };
            cbor(&result)
        }
        "client#read-coils" | "client#read-discrete" => {
            let table = if function.ends_with("discrete") {
                Table::Discrete
            } else {
                Table::Coil
            };
            let result = match args::<(String, u16, u16)>(function, raw) {
                Ok((device, address, count)) => {
                    read_bits(devices, &device, table, address, count).await
                }
                Err(e) => Err(e),
            };
            cbor(&result)
        }
        "client#write-holding" => {
            let result = match args::<(String, u16, Vec<u16>)>(function, raw) {
                Ok((device, address, values)) => {
                    match check_len(values.len(), MAX_WRITE_REGISTERS) {
                        Ok(()) => run(devices, &device, Request::WriteWords(address, values))
                            .await
                            .map(|_| ()),
                        Err(e) => Err(e),
                    }
                }
                Err(e) => Err(e),
            };
            cbor(&result)
        }
        "client#write-coils" => {
            let result = match args::<(String, u16, Vec<bool>)>(function, raw) {
                Ok((device, address, values)) => match check_len(values.len(), MAX_WRITE_BITS) {
                    Ok(()) => run(devices, &device, Request::WriteBits(address, values))
                        .await
                        .map(|_| ()),
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            };
            cbor(&result)
        }
        "client#write-tag" => {
            let result = match args::<(String, String, TagValue)>(function, raw) {
                Ok((device, tag, value)) => write_tag(devices, &device, &tag, value).await,
                Err(e) => Err(e),
            };
            cbor(&result)
        }
        other => cbor(&Err::<(), _>(ClientError::Invalid(format!(
            "no function {other}"
        )))),
    }
}

/// Answer a `server#…` call against the served tables.
pub(crate) fn server(served: &Served, function: &str, raw: &[u8]) -> Vec<u8> {
    fn decode<T: DeserializeOwned>(raw: &[u8]) -> Result<T, ServedError> {
        ciborium::from_reader(raw).map_err(|_| ServedError::OutOfRange)
    }
    fn set<T: Copy>(
        served: &Served,
        pick: impl FnOnce(&mut Tables) -> &mut Vec<T>,
        address: u16,
        values: &[T],
    ) -> Result<(), ServedError> {
        let mut tables = served.tables();
        server::set(pick(&mut tables), address, values).map_err(|_| ServedError::OutOfRange)
    }
    fn get<T: Copy>(
        served: &Served,
        pick: impl FnOnce(&Tables) -> &Vec<T>,
        address: u16,
        count: u16,
    ) -> Result<Vec<T>, ServedError> {
        let tables = served.tables();
        server::get(pick(&tables), address, count).map_err(|_| ServedError::OutOfRange)
    }
    match function {
        "server#set-holding" => cbor(
            &decode::<(u16, Vec<u16>)>(raw)
                .and_then(|(a, v)| set(served, |t| &mut t.holding, a, &v)),
        ),
        "server#set-input" => cbor(
            &decode::<(u16, Vec<u16>)>(raw).and_then(|(a, v)| set(served, |t| &mut t.input, a, &v)),
        ),
        "server#set-coils" => cbor(
            &decode::<(u16, Vec<bool>)>(raw)
                .and_then(|(a, v)| set(served, |t| &mut t.coils, a, &v)),
        ),
        "server#set-discrete" => cbor(
            &decode::<(u16, Vec<bool>)>(raw)
                .and_then(|(a, v)| set(served, |t| &mut t.discrete, a, &v)),
        ),
        "server#get-holding" => {
            cbor(&decode::<(u16, u16)>(raw).and_then(|(a, n)| get(served, |t| &t.holding, a, n)))
        }
        "server#get-coils" => {
            cbor(&decode::<(u16, u16)>(raw).and_then(|(a, n)| get(served, |t| &t.coils, a, n)))
        }
        _ => cbor(&Err::<(), _>(ServedError::NotServing)),
    }
}

/// Run `request` on `device`.
async fn run(
    devices: &BTreeMap<String, DeviceHandle>,
    device: &str,
    request: Request,
) -> Result<Reply, ClientError> {
    let handle = devices.get(device).ok_or(ClientError::NoSuchDevice)?;
    handle.request(request).await.map_err(|e| match e {
        DeviceError::Offline(why) => ClientError::Offline(why),
        DeviceError::Exception(code) => ClientError::Exception(code),
    })
}

/// Write `value` to `device`'s tag `tag` (ROADMAP DA5c).
async fn write_tag(
    devices: &BTreeMap<String, DeviceHandle>,
    device: &str,
    tag: &str,
    value: TagValue,
) -> Result<(), ClientError> {
    let handle = devices.get(device).ok_or(ClientError::NoSuchDevice)?;
    let config = handle
        .tags
        .get(tag)
        .ok_or_else(|| ClientError::Invalid(format!("no tag {tag} on {device}")))?;
    let request = match encode(config, value)
        .map_err(|e| ClientError::Invalid(format!("{tag}: {e}")))?
    {
        Write::Words(address, words) => Request::WriteWords(address, words),
        Write::Coil(address, on) => Request::WriteBits(address, vec![on]),
        Write::Mask(address, and_mask, or_mask) => Request::MaskWord(address, and_mask, or_mask),
    };
    run(devices, device, request).await.map(|_| ())
}

async fn read_words(
    devices: &BTreeMap<String, DeviceHandle>,
    device: &str,
    table: Table,
    address: u16,
    count: u16,
) -> Result<Vec<u16>, ClientError> {
    check_count(count, MAX_READ_REGISTERS)?;
    match run(devices, device, Request::ReadWords(table, address, count)).await? {
        Reply::Words(w) => Ok(w),
        other => Err(ClientError::Offline(format!("unexpected reply {other:?}"))),
    }
}

async fn read_bits(
    devices: &BTreeMap<String, DeviceHandle>,
    device: &str,
    table: Table,
    address: u16,
    count: u16,
) -> Result<Vec<bool>, ClientError> {
    check_count(count, MAX_READ_BITS)?;
    match run(devices, device, Request::ReadBits(table, address, count)).await? {
        // Coils come in whole bytes; drop the padding.
        Reply::Bits(mut b) => {
            b.truncate(usize::from(count));
            Ok(b)
        }
        other => Err(ClientError::Offline(format!("unexpected reply {other:?}"))),
    }
}

fn check_count(count: u16, max: u16) -> Result<(), ClientError> {
    if count == 0 || count > max {
        return Err(ClientError::Invalid(format!(
            "count {count} must be 1 to {max}"
        )));
    }
    Ok(())
}

fn check_len(len: usize, max: usize) -> Result<(), ClientError> {
    if len == 0 || len > max {
        return Err(ClientError::Invalid(format!(
            "{len} values; must be 1 to {max}"
        )));
    }
    Ok(())
}

#[cfg(test)]
#[path = "calls_tests.rs"]
mod calls_tests;
