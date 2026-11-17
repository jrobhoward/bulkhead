//! `bulkhead-metrics`: the node's metrics endpoint as a driver app. The
//! node runs it; it is not started by hand.

#![forbid(unsafe_code)]

bulkhead_sdk::app!(bulkhead_metrics::Metrics);

fn main() -> std::process::ExitCode {
    bulkhead_main()
}
