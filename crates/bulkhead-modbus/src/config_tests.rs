#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use serde_json::json;

use super::*;

#[test]
fn Config____null____empty() {
    let config = Config::from_json(&serde_json::Value::Null).unwrap();

    assert_eq!(config, Config::default());
}

#[test]
fn Config____device_and_server____defaults_filled_in() {
    let config = Config::from_json(&json!({
        "devices": { "pump-1": {
            "address": "10.0.0.5:502",
            "deliver_to": "scada",
            "holding": [{ "start": 0, "count": 10 }]
        } },
        "server": { "listen": "0.0.0.0:5020", "holding": 100 }
    }))
    .unwrap();

    let pump = &config.devices["pump-1"];
    assert_eq!(pump.unit, 1);
    assert_eq!(pump.poll(), Duration::from_secs(1));
    assert!(!pump.primary_only);
    assert_eq!(
        pump.holding,
        vec![Range {
            start: 0,
            count: 10
        }]
    );
    let server = config.server.unwrap();
    assert_eq!(server.name, "server");
    assert_eq!(server.holding, 100);
}

#[test]
fn Config____invalid_ranges_and_missing_target____refused() {
    let device = |extra: serde_json::Value| {
        let mut d = json!({ "address": "10.0.0.5:502", "deliver_to": "scada" });
        d.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        Config::from_json(&json!({ "devices": { "d": d } }))
    };

    let too_many = device(json!({ "holding": [{ "start": 0, "count": 126 }] }));
    let past_end = device(json!({ "coils": [{ "start": 65535, "count": 2 }] }));
    let zero = device(json!({ "input": [{ "start": 0, "count": 0 }] }));
    let unknown = device(json!({ "colis": [] }));
    let no_target = Config::from_json(&json!({ "devices": { "d": {
        "address": "10.0.0.5:502", "holding": [{ "start": 0, "count": 1 }]
    } } }));

    assert!(too_many.unwrap_err().contains("devices.d.holding"));
    assert!(past_end.unwrap_err().contains("65535"));
    assert!(zero.unwrap_err().contains("count 0"));
    assert!(unknown.unwrap_err().contains("colis"));
    assert!(no_target.unwrap_err().contains("deliver_to"));
}

/// A device polling holding 0 to 9 and coils 0 to 3, with `tags`.
fn with_tags(tags: serde_json::Value) -> Result<Config, String> {
    Config::from_json(&json!({
        "devices": { "pump-1": {
            "address": "10.0.0.5:502",
            "deliver_to": "scada",
            "holding": [{ "start": 0, "count": 10 }],
            "coils": [{ "start": 0, "count": 4 }],
            "tags": tags
        } }
    }))
}

#[test]
fn Config____tags____types_and_defaults_read() {
    let config = with_tags(json!({
        "flow": { "table": "holding", "address": 0, "type": "f32", "order": "cdab",
                  "scale": 2.0, "deadband": 0.5 },
        "level": { "table": "holding", "address": 2 },
        "pump-on": { "table": "coil", "address": 1 },
        "alarm": { "table": "holding", "address": 3, "type": "bool", "bit": 4 },
        "fault": { "table": "holding", "address": 3, "type": "bool", "bit": 5 }
    }))
    .unwrap();

    let tags = &config.devices["pump-1"].tags;
    assert_eq!(tags["flow"].kind(), TagType::F32);
    assert_eq!(tags["flow"].order, Some(WordOrder::Cdab));
    assert_eq!(tags["level"].kind(), TagType::U16);
    assert_eq!(tags["level"].scale, 1.0);
    assert_eq!(tags["pump-on"].kind(), TagType::Bool);
    assert_eq!(tags["flow"].span(), 0..2);
}

#[rstest::rstest]
#[case(json!({ "x": { "table": "holding", "address": 9, "type": "f32" } }), "not inside one of the device's polled ranges")]
#[case(json!({ "x": { "table": "input", "address": 0 } }), "not inside one of the device's polled ranges")]
#[case(json!({ "a": { "table": "holding", "address": 0, "type": "u32" }, "b": { "table": "holding", "address": 1 } }), "shares")]
#[case(json!({ "x": { "table": "holding", "address": 0, "type": "bool" } }), "names its bit")]
#[case(json!({ "x": { "table": "holding", "address": 0, "type": "bool", "bit": 16 } }), "0 to 15")]
#[case(json!({ "x": { "table": "holding", "address": 0, "bit": 1 } }), "bit goes with")]
#[case(json!({ "x": { "table": "coil", "address": 0, "type": "u16" } }), "a coil or discrete input is a bool")]
#[case(json!({ "x": { "table": "holding", "address": 0, "order": "cdab" } }), "order is for 32- and 64-bit")]
#[case(json!({ "x": { "table": "holding", "address": 0, "scale": 0.0 } }), "scale")]
#[case(json!({ "x": { "table": "holding", "address": 0, "deadband": -1.0 } }), "deadband")]
#[case(json!({ "Flow": { "table": "holding", "address": 0 } }), "lowercase")]
fn Config____tag_breaks_a_rule____refused_naming_it(
    #[case] tags: serde_json::Value,
    #[case] says: &str,
) {
    let err = with_tags(tags).unwrap_err();

    assert!(
        err.contains("devices.pump-1.tags.") && err.contains(says),
        "{err}"
    );
}

#[test]
fn Config____unknown_tag_field_or_type____refused() {
    let field = with_tags(json!({ "x": { "table": "holding", "address": 0, "units": "m" } }));
    let kind = with_tags(json!({ "x": { "table": "holding", "address": 0, "type": "f16" } }));

    assert!(field.is_err());
    assert!(kind.is_err());
}
