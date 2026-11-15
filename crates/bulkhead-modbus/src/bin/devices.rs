//! `bulkhead-modbus`: the driver polling Modbus devices (ROADMAP DA5). The
//! node runs it, and `bulkhead-modbus-primary` for `primary_only` devices;
//! it is not started by hand.

bulkhead_sdk::app!(bulkhead_modbus::Devices);

fn main() -> std::process::ExitCode {
    bulkhead_main()
}
