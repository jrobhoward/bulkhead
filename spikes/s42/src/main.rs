//! S42: blobs, the claim check (ROADMAP S42; `spikes/s42/README.md`).
//!
//! ```text
//! s42 checks [--runner PATH] [--users A,B] [--cgroup DIR]
//! s42 bench  [--runner PATH] [--sizes N,...] [--count N]
//! ```
//!
//! With no arguments (as `bulkhead-runner --executable` starts it) or
//! `app`, the binary is an app on the channel it inherits.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    missing_docs
)]

mod app;
mod node;
mod runs;
mod wire;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        None | Some("app") => app::serve(),
        Some("checks") => runs::checks(&args[2..]),
        Some("bench") => runs::bench(&args[2..]),
        Some(other) => {
            eprintln!("s42: unknown command {other}");
            std::process::exit(2);
        }
    }
}
