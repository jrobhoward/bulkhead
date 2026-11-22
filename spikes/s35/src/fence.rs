//! Linux only: the fill under a Landlock rule, as the runner gives a
//! volume to an app; and a command run under the helper's own fence, as
//! a `zfs` child of the helper would be.

use std::os::unix::process::CommandExt;

use landlock::{ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, path_beneath_rules};
use seccompiler::SeccompAction;

/// Landlock with every access handled and `dir` alone allowed, then a
/// write outside it (refused, or the rule is not in force), then the fill.
pub fn landlock_fill(dir: &str, uid: Option<u32>) -> i32 {
    let abi = ABI::V3;
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .and_then(|r| r.create())
        .and_then(|r| r.add_rules(path_beneath_rules([dir], AccessFs::from_all(abi))))
        .and_then(|r| r.restrict_self());
    match status {
        Ok(s) => println!("landlock: {:?}", s.ruleset),
        Err(e) => {
            println!("landlock: {e}");
            return 1;
        }
    }
    let outside = std::fs::write("/tmp/s35-outside", b"x");
    println!("write outside the rule: {:?}", outside.map_err(|e| e.to_string()));
    crate::fill::fill(dir, uid)
}

/// The helper's `fence_self` (bulkhead-priv's `linux.rs`): no_new_privs,
/// the nine capabilities, and its seccomp allow-list with `mode` (`trap`
/// as in production, `log` to have the kernel name each call off the
/// list), then exec `cmd`. A root process keeps its bounding set across
/// exec, so `cmd` runs with the nine.
pub fn exec_fenced(mode: &str, cmd: &[&str]) -> i32 {
    // CHOWN, DAC_OVERRIDE, DAC_READ_SEARCH, FOWNER, KILL, SETGID, SETUID,
    // NET_BIND_SERVICE, SYS_ADMIN.
    let keep: u32 = [0, 1, 2, 3, 5, 6, 7, 10, 21].iter().map(|b| 1u32 << b).sum();
    // SAFETY: prctl and capset with valid arguments on this process.
    unsafe {
        assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
        for cap in 0..64 {
            if cap >= 32 || keep & (1 << cap) == 0 {
                let _ = libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0);
            }
        }
        #[repr(C)]
        struct Header {
            version: u32,
            pid: i32,
        }
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct Data {
            effective: u32,
            permitted: u32,
            inheritable: u32,
        }
        let h = Header { version: 0x2008_0522, pid: 0 };
        let d = [Data { effective: keep, permitted: keep, inheritable: 0 }, Data { effective: 0, permitted: 0, inheritable: 0 }];
        assert_eq!(libc::syscall(libc::SYS_capset, &h, d.as_ptr()), 0, "capset");
    }
    let action = if mode == "log" { SeccompAction::Log } else { SeccompAction::Trap };
    let program = bulkhead_priv::seccomp::program(action).expect("filter");
    seccompiler::apply_filter(&program).expect("apply");
    let err = std::process::Command::new(cmd[0]).args(&cmd[1..]).exec();
    eprintln!("exec {}: {err}", cmd[0]);
    1
}
