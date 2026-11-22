//! cgroup BPF programs generated from the rules: one
//! `BPF_PROG_TYPE_CGROUP_SOCK_ADDR` program per attach type, plain
//! instructions with no helper calls, so no compiler, no BPF crate and no
//! GPL-only helper. A program returns 1 to allow the call and 0 to refuse
//! it, which the kernel turns into `EPERM`.
//!
//! The context is `struct bpf_sock_addr`: `user_ip4` at 4, `user_ip6[4]`
//! at 8, `user_port` at 24, each 32 bits in network byte order. A 32-bit
//! load gives the bytes in host order, so the generator compares against
//! `u32::from_ne_bytes` of the rule's bytes, and turns the port into host
//! order with `be16` (little-endian hosts: the Pi and x86-64).
//!
//! [`run`] interprets the same instructions, so the generated programs are
//! tested against [`crate::rules::allows`] without a kernel.

use crate::rules::{Net, Rule};

/// One instruction, as the kernel lays it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insn {
    pub code: u8,
    pub dst: u8,
    pub src: u8,
    pub off: i16,
    pub imm: i32,
}

const LDX_W: u8 = 0x61; // BPF_LDX | BPF_MEM | BPF_W
const MOV64_X: u8 = 0xbf;
const MOV64_K: u8 = 0xb7;
const MOV32_X: u8 = 0xbc;
const AND32_K: u8 = 0x54;
const BE16: u8 = 0xdc; // BPF_ALU | BPF_END | BPF_TO_BE, imm 16
const JA: u8 = 0x05;
const JEQ32_K: u8 = 0x16;
const JNE32_K: u8 = 0x56;
const JGT32_K: u8 = 0x26;
const JLT32_K: u8 = 0xa6;
const EXIT: u8 = 0x95;

const OFF_IP4: i16 = 4;
const OFF_IP6: i16 = 8;
const OFF_PORT: i16 = 24;

/// Which address family a program sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    V4,
    V6,
}

type Label = usize;

/// Instructions with jumps to labels, resolved by [`Asm::finish`].
#[derive(Default)]
struct Asm {
    insns: Vec<Insn>,
    fixups: Vec<(usize, Label)>,
    labels: Vec<Option<usize>>,
}

impl Asm {
    fn label(&mut self) -> Label {
        self.labels.push(None);
        self.labels.len() - 1
    }

    fn here(&mut self, l: Label) {
        self.labels[l] = Some(self.insns.len());
    }

    fn op(&mut self, code: u8, dst: u8, src: u8, off: i16, imm: i32) {
        self.insns.push(Insn {
            code,
            dst,
            src,
            off,
            imm,
        });
    }

    fn jump(&mut self, code: u8, dst: u8, imm: i32, to: Label) {
        self.fixups.push((self.insns.len(), to));
        self.op(code, dst, 0, 0, imm);
    }

    fn finish(mut self) -> Vec<Insn> {
        for (at, l) in self.fixups {
            let target = self.labels[l].expect("label placed");
            self.insns[at].off =
                i16::try_from(target as isize - at as isize - 1).expect("short jump");
        }
        self.insns
    }

    /// Unless `(reg & mask) == want`, jump to `miss`; nothing for an empty
    /// mask.
    fn masked(&mut self, reg: u8, mask: u32, want: u32, miss: Label) {
        match mask {
            0 => {}
            u32::MAX => self.jump(JNE32_K, reg, want as i32, miss),
            _ => {
                self.op(MOV32_X, SCRATCH, reg, 0, 0);
                self.op(AND32_K, SCRATCH, 0, 0, mask as i32);
                self.jump(JNE32_K, SCRATCH, want as i32, miss);
            }
        }
    }
}

/// Registers: r1 the context, r2 the IPv4 address or r2..r5 the IPv6
/// words, r6 the port in host order, r7 scratch.
const CTX: u8 = 1;
const PORT: u8 = 6;
const SCRATCH: u8 = 7;

/// A block's words as the program compares them: `(mask, want)` per 32
/// bits, in the byte order a load gives.
fn words(net: Net) -> Vec<(u32, u32)> {
    let mask = net.mask_bytes();
    net.bytes()
        .chunks(4)
        .zip(mask.chunks(4))
        .map(|(b, m)| {
            let m = u32::from_ne_bytes(m.try_into().unwrap());
            (m, u32::from_ne_bytes(b.try_into().unwrap()) & m)
        })
        .collect()
}

/// IPv4-mapped IPv6 addresses.
const MAPPED: Net = Net::V6([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0, 0, 0, 0], 96);

/// `::ffff:0:0/96`'s third word, as a load gives it.
fn mapped_word() -> u32 {
    u32::from_ne_bytes([0, 0, 0xff, 0xff])
}

