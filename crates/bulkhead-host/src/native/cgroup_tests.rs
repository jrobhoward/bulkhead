#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

#[rstest]
#[case::half(Some(500), "50000 100000")]
#[case::two(Some(2000), "200000 100000")]
#[case::least(Some(10), "1000 100000")]
#[case::none(None, "max 100000")]
fn cpu_max____a_limit____quota_of_the_period(#[case] millis: Option<u32>, #[case] text: &str) {
    assert_eq!(cpu_max(millis), text);
}

#[test]
fn create____a_cpu_limit____written_with_the_others() {
    let root = tempfile::tempdir().unwrap();

    let dir = create(root.path(), "i1", 64 << 20, 32, Some(250)).unwrap();

    let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap();
    assert_eq!(read("memory.max"), (64u64 << 20).to_string());
    assert_eq!(read("pids.max"), "32");
    assert_eq!(read("cpu.max"), "25000 100000");
}
