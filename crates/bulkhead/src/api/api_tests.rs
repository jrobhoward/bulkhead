#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

/// A log record from a node one release older has no `trace` (ROADMAP
/// O1): it reads as untraced, and an untraced one writes no `trace` key.
#[test]
fn LogLine____without_trace____decodes_untraced_and_encodes_without_it() {
    let older = r#"{"time_ms":1,"level":"WARN","pid":"1.2.3","source":"","message":"m"}"#;

    let line: LogLine = serde_json::from_str(older).unwrap();
    let back = serde_json::to_string(&line).unwrap();

    assert_eq!(line.trace, None);
    assert!(!back.contains("trace"), "{back}");
}
