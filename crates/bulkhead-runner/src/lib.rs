//! `bulkhead-runner`: the launcher a node runs every native app through.
//!
//! ```text
//! bulkhead-runner (--library PATH | --executable PATH) [--read DIR]... [--write DIR]... [--landlock required|best-effort] [--lease] [--accept-fd N]... [--profile app|ingress] [--unconfined | --own-sockets [--listen] [--connect-ports any|LIST] [--bind-ports any|LIST]]
//! ```
//!
//! `--own-sockets` (ROADMAP AP2) lets the app open its own TCP and UDP
//! sockets: steps 2 and 3 stay, with Landlock's TCP connect and bind
//! limited to the ports listed (`443,8000-8100`; `any` leaves that half to
//! the helper's fence alone), seccomp allowing `socket` for Unix, IPv4 and
//! IPv6 and refusing raw and packet types, `connect`, `accept`, `bind`,
//! and `listen` only with `--listen`; on FreeBSD capability mode stays
//! off, since it refuses `connect` and `bind`. Where the sockets may go is
//! the helper's fence (cgroup BPF programs, ipfw rules), from its own
//! configuration; the node passes the flags only with the operator's
//! consent on every side, and the helper refuses them otherwise.
//!
//! `--unconfined` (ROADMAP AP4) leaves out steps 2 and 3 and, on FreeBSD,
//! capability mode: the app runs as the helper started it, under its own
//! user, in its cgroup or jail, with the host's network. The node passes
//! it only with the operator's consent on every side, and the helper
//! refuses it for an app its own configuration does not list. Steps 0
//! and 1 stay, the watchdog fenced as ever.
//!
//! In order, before any code from the bundle runs:
//!
//! 0. With `--lease`, for an instance bound to a failover lease: fork a
//!    watchdog ([`watchdog`], [`bulkhead_native::lease`]) that kills the app once
//!    the lease's last deadline from the node has passed, whether or not
//!    the node runs. The steps below run in the child.
//! 1. Not dumpable: no core dump (a `core_pattern` that pipes to a helper
//!    ignores `RLIMIT_CORE` and holds the dying process for about a
//!    second, spike S19), and no ptrace from other processes of the
//!    node's user.
//! 2. Landlock ([`sandbox::restrict`]): read the system libraries, the
//!    program's directory and each `--read` directory; read and write
//!    each `--write` directory (no executing from it); execute only the
//!    program; nothing else on the filesystem; no TCP. Set while the
//!    runner has one thread, since before Linux 7.1 Landlock covers only
//!    the calling thread and the threads it starts later. With
//!    `--landlock required` (the default) a kernel without Landlock is an
//!    error.
//! 3. seccomp ([`seccomp::install`]): signals only to this process, no
//!    ptrace or access to other processes' memory, no new sockets.
//!
//! Then `--library` loads a rustbridge plugin and serves it
//! ([`library`]), and `--executable` replaces the runner with the
//! program, which serves itself through [`bulkhead_native::app`]. Landlock and
//! seccomp stay in force across the exec; not being dumpable does not
//! (`execve` resets it for a readable program), so [`bulkhead_native::app::serve`]
//! sets it again.
//!
//! On FreeBSD (ROADMAP FB2) step 1 is
//! `procctl(PROC_TRACE_CTL_DISABLE_EXEC)`, which lasts across the exec,
//! and steps 2 and 3 are Capsicum (`capsicum::exec`, FreeBSD only): the runner limits
//! each inherited descriptor's rights, opens the preopens as descriptors,
//! enters capability mode and executes the program through the run-time
//! linker. `--landlock best-effort` changes nothing there, and a
//! rustbridge plugin is refused (its loading would need paths). The jail
//! per instance comes with the helper (FB2's second half).

#![cfg(any(target_os = "linux", target_os = "freebsd"))]

#[cfg(target_os = "freebsd")]
pub mod capsicum;
pub mod library;
#[cfg(target_os = "linux")]
pub mod sandbox;
#[cfg(target_os = "linux")]
pub mod seccomp;
pub mod watchdog;

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::ExitCode;

/// What to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Program {
    /// A rustbridge plugin, loaded into the runner.
    Library(PathBuf),
    /// An executable linking [`bulkhead_native::app`], run in the runner's place.
    Executable(PathBuf),
}

impl Program {
    /// The program's file.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        match self {
            Self::Library(p) | Self::Executable(p) => p,
        }
    }
}

