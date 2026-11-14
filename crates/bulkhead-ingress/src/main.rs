//! `bulkhead-ingress`: the node's HTTP ingress as a driver app. The node
//! runs it; it is not started by hand.

bulkhead_sdk::app!(bulkhead_ingress::Ingress);

fn main() -> std::process::ExitCode {
    bulkhead_main()
}
