//! Native programs: the platforms a `native-process` app runs on, and the
//! check that a program is an ELF file for its platform's architecture.

use rustbridge_bundle::Platform;

use crate::BundleError;

/// The platforms a `native-process` app runs on: those `bulkhead-runner`
/// sandboxes (Linux, and FreeBSD since ROADMAP FB1).
pub const NATIVE_PLATFORMS: [Platform; 4] = [
    Platform::LinuxX86_64,
    Platform::LinuxAarch64,
    Platform::FreebsdX86_64,
    Platform::FreebsdAarch64,
];

/// ELF `e_machine` values.
const EM_X86_64: u16 = 62;
const EM_AARCH64: u16 = 183;

/// ELF `EI_OSABI` for FreeBSD: its linker brands every program with it.
/// Linux toolchains write 0 (System V) or 3 (GNU), so the byte tells a
/// FreeBSD program from a Linux one of the same architecture.
const ELFOSABI_FREEBSD: u8 = 9;

/// What an ELF header says: the machine and the OS ABI byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Elf {
    machine: u16,
    osabi: u8,
}

/// Check that `program` is a 64-bit little-endian ELF file (an executable
/// or a shared library) for `platform`'s architecture and system: branded
/// FreeBSD for a FreeBSD platform, and not for a Linux one.
///
/// # Errors
///
/// [`BundleError::NotNativePlatform`] for a platform outside
/// [`NATIVE_PLATFORMS`], and [`BundleError::WrongProgram`] for a program
/// that is not an ELF file for it.
pub fn check_program(platform: Platform, program: &[u8]) -> Result<(), BundleError> {
    let (machine, freebsd) = match platform {
        Platform::LinuxX86_64 => (EM_X86_64, false),
        Platform::LinuxAarch64 => (EM_AARCH64, false),
        Platform::FreebsdX86_64 => (EM_X86_64, true),
        Platform::FreebsdAarch64 => (EM_AARCH64, true),
        other => return Err(BundleError::NotNativePlatform(other.as_str().to_string())),
    };
    match elf_header(program) {
        Ok(found) if found.machine == machine && (found.osabi == ELFOSABI_FREEBSD) == freebsd => {
            Ok(())
        }
        found => Err(BundleError::WrongProgram {
            platform: platform.as_str().to_string(),
            found: describe(found, freebsd),
        }),
    }
}

/// The ELF header's machine and OS ABI, or what the file is instead.
fn elf_header(program: &[u8]) -> Result<Elf, &'static str> {
    let Some(header) = program.get(..20) else {
        return Err("not an ELF file");
    };
    if header[..4] != *b"\x7fELF" {
        return Err("not an ELF file");
    }
    if header[4] != 2 {
        return Err("a 32-bit ELF file");
    }
    if header[5] != 1 {
        return Err("a big-endian ELF file");
    }
    Ok(Elf {
        machine: u16::from_le_bytes([header[18], header[19]]),
        osabi: header[7],
    })
}

/// What `found` is, for a platform that wants FreeBSD's brand or not.
fn describe(found: Result<Elf, &str>, want_freebsd: bool) -> String {
    let elf = match found {
        Err(what) => return what.to_string(),
        Ok(elf) => elf,
    };
    let arch = match elf.machine {
        EM_X86_64 => "x86_64".to_string(),
        EM_AARCH64 => "aarch64".to_string(),
        machine => format!("machine {machine}"),
    };
    match (elf.osabi == ELFOSABI_FREEBSD, want_freebsd) {
        (true, _) => format!("a FreeBSD ELF file for {arch}"),
        (false, true) => format!(
            "an ELF file for {arch} not branded for FreeBSD (EI_OSABI {}): a Linux program?",
            elf.osabi
        ),
        (false, false) => format!("an ELF file for {arch}"),
    }
}

#[cfg(test)]
#[path = "program_tests.rs"]
mod program_tests;
