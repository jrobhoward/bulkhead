#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::config::{Tag, WordOrder};

fn scan(points: &[(Table, u16, u16)]) -> Scan {
    points.iter().map(|&(t, a, v)| ((t, a), v)).collect()
}

#[test]
fn changes____one_value_differs____only_that_point() {
    let before = scan(&[(Table::Holding, 0, 1), (Table::Holding, 1, 2)]);
    let now = scan(&[(Table::Holding, 0, 1), (Table::Holding, 1, 3)]);

    let changed = changes(&before, &now, false);

    assert_eq!(
        changed,
        vec![Point {
            table: Table::Holding,
            address: 1,
            value: 3
        }]
    );
}

#[test]
fn changes____full____every_point() {
    let now = scan(&[(Table::Coil, 0, 1), (Table::Input, 5, 9)]);

    let changed = changes(&now.clone(), &now, true);

    assert_eq!(changed.len(), 2);
}

#[test]
fn changes____first_scan____every_point_is_new() {
    let now = scan(&[(Table::Discrete, 7, 0)]);

    let changed = changes(&Scan::new(), &now, false);

    assert_eq!(changed.len(), 1);
}

#[test]
fn record____words_and_bits____addressed_from_start() {
    let mut s = Scan::new();

    record_words(&mut s, Table::Holding, 10, &[100, 200]);
    record_bits(&mut s, Table::Coil, 0, &[true, false]);

    assert_eq!(
        s,
        scan(&[
            (Table::Holding, 10, 100),
            (Table::Holding, 11, 200),
            (Table::Coil, 0, 1),
            (Table::Coil, 1, 0),
        ])
    );
}

#[test]
fn Update____cbor____round_trips_with_device_at_top_level() {
    let update = Update {
        device: "pump-1".into(),
        kind: Kind::Points,
        full: true,
        points: vec![Point {
            table: Table::Holding,
            address: 3,
            value: 17,
        }],
        error: None,
        tags: Vec::new(),
    };

    let bytes = update.to_cbor();
    let back: Update = ciborium::from_reader(bytes.as_slice()).unwrap();
    let map: ciborium::Value = ciborium::from_reader(bytes.as_slice()).unwrap();

    assert_eq!(back, update);
    let device = map
        .as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_text() == Some("device"))
        .map(|(_, v)| v.as_text().unwrap().to_string());
    assert_eq!(device.as_deref(), Some("pump-1"));
}

const FORMATS: &str = include_str!("../../../docs/GUEST_FORMATS.md");

