#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

#[test]
fn seccomp____runner_list____every_call_on_the_helpers() {
    let helper = allowed();
    let runner = bulkhead_runner::seccomp::allowed();
    let answered = bulkhead_runner::seccomp::rules(&[1], &[3]).unwrap();

    let missing: Vec<i64> = runner
        .iter()
        .chain(answered.keys())
        .chain(&[libc::SYS_clone3])
        .filter(|call| !helper.contains(call))
        .copied()
        .collect();

    assert!(missing.is_empty(), "off the helper's list: {missing:?}");
}

#[rstest]
#[case::open_by_handle_at(libc::SYS_open_by_handle_at)]
#[case::name_to_handle_at(libc::SYS_name_to_handle_at)]
#[case::bpf(libc::SYS_bpf)]
#[case::init_module(libc::SYS_init_module)]
#[case::finit_module(libc::SYS_finit_module)]
#[case::delete_module(libc::SYS_delete_module)]
#[case::kexec_load(libc::SYS_kexec_load)]
#[case::keyctl(libc::SYS_keyctl)]
#[case::add_key(libc::SYS_add_key)]
#[case::request_key(libc::SYS_request_key)]
#[case::perf_event_open(libc::SYS_perf_event_open)]
#[case::userfaultfd(libc::SYS_userfaultfd)]
#[case::fsopen(libc::SYS_fsopen)]
#[case::fsmount(libc::SYS_fsmount)]
#[case::move_mount(libc::SYS_move_mount)]
#[case::open_tree(libc::SYS_open_tree)]
#[case::mount_setattr(libc::SYS_mount_setattr)]
#[case::setns(libc::SYS_setns)]
#[case::unshare(libc::SYS_unshare)]
#[case::swapon(libc::SYS_swapon)]
#[case::reboot(libc::SYS_reboot)]
#[case::clock_settime(libc::SYS_clock_settime)]
#[case::settimeofday(libc::SYS_settimeofday)]
#[case::chroot(libc::SYS_chroot)]
fn seccomp____a_call_neither_uses____off_the_list(#[case] call: i64) {
    assert!(!allowed().contains(&call));
}

#[test]
fn seccomp____program_for_this_architecture____builds() {
    let program = program(SeccompAction::Trap).unwrap();

    assert!(!program.is_empty());
}
