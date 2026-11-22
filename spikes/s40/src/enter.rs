//! `s40 enter RULES on|off ROLE...`: in the fenced child, already under
//! its uid: the runner-like fences, then the role, in this process.
//!
//! Linux: Landlock (the whole filesystem read-only; with `on`, TCP
//! connect and bind only to the rules' ports, a second fence beside the
//! cgroup's; abstract Unix sockets and signals scoped), then a seccomp
//! filter that refuses sockets of other domains and raw types. FreeBSD:
//! none; the jail is the fence, and capability mode is off, since it
//! refuses `connect` and `bind`.

pub fn main(args: &[String]) {
    let [rules, net, role @ ..] = args else {
        eprintln!("usage: s40 enter RULES on|off ROLE...");
        std::process::exit(2);
    };
    #[cfg(target_os = "linux")]
    {
        let rules = crate::rules::Rules::read(rules).unwrap_or_else(|e| panic!("{e}"));
        match linux::fence(&rules, net == "on") {
            Ok(note) => eprintln!("s40 enter: {note}"),
            Err(e) => {
                eprintln!("s40 enter: {e}");
                std::process::exit(1);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (rules, net);
    crate::role(role);
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::BTreeMap;

    use landlock::{
        ABI, Access, AccessFs, AccessNet, BitFlags, NetPort, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus, Scope, path_beneath_rules,
    };
    use seccompiler::{
        SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule, TargetArch,
    };

    use crate::rules::{Rule, Rules};

    /// Every port of `rules`, or `None` if one allows any port (Landlock
    /// rules name single ports, and cannot say "any").
    fn ports(rules: &[Rule]) -> Option<Vec<u16>> {
        let mut out = Vec::new();
        for r in rules {
            let (lo, hi) = r.ports?;
            out.extend(lo..=hi);
        }
        out.sort_unstable();
        out.dedup();
        Some(out)
    }

    pub fn fence(rules: &Rules, landlock_net: bool) -> Result<String, String> {
        let abi = ABI::V6;
        let connect = rules.connect()?;
        let bind = rules.bind()?;
        let (cports, bports) = (ports(&connect), ports(&bind));
        let mut net: BitFlags<AccessNet> = BitFlags::EMPTY;
        if landlock_net && cports.is_some() {
            net |= AccessNet::ConnectTcp;
        }
        if landlock_net && bports.is_some() {
            net |= AccessNet::BindTcp;
        }
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
            .and_then(|r| {
                r.add_rules(path_beneath_rules(
                    ["/"],
                    AccessFs::ReadFile | AccessFs::ReadDir,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut count = 0;
        if net.contains(AccessNet::ConnectTcp) {
            for p in cports.unwrap_or_default() {
                ruleset = ruleset
                    .add_rule(NetPort::new(p, AccessNet::ConnectTcp))
                    .map_err(|e| e.to_string())?;
                count += 1;
            }
        }
        if net.contains(AccessNet::BindTcp) {
            for p in bports.unwrap_or_default() {
                ruleset = ruleset
                    .add_rule(NetPort::new(p, AccessNet::BindTcp))
                    .map_err(|e| e.to_string())?;
                count += 1;
            }
        }
        let status = ruleset.restrict_self().map_err(|e| e.to_string())?;
        let enforced = match status.ruleset {
            RulesetStatus::FullyEnforced => "fully",
            RulesetStatus::PartiallyEnforced => "partly",
            RulesetStatus::NotEnforced => "not",
        };
        seccomp()?;
        Ok(format!(
            "landlock {enforced} enforced, net {:?} with {count} port rules; seccomp socket filter on",
            net
        ))
    }

    /// Refuse `socket` for domains other than Unix, IPv4 and IPv6 and for
    /// raw, packet and reliable-datagram types, and io_uring; allow the
    /// rest (the runner's allow-list is not the question here).
    fn seccomp() -> Result<(), String> {
        // The obsolete packet type (libc deprecates the constant).
        const SOCK_PACKET: i32 = 10;
        let cond = |arg, op, val| {
            SeccompCondition::new(arg, SeccompCmpArgLen::Dword, op, val).map_err(|e| e.to_string())
        };
        let ty = |t: i32| -> Result<SeccompRule, String> {
            SeccompRule::new(vec![cond(1, SeccompCmpOp::MaskedEq(0xf), t as u64)?])
                .map_err(|e| e.to_string())
        };
        let domain = SeccompRule::new(vec![
            cond(0, SeccompCmpOp::Ne, libc::AF_UNIX as u64)?,
            cond(0, SeccompCmpOp::Ne, libc::AF_INET as u64)?,
            cond(0, SeccompCmpOp::Ne, libc::AF_INET6 as u64)?,
        ])
        .map_err(|e| e.to_string())?;
        let mut rules = BTreeMap::new();
        rules.insert(
            libc::SYS_socket,
            vec![
                domain,
                ty(libc::SOCK_RAW)?,
                ty(libc::SOCK_RDM)?,
                ty(SOCK_PACKET)?,
            ],
        );
        rules.insert(libc::SYS_io_uring_setup, Vec::new());
        let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(|e| e.to_string())?;
        let filter = SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::Errno(libc::EPERM as u32),
            arch,
        )
        .map_err(|e| e.to_string())?;
        let program: seccompiler::BpfProgram = filter
            .try_into()
            .map_err(|e: seccompiler::BackendError| e.to_string())?;
        seccompiler::apply_filter_all_threads(&program).map_err(|e| e.to_string())
    }
}
