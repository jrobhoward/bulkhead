//! The greeter app: the program the node runs.

fn main() -> std::process::ExitCode {
    example_greeter_service::bulkhead_main()
}
