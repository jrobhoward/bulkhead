//! The net fixture: the program the node runs.

#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    bulkhead_fixture_net::bulkhead_main()
}