/// The program for `family` allowing what `rules` allow; `guard` for
/// connect and send programs (the node's address guard), not bind. An
/// IPv6 program checks IPv4 rules against IPv4-mapped addresses.
pub fn program(rules: &[Rule], family: Family, guard: bool) -> Vec<Insn> {
    let mut a = Asm::default();
    let allow = a.label();
    let deny = a.label();
    a.op(MOV64_X, CTX, 1, 0, 0);
    match family {
        Family::V4 => a.op(LDX_W, 2, CTX, OFF_IP4, 0),
        Family::V6 => {
            for i in 0..4u8 {
                a.op(LDX_W, 2 + i, CTX, OFF_IP6 + 4 * i16::from(i), 0);
            }
        }
    }
    a.op(LDX_W, PORT, CTX, OFF_PORT, 0);
    a.op(BE16, PORT, 0, 0, 16);
    for r in rules {
        let next = a.label();
        // The address registers holding this rule's family: IPv4 in r2,
        // or (an IPv6 program) the mapped address's last word r5, after
        // checking the first three.
        let regs: Vec<u8> = match (family, r.net.is_v4()) {
            (Family::V4, true) => vec![2],
            (Family::V4, false) => continue,
            (Family::V6, false) => vec![2, 3, 4, 5],
            (Family::V6, true) => {
                a.jump(JNE32_K, 2, 0, next);
                a.jump(JNE32_K, 3, 0, next);
                a.jump(JNE32_K, 4, mapped_word() as i32, next);
                vec![5]
            }
        };
        for (reg, (mask, want)) in regs.iter().zip(words(r.net)) {
            a.masked(*reg, mask, want, next);
        }
        // IPv4-mapped addresses travel as IPv4 and answer to the IPv4
        // rules only, guard or not.
        let mut skipped = if guard { r.guarded() } else { Vec::new() };
        if !r.net.is_v4() && r.net.contains(MAPPED) && !MAPPED.contains(r.net) {
            skipped.push(MAPPED);
        }
        {
            for special in skipped {
                // Inside `special`: this rule does not apply.
                let outside = a.label();
                for (reg, (mask, want)) in regs.iter().zip(words(special)) {
                    a.masked(*reg, mask, want, outside);
                }
                a.jump(JA, 0, 0, next);
                a.here(outside);
            }
        }
        match r.ports {
            None => {}
            Some((lo, hi)) if lo == hi => a.jump(JNE32_K, PORT, i32::from(lo), next),
            Some((lo, hi)) => {
                a.jump(JLT32_K, PORT, i32::from(lo), next);
                a.jump(JGT32_K, PORT, i32::from(hi), next);
            }
        }
        a.jump(JA, 0, 0, allow);
        a.here(next);
    }
    a.jump(JA, 0, 0, deny);
    // The verifier refuses unreachable instructions: no allow block when
    // no rule reaches it (no rules of this family).
    if a.fixups.iter().any(|(_, l)| *l == allow) {
        a.here(allow);
        a.op(MOV64_K, 0, 0, 0, 1);
        a.op(EXIT, 0, 0, 0, 0);
    }
    a.here(deny);
    a.op(MOV64_K, 0, 0, 0, 0);
    a.op(EXIT, 0, 0, 0, 0);
    a.finish()
}

/// A `bpf_sock_addr` context for `addr:port`, as the kernel fills it.
pub fn context(addr: std::net::IpAddr, port: u16) -> [u8; 64] {
    let mut ctx = [0u8; 64];
    match addr {
        std::net::IpAddr::V4(a) => {
            ctx[0..4].copy_from_slice(&2u32.to_ne_bytes());
            ctx[4..8].copy_from_slice(&a.octets());
        }
        std::net::IpAddr::V6(a) => {
            ctx[0..4].copy_from_slice(&10u32.to_ne_bytes());
            ctx[8..24].copy_from_slice(&a.octets());
        }
    }
    ctx[24..26].copy_from_slice(&port.to_be_bytes());
    ctx
}

/// Run `prog` on `ctx` as the kernel would, for the instructions
/// [`program`] emits; its return value.
pub fn run(prog: &[Insn], ctx: &[u8]) -> u64 {
    const BASE: u64 = 0x1000_0000;
    let mut r = [0u64; 11];
    r[1] = BASE;
    let mut pc = 0usize;
    loop {
        let i = prog[pc];
        let (d, s) = (usize::from(i.dst), usize::from(i.src));
        let imm32 = i.imm as u32;
        let mut jump = |cond: bool| {
            if cond {
                pc = (pc as isize + isize::from(i.off)) as usize
            }
        };
        match i.code {
            LDX_W => {
                let at = (r[s] - BASE) as usize + i.off as usize;
                r[d] = u64::from(u32::from_ne_bytes(ctx[at..at + 4].try_into().unwrap()));
            }
            MOV64_X => r[d] = r[s],
            MOV64_K => r[d] = i.imm as i64 as u64,
            MOV32_X => r[d] = u64::from(r[s] as u32),
            AND32_K => r[d] = u64::from(r[d] as u32 & imm32),
            BE16 => r[d] = u64::from((r[d] as u16).to_be()),
            JA => jump(true),
            JEQ32_K => jump(r[d] as u32 == imm32),
            JNE32_K => jump(r[d] as u32 != imm32),
            JGT32_K => jump(r[d] as u32 > imm32),
            JLT32_K => jump((r[d] as u32) < imm32),
            EXIT => return r[0],
            other => panic!("opcode {other:#x} not interpreted"),
        }
        pc += 1;
    }
}

