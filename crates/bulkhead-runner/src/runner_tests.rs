#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::path::PathBuf;

use super::{Args, Program};

fn args(s: &str) -> Result<Args, String> {
    Args::parse(s.split_whitespace().map(String::from))
}

#[test]
fn parse____library_with_reads____all_kept() {
    let found =
        args("--library /b/lib.so --read /data --read /etc/x --landlock best-effort").unwrap();

    assert_eq!(found.program, Program::Library("/b/lib.so".into()));
    assert_eq!(
        found.read,
        vec![PathBuf::from("/data"), PathBuf::from("/etc/x")]
    );
    assert!(!found.landlock_required);
}

#[test]
fn parse____writes_beside_reads____kept_apart() {
    let found = args("--executable /b/app --read /priv --write /data --write /logs").unwrap();

    assert_eq!(found.read, vec![PathBuf::from("/priv")]);
    assert_eq!(
        found.write,
        vec![PathBuf::from("/data"), PathBuf::from("/logs")]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn writable_access____a_volume____no_execute_or_devices() {
    use landlock::AccessFs;

    let access = super::sandbox::writable_access();

    assert!(access.contains(AccessFs::WriteFile | AccessFs::MakeReg | AccessFs::Refer));
    assert!(!access.intersects(
        AccessFs::Execute | AccessFs::MakeChar | AccessFs::MakeBlock | AccessFs::MakeSock
    ));
}

#[test]
fn parse____executable_only____landlock_required() {
    let found = args("--executable /b/app").unwrap();

    assert_eq!(found.program, Program::Executable("/b/app".into()));
    assert!(found.landlock_required);
    assert!(!found.unconfined);
}

#[test]
fn parse____unconfined____kept() {
    let found = args("--executable /b/app --write /data --unconfined").unwrap();

    assert!(found.unconfined);
}

#[rstest::rstest]
#[case("--executable /b/app --unconfined --profile ingress")]
#[case("--executable /b/app --unconfined --accept-fd 8")]
fn parse____unconfined_driver____refused(#[case] s: &str) {
    let found = args(s).unwrap_err();

    assert!(found.contains("driver keeps its fences"), "{found}");
}

#[rstest::rstest]
#[case("")]
#[case("--read /x")]
#[case("--executable /a --write")]
#[case("--library")]
#[case("--library /a --landlock maybe")]
#[case("--library /a --verbose")]
fn parse____bad_arguments____refused(#[case] s: &str) {
    let found = args(s);

    assert!(found.is_err(), "{found:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn readable____program_directory_and_reads____included() {
    let a = args("--library /b/lib.so --read /data").unwrap();

    let found = super::sandbox::readable(&a);

    assert!(found.contains(&PathBuf::from("/b")));
    assert!(found.contains(&PathBuf::from("/data")));
    assert!(!found.iter().any(|p| p == &PathBuf::from("/b/lib.so")));
}

#[cfg(target_os = "linux")]
#[test]
fn rules____signals_and_tracing____listed() {
    let found = super::seccomp::rules(&[1234], &[]).unwrap();

    assert_eq!(found[&libc::SYS_kill].len(), 1);
    assert!(found[&libc::SYS_ptrace].is_empty());
    assert!(found[&libc::SYS_socket].is_empty());
    for call in [
        libc::SYS_connect,
        libc::SYS_accept,
        libc::SYS_accept4,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ] {
        assert!(found[&call].is_empty(), "{call}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn rules____listening_sockets_passed____accept4_refused_unless_one_of_them() {
    let found = super::seccomp::rules(&[1234], &[8, 9]).unwrap();

    assert_eq!(found[&libc::SYS_accept4].len(), 1);
    assert!(found[&libc::SYS_accept].is_empty());
    assert!(found[&libc::SYS_socket].is_empty());
}

#[test]
fn parse____accept_fds____kept() {
    let args = Args::parse(
        [
            "--executable",
            "/a/app",
            "--accept-fd",
            "8",
            "--accept-fd",
            "9",
        ]
        .map(String::from),
    )
    .unwrap();

    assert_eq!(args.accept_fds, vec![8, 9]);
}

#[cfg(target_os = "linux")]
#[test]
fn rules____arguments_decide____one_rule_each_and_one_per_namespace_flag() {
    let found = super::seccomp::rules(&[1234], &[]).unwrap();

    assert_eq!(found[&libc::SYS_clone].len(), 8);
    assert_eq!(found[&libc::SYS_socketpair].len(), 1);
    assert_eq!(found[&libc::SYS_ioctl].len(), 1);
}

#[cfg(target_os = "linux")]
#[test]
fn allowed____what_an_app_needs____listed_and_what_leaves_its_fence_not() {
    let found = super::seccomp::allowed();

    for call in [
        libc::SYS_read,
        libc::SYS_futex,
        libc::SYS_openat,
        libc::SYS_execve,
    ] {
        assert!(found.contains(&call), "{call}");
    }
    for call in [
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_mount,
        libc::SYS_bpf,
        libc::SYS_keyctl,
        libc::SYS_perf_event_open,
        libc::SYS_memfd_create,
        libc::SYS_seccomp,
        libc::SYS_setuid,
    ] {
        assert!(!found.contains(&call), "{call}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn allowed____every_call_the_rules_refuse____listed() {
    let allowed = super::seccomp::allowed();

    let rules = super::seccomp::rules(&[1234], &[]).unwrap();

    // Otherwise the allow-list's trap, the stricter answer, would win.
    for call in rules.keys() {
        assert!(allowed.contains(call), "{call}");
    }
    assert!(allowed.contains(&libc::SYS_clone3));
}

#[cfg(target_os = "linux")]
#[test]
fn interpreter____this_test_binary____dynamic_loader() {
    let me = std::env::current_exe().unwrap();

    let found = super::sandbox::interpreter(&me).unwrap();

    let name = found.unwrap().display().to_string();
    assert!(name.contains("ld-linux"), "{name}");
}

#[cfg(target_os = "linux")]
#[rstest::rstest]
#[case(b"#!/bin/sh\n".to_vec())]
#[case(b"\x7fELF\x01\x01".to_vec())]
#[case(b"\x7fELF\x02\x01".to_vec())]
fn parse_interpreter____not_a_whole_elf64____none(#[case] bytes: Vec<u8>) {
    let found = super::sandbox::parse_interpreter(&bytes);

    assert!(found.is_none(), "{found:?}");
}

#[test]
fn args____profile____parsed_and_refused_unknown() {
    let parse = |p: &str| {
        Args::parse(
            ["--executable", "/bin/true", "--profile", p]
                .into_iter()
                .map(String::from),
        )
    };

    let ingress = parse("ingress").unwrap();
    let unknown = parse("sandbox").unwrap_err();

    assert_eq!(ingress.profile, crate::Profile::Ingress);
    assert!(unknown.contains("app or ingress"), "{unknown}");
}

#[cfg(target_os = "linux")]
#[test]
fn ingress_profile____the_apps_list_without_process_and_file_changes____and_builds() {
    use super::seccomp::{INGRESS_WITHOUT, Profile, allowed, allowed_for};

    let app = allowed();
    let ingress = allowed_for(Profile::Ingress);
    let installed = super::seccomp::rules(&[1234], &[8]).map(|_| ());

    assert!(ingress.iter().all(|c| app.contains(c)));
    assert!(INGRESS_WITHOUT.iter().all(|c| !ingress.contains(c)));
    assert!(
        INGRESS_WITHOUT.iter().all(|c| app.contains(c)),
        "only calls the apps' list has"
    );
    for kept in [
        libc::SYS_execve,
        libc::SYS_accept4,
        libc::SYS_openat,
        libc::SYS_futex,
        // tokio's signal driver, built in beside the node (the e2e run on
        // the Ubuntu VM, 2026-11-21).
        libc::SYS_socketpair,
    ] {
        assert!(ingress.contains(&kept), "{kept}");
    }
    assert!(installed.is_ok());
}

#[test]
fn parse____own_sockets____listen_and_port_lists_kept() {
    let found = args("--executable /b/app --own-sockets --listen --connect-ports 1883,8000-8100 --bind-ports any").unwrap();

    let own = found.own_sockets.unwrap();
    assert!(own.listen);
    assert_eq!(own.connect, Some(vec![(1883, 1883), (8000, 8100)]));
    assert_eq!(own.bind, None);
    assert!(!found.unconfined);
}

#[rstest::rstest]
#[case("--executable /b/app --own-sockets --unconfined", "not both")]
#[case(
    "--executable /b/app --own-sockets --profile ingress",
    "driver keeps its fences"
)]
#[case("--executable /b/app --listen", "go with --own-sockets")]
#[case("--executable /b/app --connect-ports 80", "go with --own-sockets")]
#[case(
    "--executable /b/app --own-sockets --connect-ports 9-1",
    "not a port or a range"
)]
fn parse____bad_own_sockets____refused(#[case] s: &str, #[case] message: &str) {
    let found = args(s).unwrap_err();

    assert!(found.contains(message), "{found}");
}

#[test]
fn landlock_ports____ranges_any_or_too_many____listed_or_none() {
    assert_eq!(
        super::landlock_ports(Some(&[(80, 80), (8000, 8002), (8001, 8001)])),
        Some(vec![80, 8000, 8001, 8002])
    );
    assert_eq!(super::landlock_ports(None), None);
    assert_eq!(super::landlock_ports(Some(&[(1, 2000)])), None);
}

#[cfg(target_os = "linux")]
#[test]
fn tcp____node_or_own_sockets____every_port_refused_or_the_listed_only() {
    use landlock::AccessNet;

    let node = args("--executable /b/app").unwrap();
    let own =
        args("--executable /b/app --own-sockets --connect-ports 1883 --bind-ports any").unwrap();
    let binds =
        args("--executable /b/app --own-sockets --connect-ports any --bind-ports 9000").unwrap();

    let (handled, connect, bind) = super::sandbox::tcp(&node);
    assert!(handled.contains(AccessNet::ConnectTcp | AccessNet::BindTcp));
    assert!(connect.is_empty() && bind.is_empty());
    let (handled, connect, bind) = super::sandbox::tcp(&own);
    assert_eq!(handled, AccessNet::ConnectTcp);
    assert_eq!(connect, [1883]);
    assert!(bind.is_empty());
    let (handled, _, bind) = super::sandbox::tcp(&binds);
    assert_eq!(handled, AccessNet::BindTcp);
    assert_eq!(bind, [0, 9000], "port 0: the bind before a connect");
    assert!(
        super::sandbox::readable(&own)
            .iter()
            .any(|p| p.ends_with("hosts"))
            || !std::path::Path::new("/etc/hosts").exists()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn rules_with____own_sockets____connect_allowed_socket_domains_and_listen_refused() {
    let quiet = crate::OwnSockets::default();
    let listening = crate::OwnSockets {
        listen: true,
        ..crate::OwnSockets::default()
    };

    let found = super::seccomp::rules_with(&[1234], &[], Some(&quiet)).unwrap();
    let may_listen = super::seccomp::rules_with(&[1234], &[], Some(&listening)).unwrap();
    let calls = super::seccomp::allowed_with(crate::Profile::App, Some(&quiet));

    assert_eq!(
        found[&libc::SYS_socket].len(),
        4,
        "domains, raw, rdm, packet"
    );
    for call in [libc::SYS_connect, libc::SYS_accept, libc::SYS_accept4] {
        assert!(!found.contains_key(&call), "{call}");
    }
    assert!(found[&libc::SYS_listen].is_empty());
    assert!(!may_listen.contains_key(&libc::SYS_listen));
    assert!(calls.contains(&libc::SYS_bind) && calls.contains(&libc::SYS_listen));
    assert!(!super::seccomp::allowed_for(crate::Profile::App).contains(&libc::SYS_bind));
}