/// The bytes of the ` ```cbor-hex <name> ` block in GUEST_FORMATS.md.
fn documented(name: &str) -> Vec<u8> {
    let fence = format!("```cbor-hex {name}\n");
    let start = FORMATS
        .find(&fence)
        .unwrap_or_else(|| panic!("no example {name}"))
        + fence.len();
    let hex: String = FORMATS[start..start + FORMATS[start..].find("```").unwrap()]
        .split_whitespace()
        .collect();
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn Update____documented_examples____same_bytes() {
    let points = Update {
        device: "pump-1".into(),
        kind: Kind::Points,
        full: false,
        points: vec![Point {
            table: Table::Holding,
            address: 3,
            value: 17,
        }],
        error: None,
        tags: Vec::new(),
    };
    let offline = Update {
        device: "pump-1".into(),
        kind: Kind::Offline,
        full: false,
        points: Vec::new(),
        error: Some("connection refused".into()),
        tags: Vec::new(),
    };

    let (a, b) = (points.to_cbor(), offline.to_cbor());

    assert_eq!(a, documented("modbus-points"));
    assert_eq!(b, documented("modbus-offline"));
}

/// A tag from its TOML fields, as JSON.
fn tag(fields: serde_json::Value) -> Tag {
    serde_json::from_value(fields).unwrap()
}

/// Holding registers from address 0.
fn holding(words: &[u16]) -> Scan {
    (0u16..)
        .zip(words)
        .map(|(a, &w)| ((Table::Holding, a), w))
        .collect()
}

/// `be` (a value's big-endian bytes) as registers laid out in `order`.
fn registers(be: &[u8], order: WordOrder) -> Vec<u16> {
    let mut words: Vec<u16> = be
        .chunks(2)
        .map(|c| match order {
            WordOrder::Badc | WordOrder::Dcba => u16::from_be_bytes([c[1], c[0]]),
            WordOrder::Abcd | WordOrder::Cdab => u16::from_be_bytes([c[0], c[1]]),
        })
        .collect();
    if matches!(order, WordOrder::Cdab | WordOrder::Dcba) {
        words.reverse();
    }
    words
}

#[rstest::rstest]
#[case("abcd", &[0x4148, 0x0000])]
#[case("cdab", &[0x0000, 0x4148])]
#[case("badc", &[0x4841, 0x0000])]
#[case("dcba", &[0x0000, 0x4841])]
fn decode____f32_12_5_in_each_order____12_5(#[case] order: &str, #[case] words: &[u16]) {
    let t =
        tag(serde_json::json!({ "table": "holding", "address": 0, "type": "f32", "order": order }));

    let found = decode(&t, &holding(words));

    assert_eq!(found, Some(TagValue::Float(12.5)));
}

#[rstest::rstest]
#[case("i16", &[0xFFFF], TagValue::Signed(-1))]
#[case("u16", &[0xFFFF], TagValue::Unsigned(65_535))]
#[case("i32", &[0xFFFF, 0xFFFE], TagValue::Signed(-2))]
#[case("u32", &[0x0001, 0x0002], TagValue::Unsigned(65_538))]
#[case("f64", &[0x3FF8, 0, 0, 0], TagValue::Float(1.5))]
#[case("i64", &[0xFFFF, 0xFFFF, 0xFFFF, 0xFFFD], TagValue::Signed(-3))]
fn decode____each_type____read_big_endian(
    #[case] kind: &str,
    #[case] words: &[u16],
    #[case] expected: TagValue,
) {
    let t = tag(serde_json::json!({ "table": "holding", "address": 0, "type": kind }));

    let found = decode(&t, &holding(words));

    assert_eq!(found, Some(expected));
}

#[test]
fn decode____bits____one_bit_of_a_register_and_a_coil() {
    let bit3 =
        tag(serde_json::json!({ "table": "holding", "address": 0, "type": "bool", "bit": 3 }));
    let bit2 =
        tag(serde_json::json!({ "table": "holding", "address": 0, "type": "bool", "bit": 2 }));
    let coil = tag(serde_json::json!({ "table": "coil", "address": 5 }));
    let mut now = holding(&[0b1000]);
    now.insert((Table::Coil, 5), 1);

    let found = (
        decode(&bit3, &now),
        decode(&bit2, &now),
        decode(&coil, &now),
    );

    assert_eq!(
        found,
        (
            Some(TagValue::Bool(true)),
            Some(TagValue::Bool(false)),
            Some(TagValue::Bool(true))
        )
    );
}

#[test]
fn decode____scale_and_offset____a_float() {
    let t = tag(
        serde_json::json!({ "table": "holding", "address": 0, "type": "i16",
                                     "scale": 0.5, "offset": -10.0 }),
    );

    let found = decode(&t, &holding(&[0xFFFC]));

    assert_eq!(found, Some(TagValue::Float(-12.0)));
}

#[test]
fn decode____a_register_missing____none() {
    let t = tag(serde_json::json!({ "table": "holding", "address": 0, "type": "u32" }));

    let found = decode(&t, &holding(&[7]));

    assert_eq!(found, None);
}

proptest::proptest! {
    #[test]
    fn decode____any_f32_i32_u32_in_any_order____round_trips(
        x in proptest::num::f32::NORMAL, i in proptest::prelude::any::<i32>(),
        u in proptest::prelude::any::<u32>(), o in 0usize..4
    ) {
        let order = [WordOrder::Abcd, WordOrder::Cdab, WordOrder::Badc, WordOrder::Dcba][o];
        let name = ["abcd", "cdab", "badc", "dcba"][o];
        let of = |kind: &str| tag(serde_json::json!({ "table": "holding", "address": 0,
                                                        "type": kind, "order": name }));

        let float = decode(&of("f32"), &holding(&registers(&x.to_be_bytes(), order)));
        let signed = decode(&of("i32"), &holding(&registers(&i.to_be_bytes(), order)));
        let unsigned = decode(&of("u32"), &holding(&registers(&u.to_be_bytes(), order)));

        proptest::prop_assert_eq!(float, Some(TagValue::Float(f64::from(x))));
        proptest::prop_assert_eq!(signed, Some(TagValue::Signed(i64::from(i))));
        proptest::prop_assert_eq!(unsigned, Some(TagValue::Unsigned(u64::from(u))));
    }
}

/// One `level` tag, a u16 at holding 0 with `deadband`, and what
/// `tag_changes` sends for each scan of `values` in turn.
fn sent(deadband: f64, values: &[u16], full_at: Option<usize>) -> Vec<Option<TagValue>> {
    let tags = BTreeMap::from([(
        "level".to_string(),
        tag(serde_json::json!({ "table": "holding", "address": 0, "deadband": deadband })),
    )]);
    let mut last = BTreeMap::new();
    values
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            tag_changes(&tags, &mut last, &holding(&[v]), full_at == Some(i))
                .first()
                .map(|p| p.value)
        })
        .collect()
}