#[cfg(target_os = "linux")]
pub use kernel::*;

#[cfg(target_os = "linux")]
mod kernel {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use super::Insn;

    pub const PROG_TYPE_CGROUP_SOCK_ADDR: u32 = 18;
    pub const INET4_BIND: u32 = 8;
    pub const INET6_BIND: u32 = 9;
    pub const INET4_CONNECT: u32 = 10;
    pub const INET6_CONNECT: u32 = 11;
    pub const UDP4_SENDMSG: u32 = 14;
    pub const UDP6_SENDMSG: u32 = 15;
    const BPF_PROG_LOAD: libc::c_long = 5;
    const BPF_PROG_ATTACH: libc::c_long = 8;
    const BPF_PROG_DETACH: libc::c_long = 9;
    const F_ALLOW_MULTI: u32 = 2;

    #[repr(C)]
    #[derive(Default)]
    struct ProgLoad {
        prog_type: u32,
        insn_cnt: u32,
        insns: u64,
        license: u64,
        log_level: u32,
        log_size: u32,
        log_buf: u64,
        kern_version: u32,
        prog_flags: u32,
        prog_name: [u8; 16],
        prog_ifindex: u32,
        expected_attach_type: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct Attach {
        target_fd: u32,
        attach_bpf_fd: u32,
        attach_type: u32,
        attach_flags: u32,
    }

    fn bpf<T>(cmd: libc::c_long, attr: &mut T) -> io::Result<libc::c_long> {
        // SAFETY: `attr` is a valid, initialized `bpf_attr` prefix of its
        // size for the call's duration.
        let r =
            unsafe { libc::syscall(libc::SYS_bpf, cmd, std::ptr::from_mut(attr), size_of::<T>()) };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(r)
        }
    }

    /// Load `prog` for `attach_type`; the verifier's log on refusal.
    pub fn load(prog: &[Insn], attach_type: u32, name: &str) -> Result<OwnedFd, String> {
        let raw: Vec<u64> = prog
            .iter()
            .map(|i| {
                u64::from(i.code)
                    | u64::from(i.dst & 0xf | (i.src & 0xf) << 4) << 8
                    | u64::from(i.off as u16) << 16
                    | u64::from(i.imm as u32) << 32
            })
            .collect();
        // No helper is called, so the licence string carries no meaning
        // for the verifier.
        let license = c"MIT OR Apache-2.0";
        let mut log = vec![0u8; 1 << 16];
        let mut attr = ProgLoad {
            prog_type: PROG_TYPE_CGROUP_SOCK_ADDR,
            insn_cnt: raw.len() as u32,
            insns: raw.as_ptr() as u64,
            license: license.as_ptr() as u64,
            log_level: 1,
            log_size: log.len() as u32,
            log_buf: log.as_mut_ptr() as u64,
            expected_attach_type: attach_type,
            ..ProgLoad::default()
        };
        for (d, s) in attr.prog_name.iter_mut().zip(name.bytes().take(15)) {
            *d = s;
        }
        match bpf(BPF_PROG_LOAD, &mut attr) {
            // SAFETY: the kernel returned a new descriptor we own.
            Ok(fd) => Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) }),
            Err(e) => {
                let end = log.iter().position(|b| *b == 0).unwrap_or(log.len());
                Err(format!("{e}: {}", String::from_utf8_lossy(&log[..end])))
            }
        }
    }

    pub fn attach(cgroup: &OwnedFd, prog: &OwnedFd, attach_type: u32) -> io::Result<()> {
        let mut attr = Attach {
            target_fd: cgroup.as_raw_fd() as u32,
            attach_bpf_fd: prog.as_raw_fd() as u32,
            attach_type,
            attach_flags: F_ALLOW_MULTI,
        };
        bpf(BPF_PROG_ATTACH, &mut attr).map(|_| ())
    }

    pub fn detach(cgroup: &OwnedFd, prog: &OwnedFd, attach_type: u32) -> io::Result<()> {
        let mut attr = Attach {
            target_fd: cgroup.as_raw_fd() as u32,
            attach_bpf_fd: prog.as_raw_fd() as u32,
            attach_type,
            attach_flags: 0,
        };
        bpf(BPF_PROG_DETACH, &mut attr).map(|_| ())
    }
}

#[cfg(test)]
#[path = "bpf_tests.rs"]
mod bpf_tests;
