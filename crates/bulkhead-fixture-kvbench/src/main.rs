//! The kvbench loops: the program the node runs.

#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    bulkhead_fixture_kvbench::bulkhead_main()
}
