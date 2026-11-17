//! The counter: the program the node runs.

#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    bulkhead_fixture_counter::bulkhead_main()
}
