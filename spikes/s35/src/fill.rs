//! What an app sees at its quota: write 1 MiB pieces of incompressible
//! data into 16 MiB files until a write (or a close's flush) is refused,
//! then whether a further write is refused too, and whether removing a
//! file lets writing go on.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

const PIECE: usize = 1 << 20;
const PIECES_PER_FILE: usize = 16;

fn errno_name(e: &io::Error) -> String {
    let n = e.raw_os_error().unwrap_or(0);
    let name = match n {
        libc::EDQUOT => "EDQUOT",
        libc::ENOSPC => "ENOSPC",
        libc::EACCES => "EACCES",
        libc::EPERM => "EPERM",
        libc::EROFS => "EROFS",
        _ => "other",
    };
    format!("{name} ({n}: {e})")
}

/// The fill described above, in `dir`, as `uid` (its own group) if given.
pub fn fill(dir: &str, uid: Option<u32>) -> i32 {
    if let Some(u) = uid {
        // SAFETY: plain calls on this single-threaded process.
        let ok = unsafe { libc::setgroups(0, std::ptr::null()) == 0 && libc::setgid(u) == 0 && libc::setuid(u) == 0 };
        if !ok {
            println!("drop to {u}: {}", io::Error::last_os_error());
            return 1;
        }
    }
    let dir = Path::new(dir);
    report_statvfs(dir, "before");
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut piece = vec![0u8; PIECE];
    let mut written: u64 = 0;
    let started = Instant::now();
    let mut refused = None;
    'files: for f in 0.. {
        let path = dir.join(format!("fill-{f}"));
        let mut file = match File::create(&path) {
            Ok(file) => file,
            Err(e) => {
                refused = Some(format!("create {}: {}", path.display(), errno_name(&e)));
                break;
            }
        };
        for _ in 0..PIECES_PER_FILE {
            random(&mut seed, &mut piece);
            if let Err(e) = file.write_all(&piece) {
                refused = Some(format!("write at {written} bytes: {}", errno_name(&e)));
                break 'files;
            }
            written += PIECE as u64;
        }
        if let Err(e) = file.sync_all() {
            refused = Some(format!("fsync at {written} bytes: {}", errno_name(&e)));
            break;
        }
    }
    println!("written={written} bytes ({:.1} MiB) in {:?}", written as f64 / PIECE as f64, started.elapsed());
    println!("refused: {}", refused.as_deref().unwrap_or("never"));
    report_statvfs(dir, "at quota");

    // After the pool's next transaction group.
    std::thread::sleep(Duration::from_secs(6));
    println!("a further 1 MiB file: {}", one_more(dir, "more", &mut seed));
    let removed = fs::remove_file(dir.join("fill-0")).map_or_else(|e| errno_name(&e), |()| "ok".into());
    println!("remove fill-0: {removed}");
    std::thread::sleep(Duration::from_secs(6));
    report_statvfs(dir, "after remove");
    println!("a 1 MiB file after remove: {}", one_more(dir, "after", &mut seed));
    0
}

fn one_more(dir: &Path, name: &str, seed: &mut u64) -> String {
    let mut piece = vec![0u8; PIECE];
    random(seed, &mut piece);
    let r = File::create(dir.join(name)).and_then(|mut f| {
        f.write_all(&piece)?;
        f.sync_all()
    });
    r.map_or_else(|e| errno_name(&e), |()| "ok".into())
}

fn random(seed: &mut u64, out: &mut [u8]) {
    for chunk in out.chunks_mut(8) {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        let b = seed.to_le_bytes();
        chunk.copy_from_slice(&b[..chunk.len()]);
    }
}

fn report_statvfs(dir: &Path, when: &str) {
    let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a zeroed statvfs is valid, and `c` is a C string.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        println!("statvfs {when}: {}", io::Error::last_os_error());
        return;
    }
    let mib = |blocks: u64| blocks as f64 * st.f_frsize as f64 / PIECE as f64;
    println!(
        "statvfs {when}: size {:.1} MiB, available {:.1} MiB",
        mib(st.f_blocks as u64),
        mib(st.f_bavail as u64)
    );
}
