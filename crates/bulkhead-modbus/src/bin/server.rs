//! `bulkhead-modbus-server`: the driver serving Modbus masters (ROADMAP
//! DA5), on the listening socket the node binds; it is not started by
//! hand.

bulkhead_sdk::app!(bulkhead_modbus::Server);

fn main() -> std::process::ExitCode {
    bulkhead_main()
}