/// The runner's arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// What to run.
    pub program: Program,
    /// Directories the program may read, beyond the system libraries and
    /// its own directory.
    pub read: Vec<PathBuf>,
    /// Directories the program may read and write (a read-write volume).
    pub write: Vec<PathBuf>,
    /// Whether to refuse to run without Landlock.
    pub landlock_required: bool,
    /// Whether a lease bounds the app ([`bulkhead_native::lease`]): fork a watchdog.
    pub lease: bool,
    /// Listening sockets a driver app was passed (ROADMAP DA1), by
    /// descriptor number: `accept4` is allowed on these alone.
    pub accept_fds: Vec<i32>,
    /// `--profile`: which system calls the app may make (Linux; on
    /// FreeBSD Capsicum fences every app alike).
    pub profile: Profile,
    /// `--unconfined`: no Landlock, seccomp filter or capability mode for
    /// the app (ROADMAP AP4).
    pub unconfined: bool,
    /// `--own-sockets`: the app opens its own sockets (ROADMAP AP2).
    pub own_sockets: Option<OwnSockets>,
}

/// What an own-sockets app may do with its sockets, beside the helper's
/// fence (the runner's `--own-sockets`, ROADMAP AP2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OwnSockets {
    /// `--listen`: whether `listen` is allowed. It binds an unbound socket
    /// to an ephemeral port without the bind hooks, so seccomp is its
    /// fence.
    pub listen: bool,
    /// `--connect-ports`: the TCP ports Landlock lets it connect to;
    /// `None` for any.
    pub connect: Option<Vec<(u16, u16)>>,
    /// `--bind-ports`: the TCP ports Landlock lets it bind; `None` for any.
    pub bind: Option<Vec<(u16, u16)>>,
}

/// The most ports a list may name before Landlock leaves that half to
/// the helper's fence: Landlock takes one rule per port.
pub const MAX_LANDLOCK_PORTS: usize = 1024;

/// Parse `any`, `none`, or a list of ports and ranges (`443,8000-8100`).
///
/// # Errors
///
/// A part that is not a port or an ordered range.
pub fn parse_ports(v: &str) -> Result<Option<Vec<(u16, u16)>>, String> {
    match v {
        "any" => return Ok(None),
        "none" => return Ok(Some(Vec::new())),
        _ => {}
    }
    v.split(',')
        .map(|part| {
            let bad = || format!("{part}: not a port or a range like 8000-8100");
            let (lo, hi) = part.split_once('-').unwrap_or((part, part));
            let lo: u16 = lo.trim().parse().map_err(|_| bad())?;
            let hi: u16 = hi.trim().parse().map_err(|_| bad())?;
            if lo > hi { Err(bad()) } else { Ok((lo, hi)) }
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// The ports Landlock is given for `ports`, or `None` (leave that half
/// unhandled) for any, or for more than [`MAX_LANDLOCK_PORTS`].
#[must_use]
pub fn landlock_ports(ports: Option<&[(u16, u16)]>) -> Option<Vec<u16>> {
    let ports = ports?;
    let count: usize = ports.iter().map(|(lo, hi)| usize::from(hi - lo) + 1).sum();
    if count > MAX_LANDLOCK_PORTS {
        return None;
    }
    let mut out: Vec<u16> = ports.iter().flat_map(|(lo, hi)| *lo..=*hi).collect();
    out.sort_unstable();
    out.dedup();
    Some(out)
}

/// Which system calls a process gets (the runner's `--profile`, ROADMAP
/// DA2b): on Linux, `seccomp::allowed_for`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Profile {
    /// Any native app's.
    #[default]
    App,
    /// The HTTP ingress driver's: the apps' list without what it has no
    /// use for (`seccomp::INGRESS_WITHOUT`).
    Ingress,
}

impl Profile {
    /// The profile named `name` (`app`, `ingress`).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "app" => Some(Self::App),
            "ingress" => Some(Self::Ingress),
            _ => None,
        }
    }
}

