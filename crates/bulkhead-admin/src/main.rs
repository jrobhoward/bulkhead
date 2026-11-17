//! `bulkhead-admin`: the node's remote admin API as a driver app. The node
//! runs it; it is not started by hand.

#![forbid(unsafe_code)]

bulkhead_sdk::app!(bulkhead_admin::Admin);

fn main() -> std::process::ExitCode {
    bulkhead_main()
}
