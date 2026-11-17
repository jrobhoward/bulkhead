//! The SDK fixture: the program the node runs.

#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    bulkhead_fixture_sdk::bulkhead_main()
}
