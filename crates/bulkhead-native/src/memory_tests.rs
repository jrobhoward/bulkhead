#![allow(non_snake_case)]

use std::ffi::OsString;

use rstest::rstest;

use super::*;

#[rstest]
#[case::nothing(&[], false)]
#[case::mmap(&[("MALLOC_MMAP_THRESHOLD_", "131072")], true)]
#[case::trim(&[("MALLOC_TRIM_THRESHOLD_", "131072")], true)]
#[case::tunable_mmap(&[("GLIBC_TUNABLES", "glibc.malloc.mmap_threshold=131072")], true)]
#[case::tunable_trim(&[("GLIBC_TUNABLES", "glibc.malloc.arena_max=2:glibc.malloc.trim_threshold=1")], true)]
#[case::tunable_other(&[("GLIBC_TUNABLES", "glibc.malloc.arena_max=2")], false)]
#[case::arena_only(&[("MALLOC_ARENA_MAX", "2")], false)]
fn set_by_environment__an_operators_threshold_wins(
    #[case] vars: &[(&str, &str)],
    #[case] expected: bool,
) {
    let var = |name: &str| {
        vars.iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| OsString::from(v))
    };
    assert_eq!(set_by_environment(var), expected);
}

#[test]
fn keep_freed_memory__sets_or_says_why_not() {
    let kept = keep_freed_memory(FOR_NODE);
    if cfg!(all(target_os = "linux", target_env = "gnu")) {
        assert!(matches!(kept, Kept::Set | Kept::Environment), "{kept:?}");
    } else {
        assert!(
            matches!(kept, Kept::Unsupported | Kept::Environment),
            "{kept:?}"
        );
    }
}
