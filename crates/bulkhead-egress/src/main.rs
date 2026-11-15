//! `bulkhead-egress`: an app's outgoing HTTP as a driver app. The node runs
//! one per app granted `bulkhead:http-egress`; it is not started by hand.

bulkhead_sdk::app!(bulkhead_egress::Egress);

fn main() -> std::process::ExitCode {
    bulkhead_main()
}
