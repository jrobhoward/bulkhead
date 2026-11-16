//! The greeted app: the program the node runs, calling the greeter app.

fn main() -> std::process::ExitCode {
    example_greeter_app::bulkhead_main()
}
