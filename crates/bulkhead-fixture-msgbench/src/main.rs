//! The msgbench app: the program the node runs.

#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    bulkhead_fixture_msgbench::bulkhead_main()
}
