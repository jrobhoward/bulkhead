#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::collections::VecDeque;
use std::time::Duration;

use super::*;

#[test]
fn ingress_manifest____driver_crashing_at_once_for_ten_minutes____supervisor_never_gives_up() {
    let supervision = ingress_manifest(serde_json::json!({}))
        .unwrap()
        .spec
        .supervision;
    let backoff = bulkhead_host::Backoff::from(&supervision.backoff);
    let period = Duration::from_millis(supervision.period_ms);
    // The supervisor's rule: each exit is a restart in the window, the
    // window holding at most `intensity`, then a delay by the count in it;
    // the driver exits again the moment it starts.
    let mut now = Duration::ZERO;
    let mut restarts = VecDeque::new();
    let mut most = 0;
    while now < Duration::from_secs(600) {
        restarts.push_back(now);
        while restarts.front().is_some_and(|t| now - *t > period) {
            restarts.pop_front();
        }
        most = most.max(restarts.len());
        now += backoff.delay(restarts.len(), 0.0);
    }

    assert!(!backoff.jitter);
    assert!(
        most <= supervision.intensity as usize,
        "{most} restarts in {period:?}, intensity {}",
        supervision.intensity
    );
}
