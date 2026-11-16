//! The helper's own seccomp filter (Linux, ROADMAP PS1b): an allow-list,
//! installed by the helper's fence once the node runs; any call off the
//! list raises `SIGSYS` (`SECCOMP_RET_TRAP`), which the helper's handler
//! turns into a line naming the call (`bulkhead-priv: system call N
//! refused by the helper's seccomp filter`) and exit status 159.
//!
//! A filter stays in force across `clone3` and `execve`, so every
//! instance the helper starts afterwards runs under this one as well as
//! the runner's own (`bulkhead_native::runner::seccomp`). The list is
//! therefore the union of three:
//!
//! - `HELPER`: what the helper itself calls, and what a child does
//!   between `clone3` and `execve` (ids, mounts, `pivot_root`);
//! - `PRELUDE`: what the runner calls before its own filters are in
//!   force (Landlock, `seccomp`, `prctl`, the dynamic linker's);
//! - `INSTANCE`: every call the runner's filters allow or answer, so an
//!   app meets the runner's answers and no others. The kernel takes the
//!   strictest answer among a process's filters; both raise `SIGSYS` for
//!   a call off their lists, so an app's `crashed: syscall <name>` is
//!   unchanged. `seccomp_tests` checks that the runner's list stays
//!   inside this one.
//!
//! The list is broad, since a runner and its app need most of the
//! ordinary interface. What it refuses is the rest, which neither uses:
//! `open_by_handle_at` (a way past mount namespaces for a process with
//! `CAP_DAC_READ_SEARCH`, which the helper keeps), `bpf`, kernel modules,
//! `kexec`, the key calls, `perf_event_open`, `userfaultfd`, the new
//! mount API, `setns` and `unshare`, swap, reboot, the clock setters, and
//! any call a later kernel adds.

use std::collections::BTreeMap;

use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, TargetArch};

/// What the helper calls itself, and what its children call before they
/// execute the node or the runner.
const HELPER: &[i64] = &[
    // Starting processes: `clone3` into a cgroup and namespaces, then the
    // child's ids, mounts and root.
    libc::SYS_clone3,
    libc::SYS_setsid,
    libc::SYS_setresuid,
    libc::SYS_setresgid,
    libc::SYS_setgroups,
    libc::SYS_sethostname,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_capget,
    libc::SYS_capset,
    libc::SYS_setrlimit,
    // Instances' exits and signals.
    libc::SYS_pidfd_open,
    // The cgroup and share files.
    libc::SYS_fchown,
    libc::SYS_fchownat,
    // Ports bound for the node (`Bind`).
    libc::SYS_bind,
    libc::SYS_listen,
];

/// What the runner calls before its own filters are installed.
const PRELUDE: &[i64] = &[
    libc::SYS_landlock_create_ruleset,
    libc::SYS_landlock_add_rule,
    libc::SYS_landlock_restrict_self,
    libc::SYS_seccomp,
];

/// `bulkhead_native::runner::seccomp`'s list: every call its filters
/// allow, or answer with `EPERM` or `ENOSYS`.
const INSTANCE: &[i64] = &[
    // Memory.
    libc::SYS_brk,
    libc::SYS_mmap,
    libc::SYS_munmap,
    libc::SYS_mprotect,
    libc::SYS_mremap,
    libc::SYS_madvise,
    libc::SYS_membarrier,
    // Processes and threads.
    libc::SYS_clone,
    libc::SYS_clone3,
    libc::SYS_execve,
    libc::SYS_exit,
    libc::SYS_exit_group,
    libc::SYS_wait4,
    libc::SYS_waitid,
    libc::SYS_set_tid_address,
    libc::SYS_set_robust_list,
    libc::SYS_rseq,
    libc::SYS_futex,
    libc::SYS_sched_yield,
    libc::SYS_sched_getaffinity,
    libc::SYS_sched_setaffinity,
    libc::SYS_getcpu,
    libc::SYS_getpid,
    libc::SYS_getppid,
    libc::SYS_gettid,
    libc::SYS_getuid,
    libc::SYS_geteuid,
    libc::SYS_getgid,
    libc::SYS_getegid,
    libc::SYS_prctl,
    libc::SYS_prlimit64,
    libc::SYS_getrusage,
    libc::SYS_uname,
    libc::SYS_sysinfo,
    libc::SYS_getrandom,
    libc::SYS_restart_syscall,
    // Signals.
    libc::SYS_rt_sigaction,
    libc::SYS_rt_sigprocmask,
    libc::SYS_rt_sigreturn,
    libc::SYS_sigaltstack,
    libc::SYS_kill,
    libc::SYS_tgkill,
    libc::SYS_tkill,
    libc::SYS_rt_sigqueueinfo,
    libc::SYS_rt_tgsigqueueinfo,
    libc::SYS_pidfd_send_signal,
    libc::SYS_pidfd_getfd,
    // Time.
    libc::SYS_clock_gettime,
    libc::SYS_clock_getres,
    libc::SYS_clock_nanosleep,
    libc::SYS_nanosleep,
    libc::SYS_gettimeofday,
    libc::SYS_timerfd_create,
    libc::SYS_timerfd_settime,
    libc::SYS_timerfd_gettime,
    // Descriptors.
    libc::SYS_read,
    libc::SYS_write,
    libc::SYS_readv,
    libc::SYS_writev,
    libc::SYS_pread64,
    libc::SYS_pwrite64,
    libc::SYS_preadv,
    libc::SYS_pwritev,
    libc::SYS_lseek,
    libc::SYS_close,
    libc::SYS_close_range,
    libc::SYS_dup,
    libc::SYS_dup3,
    libc::SYS_fcntl,
    libc::SYS_ioctl,
    libc::SYS_pipe2,
    libc::SYS_eventfd2,
    libc::SYS_epoll_create1,
    libc::SYS_epoll_ctl,
    libc::SYS_epoll_pwait,
    libc::SYS_epoll_pwait2,
    libc::SYS_ppoll,
    libc::SYS_pselect6,
    // Files.
    libc::SYS_openat,
    libc::SYS_openat2,
    libc::SYS_fstat,
    libc::SYS_newfstatat,
    libc::SYS_statx,
    libc::SYS_statfs,
    libc::SYS_fstatfs,
    libc::SYS_faccessat,
    libc::SYS_faccessat2,
    libc::SYS_readlinkat,
    libc::SYS_getdents64,
    libc::SYS_getcwd,
    libc::SYS_chdir,
    libc::SYS_fchdir,
    libc::SYS_umask,
    libc::SYS_mkdirat,
    libc::SYS_unlinkat,
    libc::SYS_renameat,
    libc::SYS_renameat2,
    libc::SYS_symlinkat,
    libc::SYS_linkat,
    libc::SYS_truncate,
    libc::SYS_ftruncate,
    libc::SYS_fallocate,
    libc::SYS_fadvise64,
    libc::SYS_fsync,
    libc::SYS_fdatasync,
    libc::SYS_flock,
    libc::SYS_utimensat,
    libc::SYS_fchmod,
    libc::SYS_fchmodat,
    libc::SYS_copy_file_range,
    libc::SYS_sendfile,
    libc::SYS_splice,
    // Sockets.
    libc::SYS_recvfrom,
    libc::SYS_sendto,
    libc::SYS_recvmsg,
    libc::SYS_sendmsg,
    libc::SYS_recvmmsg,
    libc::SYS_sendmmsg,
    libc::SYS_shutdown,
    libc::SYS_getsockopt,
    libc::SYS_setsockopt,
    libc::SYS_getsockname,
    libc::SYS_getpeername,
    libc::SYS_socketpair,
    libc::SYS_socket,
    libc::SYS_connect,
    libc::SYS_accept,
    libc::SYS_accept4,
    // Answered `EPERM` by the runner's filters.
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
];

