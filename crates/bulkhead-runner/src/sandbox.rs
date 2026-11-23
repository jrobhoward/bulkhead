//! Landlock for a native app: what it may read, write and execute, and no
//! TCP; for an app with its own sockets (ROADMAP AP2), TCP to and from
//! the ports it lists.

use std::path::PathBuf;

use landlock::{
    ABI, Access, AccessFs, AccessNet, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus, Scope, path_beneath_rules,
};

use super::{Args, Program};

/// The newest Landlock ABI the rules are written for. Older kernels get
/// what they support (`best-effort` compatibility); the seccomp filter
/// covers signals, which only ABI 6 (Linux 6.12) scopes.
const ABI_TARGET: ABI = ABI::V6;

/// System directories every program reads its libraries from.
const SYSTEM: &[&str] = &["/usr", "/lib", "/lib64", "/etc/ld.so.cache"];

/// What an app with its own sockets reads for name lookups (the helper
/// puts them in its root).
const NAMES: &[&str] = &["/etc/resolv.conf", "/etc/hosts", "/etc/nsswitch.conf"];

/// The directories and files the program may read.
#[must_use]
pub fn readable(args: &Args) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = SYSTEM
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect();
    if let Some(dir) = args.program.path().parent() {
        out.push(dir.to_path_buf());
    }
    out.extend(args.read.iter().cloned());
    if args.own_sockets.is_some() {
        out.extend(NAMES.iter().map(PathBuf::from).filter(|p| p.exists()));
    }
    out
}

/// The TCP rights Landlock handles, and the ports it allows for each:
/// every right with no ports (no TCP) for an app the node makes sockets
/// for; for one with its own sockets (ROADMAP AP2), connect and bind each
/// to the ports listed, or left unhandled for any port (or too many to
/// list: the helper's fence holds them).
#[must_use]
pub fn tcp(args: &Args) -> (landlock::BitFlags<AccessNet>, Vec<u16>, Vec<u16>) {
    let Some(own) = &args.own_sockets else {
        return (AccessNet::from_all(ABI_TARGET), Vec::new(), Vec::new());
    };
    let connect = crate::landlock_ports(own.connect.as_deref());
    // Port 0 too: Landlock cannot name the unspecified address, whose bind
    // with port 0 (before a connect) needs no rule; the helper's programs
    // hold the address.
    let bind = crate::landlock_ports(own.bind.as_deref()).map(|mut p| {
        p.insert(0, 0);
        p
    });
    let mut handled = landlock::BitFlags::<AccessNet>::EMPTY;
    if connect.is_some() {
        handled |= AccessNet::ConnectTcp;
    }
    if bind.is_some() {
        handled |= AccessNet::BindTcp;
    }
    (
        handled,
        connect.unwrap_or_default(),
        bind.unwrap_or_default(),
    )
}

/// What a read-write volume allows: reading, writing and truncating
/// files, making and removing files, directories and symbolic links, and
/// renaming within it (`Refer`). Not executing, nor making devices,
/// sockets or pipes, nor device ioctls.
#[must_use]
pub fn writable_access() -> landlock::BitFlags<AccessFs> {
    AccessFs::ReadFile
        | AccessFs::ReadDir
        | AccessFs::WriteFile
        | AccessFs::Truncate
        | AccessFs::RemoveFile
        | AccessFs::RemoveDir
        | AccessFs::MakeReg
        | AccessFs::MakeDir
        | AccessFs::MakeSym
        | AccessFs::Refer
}

/// Restrict this process with Landlock. Read access is `ReadFile` and
/// `ReadDir` only: loading a library maps it, which needs read, and
/// execution is granted on the executable alone. Each `--write`
/// directory gets [`writable_access`].
///
/// # Errors
///
/// A Landlock error, or, with `--landlock required`, a kernel that does
/// not enforce it.
pub fn restrict(args: &Args) -> Result<(), String> {
    let abi = ABI_TARGET;
    let read = AccessFs::ReadFile | AccessFs::ReadDir;
    let (net, connect, bind) = tcp(args);
    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .and_then(|r| {
            if net.is_empty() {
                Ok(r)
            } else {
                r.handle_access(net)
            }
        })
        .and_then(|r| r.scope(Scope::from_all(abi)))
        .and_then(Ruleset::create)
        .and_then(|r| r.add_rules(path_beneath_rules(readable(args), read)))
        .and_then(|r| r.add_rules(path_beneath_rules(&args.write, writable_access())))
        .map_err(|e| e.to_string())?;
    if let Program::Executable(path) = &args.program {
        // The kernel opens a dynamic executable's interpreter (`ld.so`)
        // for execution too.
        let interpreter = interpreter(path).map_err(|e| format!("{}: {e}", path.display()))?;
        for file in std::iter::once(path.clone()).chain(interpreter) {
            ruleset = ruleset
                .add_rule(PathBeneath::new(
                    PathFd::new(&file).map_err(|e| format!("{}: {e}", file.display()))?,
                    AccessFs::Execute | AccessFs::ReadFile,
                ))
                .map_err(|e| e.to_string())?;
        }
    }
    for (ports, access) in [(connect, AccessNet::ConnectTcp), (bind, AccessNet::BindTcp)] {
        for port in ports {
            ruleset = ruleset
                .add_rule(landlock::NetPort::new(port, access))
                .map_err(|e| e.to_string())?;
        }
    }
    let status = ruleset.restrict_self().map_err(|e| e.to_string())?;
    if status.ruleset == RulesetStatus::NotEnforced && args.landlock_required {
        return Err("this kernel does not enforce Landlock (--landlock required)".into());
    }
    Ok(())
}

/// The interpreter a 64-bit little-endian ELF executable names
/// (`PT_INTERP`); `None` for a static one.
///
/// # Errors
///
/// The file cannot be read, or is not such an ELF file.
pub fn interpreter(path: &std::path::Path) -> std::io::Result<Option<PathBuf>> {
    let bytes = std::fs::read(path)?;
    parse_interpreter(&bytes)
        .ok_or_else(|| std::io::Error::other("not a 64-bit little-endian ELF executable"))
}

/// [`interpreter`] on the file's bytes.
#[must_use]
pub fn parse_interpreter(elf: &[u8]) -> Option<Option<PathBuf>> {
    const PT_INTERP: u32 = 3;
    let u16_at = |at: usize| Some(u16::from_le_bytes(elf.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(elf.get(at..at + 4)?.try_into().ok()?));
    let u64_at = |at: usize| {
        let v = u64::from_le_bytes(elf.get(at..at + 8)?.try_into().ok()?);
        usize::try_from(v).ok()
    };
    // Magic, 64-bit class, little-endian.
    if elf.get(..6)? != b"\x7fELF\x02\x01" {
        return None;
    }
    let phoff = u64_at(0x20)?;
    let phentsize = usize::from(u16_at(0x36)?);
    let phnum = usize::from(u16_at(0x38)?);
    for i in 0..phnum {
        let ph = phoff.checked_add(i.checked_mul(phentsize)?)?;
        if u32_at(ph)? != PT_INTERP {
            continue;
        }
        let offset = u64_at(ph + 8)?;
        let size = u64_at(ph + 32)?;
        let raw = elf.get(offset..offset.checked_add(size)?)?;
        let name = raw.split(|b| *b == 0).next()?;
        return Some(Some(PathBuf::from(std::str::from_utf8(name).ok()?)));
    }
    Some(None)
}
