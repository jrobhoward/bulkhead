#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn create____limits____memory_and_pids_written_no_cpu() {
    let root = tempfile::tempdir().unwrap();

    let dir = create(root.path(), "i1", 64 << 20, 32).unwrap();

    let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap();
    assert_eq!(read("memory.max"), (64u64 << 20).to_string());
    assert_eq!(read("pids.max"), "32");
    assert!(!dir.join("cpu.max").exists());
}