/// The calls only x86-64 has: the older forms its libc still makes, for
/// the runner (as its own list has them) and for the helper.
#[cfg(target_arch = "x86_64")]
const ARCH: &[i64] = &[
    libc::SYS_arch_prctl,
    libc::SYS_open,
    libc::SYS_stat,
    libc::SYS_lstat,
    libc::SYS_access,
    libc::SYS_readlink,
    libc::SYS_getdents,
    libc::SYS_mkdir,
    libc::SYS_rmdir,
    libc::SYS_unlink,
    libc::SYS_rename,
    libc::SYS_symlink,
    libc::SYS_link,
    libc::SYS_chmod,
    libc::SYS_chown,
    libc::SYS_pipe,
    libc::SYS_dup2,
    libc::SYS_poll,
    libc::SYS_select,
    libc::SYS_epoll_wait,
    libc::SYS_fork,
    libc::SYS_vfork,
    libc::SYS_time,
    libc::SYS_getrlimit,
];

#[cfg(not(target_arch = "x86_64"))]
const ARCH: &[i64] = &[];

/// Every call the filter allows.
#[must_use]
pub fn allowed() -> Vec<i64> {
    let mut calls: Vec<i64> = HELPER
        .iter()
        .chain(PRELUDE)
        .chain(INSTANCE)
        .chain(ARCH)
        .copied()
        .collect();
    calls.sort_unstable();
    calls.dedup();
    calls
}

/// The filter: the calls of [`allowed`] pass, any other gets `mismatch`
/// (`Trap` in use; `Log` to find a missing call without refusing it).
///
/// # Errors
///
/// A seccompiler error (none for valid input).
pub fn program(mismatch: SeccompAction) -> Result<BpfProgram, String> {
    let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(|e| e.to_string())?;
    let calls: BTreeMap<i64, Vec<seccompiler::SeccompRule>> =
        allowed().into_iter().map(|c| (c, Vec::new())).collect();
    SeccompFilter::new(calls, mismatch, SeccompAction::Allow, arch)
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|e: seccompiler::BackendError| e.to_string())
}

/// Install the filter on this thread, the helper's only one, after the
/// `SIGSYS` report is in place.
///
/// # Errors
///
/// The filter could not be built, or the kernel refused it.
pub fn install() -> Result<(), String> {
    let mismatch = if std::env::var_os(LOG_ENV).is_some() {
        SeccompAction::Log
    } else {
        SeccompAction::Trap
    };
    crate::sys::report_sigsys().map_err(|e| format!("SIGSYS handler: {e}"))?;
    seccompiler::apply_filter(&program(mismatch)?).map_err(|e| e.to_string())
}

/// Set in the helper's environment: calls off the list are logged by the
/// kernel (`type=1326`) and allowed, not refused. For finding a missing
/// call on a test machine; never in production.
pub const LOG_ENV: &str = "BULKHEAD_PRIV_SECCOMP_LOG";

#[cfg(test)]
#[path = "seccomp_tests.rs"]
mod tests;
