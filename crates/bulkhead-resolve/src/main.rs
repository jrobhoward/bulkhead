//! `bulkhead-resolve`: the node's resolver of host names as a driver app.
//! The node runs it; it is not started by hand.

#![forbid(unsafe_code)]

bulkhead_sdk::app!(bulkhead_resolve::Resolve);

fn main() -> std::process::ExitCode {
    bulkhead_main()
}