#[test]
fn tag_changes____deadband____measured_from_the_value_last_sent() {
    let found = sent(5.0, &[100, 103, 106, 104, 111], None);

    assert_eq!(
        found,
        [
            Some(TagValue::Unsigned(100)),
            None,
            Some(TagValue::Unsigned(106)),
            None,
            Some(TagValue::Unsigned(111))
        ]
    );
}

#[test]
fn tag_changes____no_deadband____every_change_and_nothing_else() {
    let found = sent(0.0, &[7, 7, 8], None);

    assert_eq!(
        found,
        [
            Some(TagValue::Unsigned(7)),
            None,
            Some(TagValue::Unsigned(8))
        ]
    );
}

#[test]
fn tag_changes____full____sent_inside_the_deadband() {
    let found = sent(5.0, &[100, 101], Some(1));

    assert_eq!(
        found,
        [Some(TagValue::Unsigned(100)), Some(TagValue::Unsigned(101))]
    );
}

#[test]
fn covered____tags____their_registers_only() {
    let tags = BTreeMap::from([
        (
            "flow".to_string(),
            tag(serde_json::json!({ "table": "holding", "address": 4, "type": "f32" })),
        ),
        (
            "on".to_string(),
            tag(serde_json::json!({ "table": "coil", "address": 1 })),
        ),
    ]);

    let found = covered(&tags);

    assert_eq!(
        found,
        BTreeSet::from([(Table::Holding, 4), (Table::Holding, 5), (Table::Coil, 1)])
    );
}

#[test]
fn Update____tags_example____documented_bytes() {
    let update = Update {
        device: "pump-1".into(),
        kind: Kind::Points,
        full: false,
        points: Vec::new(),
        error: None,
        tags: vec![
            TagPoint {
                name: "flow".into(),
                value: TagValue::Float(12.5),
            },
            TagPoint {
                name: "running".into(),
                value: TagValue::Bool(true),
            },
        ],
    };

    let bytes = update.to_cbor();
    let back: Update = ciborium::from_reader(bytes.as_slice()).unwrap();

    assert_eq!(back, update);
    assert_eq!(bytes, documented("modbus-tags"), "{}", hex(&bytes));
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
