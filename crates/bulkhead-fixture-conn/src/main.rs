//! The connection fixture: the program the node runs.

#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    bulkhead_fixture_conn::bulkhead_main()
}