impl Args {
    /// Parse the runner's arguments (without the program name).
    ///
    /// # Errors
    ///
    /// What is wrong with them.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut it = args.into_iter();
        let mut program = None;
        let mut read = Vec::new();
        let mut write = Vec::new();
        let mut landlock_required = true;
        let mut lease = false;
        let mut accept_fds = Vec::new();
        let mut profile = Profile::App;
        let mut unconfined = false;
        let mut own = false;
        let mut own_sockets = OwnSockets::default();
        let mut port_lists = false;
        while let Some(flag) = it.next() {
            match flag.as_str() {
                f if f == bulkhead_native::lease::FLAG => {
                    lease = true;
                    continue;
                }
                "--unconfined" => {
                    unconfined = true;
                    continue;
                }
                "--own-sockets" => {
                    own = true;
                    continue;
                }
                "--listen" => {
                    own_sockets.listen = true;
                    port_lists = true;
                    continue;
                }
                _ => {}
            }
            let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
            match flag.as_str() {
                "--library" => program = Some(Program::Library(value()?.into())),
                "--executable" => program = Some(Program::Executable(value()?.into())),
                "--read" => read.push(value()?.into()),
                "--write" => write.push(value()?.into()),
                "--profile" => {
                    let v = value()?;
                    profile = Profile::parse(&v)
                        .ok_or_else(|| format!("--profile {v}: app or ingress"))?;
                }
                "--connect-ports" => {
                    own_sockets.connect = parse_ports(&value()?)?;
                    port_lists = true;
                }
                "--bind-ports" => {
                    own_sockets.bind = parse_ports(&value()?)?;
                    port_lists = true;
                }
                "--accept-fd" => {
                    let v = value()?;
                    accept_fds.push(
                        v.parse()
                            .map_err(|_| format!("--accept-fd {v}: a descriptor number"))?,
                    );
                }
                "--landlock" => {
                    landlock_required = match value()?.as_str() {
                        "required" => true,
                        "best-effort" => false,
                        other => {
                            return Err(format!("--landlock {other}: required or best-effort"));
                        }
                    }
                }
                other => return Err(format!("unknown argument {other}")),
            }
        }
        let program = program.ok_or("--library or --executable is required")?;
        if (unconfined || own) && (profile != Profile::App || !accept_fds.is_empty()) {
            return Err(
                "--unconfined and --own-sockets are for apps; a driver keeps its fences".into(),
            );
        }
        if unconfined && own {
            return Err("--unconfined or --own-sockets, not both".into());
        }
        if port_lists && !own {
            return Err("--listen, --connect-ports and --bind-ports go with --own-sockets".into());
        }
        Ok(Self {
            program,
            read,
            write,
            landlock_required,
            lease,
            accept_fds,
            profile,
            unconfined,
            own_sockets: own.then_some(own_sockets),
        })
    }
}

/// The runner's `main`.
pub fn main() -> ExitCode {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("bulkhead-runner: {e}");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = not_dumpable() {
        eprintln!("bulkhead-runner: not dumpable: {e}");
        return ExitCode::from(2);
    }
    if args.lease
        && let Err(e) = watchdog::fork()
    {
        eprintln!("bulkhead-runner: lease: {e}");
        return ExitCode::from(2);
    }
    #[cfg(target_os = "linux")]
    if !args.unconfined
        && let Err(e) = sandbox::restrict(&args)
    {
        eprintln!("bulkhead-runner: landlock: {e}");
        return ExitCode::from(2);
    }
    #[cfg(target_os = "linux")]
    if !args.unconfined
        && let Err(e) =
            seccomp::install_with(&args.accept_fds, args.profile, args.own_sockets.as_ref())
    {
        eprintln!("bulkhead-runner: seccomp: {e}");
        return ExitCode::from(2);
    }
    match &args.program {
        // A plugin is loaded by path at `init`, which capability mode
        // refuses (ROADMAP FB2).
        #[cfg(target_os = "freebsd")]
        Program::Library(_) => {
            eprintln!(
                "bulkhead-runner: a rustbridge plugin cannot run in capability mode \
                 on FreeBSD (it is loaded by path); build the app as an executable"
            );
            ExitCode::from(2)
        }
        #[cfg(not(target_os = "freebsd"))]
        Program::Library(path) => library::serve(path.clone()),
        Program::Executable(path) => {
            // The environment is the node's choice, already narrowed to
            // what the app may know.
            let env: Vec<(String, String)> = [
                bulkhead_native::protocol::PRIV_ENV,
                bulkhead_native::preopens::ENV,
                bulkhead_native::channel::SPIN_ENV,
                bulkhead_native::channel::SENDER_SPIN_ENV,
                bulkhead_native::driver::LISTENERS_ENV,
            ]
            .into_iter()
            .filter_map(|var| std::env::var(var).ok().map(|v| (var.to_string(), v)))
            .collect();
            #[cfg(target_os = "freebsd")]
            if !args.unconfined && args.own_sockets.is_none() {
                return capsicum::exec(&args, path, env);
            }
            // Unconfined or with its own sockets, the app opens its
            // preopens by path, as without the descriptors capability
            // mode needs.
            {
                let mut cmd = std::process::Command::new(path);
                cmd.env_clear().envs(env);
                let err = cmd.exec();
                eprintln!("bulkhead-runner: {}: {err}", path.display());
                ExitCode::from(2)
            }
        }
    }
}

/// No core dump, and no ptrace from other processes of the node's user.
#[cfg(target_os = "linux")]
fn not_dumpable() -> rustix::io::Result<()> {
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
}

/// As on Linux, and kept across the exec into the app.
#[cfg(target_os = "freebsd")]
fn not_dumpable() -> std::io::Result<()> {
    bulkhead_native::freebsd::set_not_traceable(true)
}

#[cfg(test)]
#[path = "runner_tests.rs"]
mod runner_tests;
