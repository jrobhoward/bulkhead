//! S40: client libraries that open their own sockets, fenced per app.
//! `README.md` has the question, the runs and the findings.
//!
//! - `s40 launch --fence F --rules FILE [...] -- ROLE...` (root): the
//!   targets, the fence (`none`, `bpf`, `nft` on Linux; `none`, `pf`,
//!   `ipfw` on FreeBSD) and the fenced child.
//! - `s40 enter RULES on|off ROLE...`: the child's own fences, then ROLE.
//! - Roles: `probe`, `bench`, `passed`, `mqtt`.

// The interpreter and the reference rules serve the tests, and each
// system leaves the other's generators unused.
#![allow(dead_code)]

mod bench;
mod bpf;
mod enter;
mod firewall;
mod launch;
mod mqtt;
mod net;
mod os;
mod probe;
mod rules;

/// Run a role in this process.
pub fn role(args: &[String]) {
    match args.first().map(String::as_str) {
        Some("probe") => probe::main(&args[1..]),
        Some("bench") => bench::main(&args[1..]),
        Some("passed") => bench::passed(&args[1..]),
        Some("mqtt") => mqtt::main(&args[1..]),
        _ => {
            eprintln!("roles: probe, bench, passed, mqtt");
            std::process::exit(2);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("launch") => launch::main(&args[2..]),
        Some("enter") => enter::main(&args[2..]),
        _ => role(&args[1..]),
    }
}
