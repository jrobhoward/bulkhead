//! The fence around one app, per system: set up by the launcher before
//! the child starts, joined by the child before exec, taken down after.

use std::io::Write;
use std::process::{Command, Stdio};

/// Run `cmd args`, feeding `stdin`; its stdout, or why it failed.
pub fn run(cmd: &str, args: &[&str], stdin: Option<&str>) -> Result<String, String> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{cmd}: {e}"))?;
    if let Some(text) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(text.as_bytes())
            .map_err(|e| format!("{cmd}: {e}"))?;
    }
    drop(child.stdin.take());
    let out = child
        .wait_with_output()
        .map_err(|e| format!("{cmd}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{cmd} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(target_os = "linux")]
pub use linux::Fence;

#[cfg(target_os = "freebsd")]
pub use freebsd::Fence;

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::path::PathBuf;

    use crate::bpf::{self, Family};
    use crate::launch::Opts;
    use crate::rules::Rules;

    pub struct Fence {
        dir: PathBuf,
        cgroup: OwnedFd,
        progs: Vec<(OwnedFd, u32)>,
        nft: bool,
        procs: CString,
    }

    impl Fence {
        pub fn up(o: &Opts, rules: &Rules) -> Result<Self, String> {
            let dir = PathBuf::from("/sys/fs/cgroup/s40/apps").join(&o.app);
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let cgroup: OwnedFd = std::fs::File::open(&dir).map_err(|e| e.to_string())?.into();
            let procs = CString::new(dir.join("cgroup.procs").to_str().unwrap()).unwrap();
            let connect = rules.connect()?;
            let bind = rules.bind()?;
            let mut f = Self {
                dir,
                cgroup,
                progs: Vec::new(),
                nft: false,
                procs,
            };
            match o.fence.as_str() {
                "none" => {}
                "bpf" => {
                    let set = [
                        (
                            Family::V4,
                            true,
                            bpf::INET4_CONNECT,
                            &connect,
                            "s40_connect4",
                        ),
                        (
                            Family::V6,
                            true,
                            bpf::INET6_CONNECT,
                            &connect,
                            "s40_connect6",
                        ),
                        (
                            Family::V4,
                            true,
                            bpf::UDP4_SENDMSG,
                            &connect,
                            "s40_sendmsg4",
                        ),
                        (
                            Family::V6,
                            true,
                            bpf::UDP6_SENDMSG,
                            &connect,
                            "s40_sendmsg6",
                        ),
                        (Family::V4, false, bpf::INET4_BIND, &bind, "s40_bind4"),
                        (Family::V6, false, bpf::INET6_BIND, &bind, "s40_bind6"),
                    ];
                    for (family, guard, attach, rules, name) in set {
                        let prog = bpf::program(rules, family, guard);
                        let fd = bpf::load(&prog, attach, name)?;
                        bpf::attach(&f.cgroup, &fd, attach)
                            .map_err(|e| format!("attach {name}: {e}"))?;
                        f.progs.push((fd, attach));
                    }
                }
                "nft" => {
                    let text = crate::firewall::nft(&o.app, &connect, &bind);
                    super::run("nft", &["-f", "-"], Some(&text))?;
                    f.nft = true;
                }
                other => return Err(format!("no fence {other:?} on Linux")),
            }
            Ok(f)
        }

        /// What the child does before exec: join the app's cgroup.
        pub fn child_hook(&self) -> impl FnMut() -> std::io::Result<()> + Send + Sync + 'static {
            let procs = self.procs.clone();
            move || {
                // SAFETY: open, write and close on a valid C string and
                // buffer, async-signal-safe.
                unsafe {
                    let fd = libc::open(procs.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                    if fd < 0 || libc::write(fd, b"0".as_ptr().cast(), 1) != 1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::close(fd);
                }
                Ok(())
            }
        }

        pub fn down(self) {
            for (fd, attach) in &self.progs {
                if let Err(e) = bpf::detach(&self.cgroup, fd, *attach) {
                    eprintln!("s40: detach: {e}");
                }
            }
            if self.nft
                && let Err(e) = super::run("nft", &["delete", "table", "inet", "s40"], None)
            {
                eprintln!("s40: {e}");
            }
            let _ = self.cgroup.as_raw_fd();
            drop(self.cgroup);
            if let Err(e) = std::fs::remove_dir(&self.dir) {
                eprintln!("s40: {}: {e}", self.dir.display());
            }
        }
    }
}

#[cfg(target_os = "freebsd")]
mod freebsd {
    use crate::launch::Opts;
    use crate::rules::Rules;

    const FIRST_IPFW: u32 = 10000;

    pub struct Fence {
        name: String,
        jid: i32,
        anchor: Option<String>,
        ipfw: Vec<String>,
    }

    impl Fence {
        pub fn up(o: &Opts, rules: &Rules) -> Result<Self, String> {
            let name = format!("s40-{}", o.app);
            let ip = match o.fence.as_str() {
                "pf" => vec![format!("ip4.addr={}", o.self_ip), "ip6=disable".into()],
                _ => vec!["ip4=inherit".into(), "ip6=inherit".into()],
            };
            let mut args = vec![
                "-c".to_string(),
                format!("name={name}"),
                "path=/".into(),
                "host.hostname=s40".into(),
                "persist".into(),
            ];
            args.extend(ip);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            super::run("jail", &args, None)?;
            let jid: i32 = super::run("jls", &["-j", &name, "jid"], None)?
                .trim()
                .parse()
                .map_err(|e| format!("jid: {e}"))?;
            let connect = rules.connect()?;
            let bind = rules.bind()?;
            let mut f = Self {
                name,
                jid,
                anchor: None,
                ipfw: Vec::new(),
            };
            match o.fence.as_str() {
                "none" => {}
                "pf" => {
                    let anchor = format!("s40/{}", o.app);
                    let text = crate::firewall::pf(&o.self_ip.to_string(), &connect, &bind);
                    super::run("pfctl", &["-a", &anchor, "-f", "-"], Some(&text))?;
                    f.anchor = Some(anchor);
                }
                "ipfw" => {
                    for line in crate::firewall::ipfw(jid as u32, FIRST_IPFW, &connect, &bind) {
                        let mut a = vec!["-q", "add"];
                        a.extend(line.split_whitespace());
                        super::run("ipfw", &a, None)?;
                        f.ipfw
                            .push(line.split_whitespace().next().unwrap().to_string());
                    }
                }
                other => return Err(format!("no fence {other:?} on FreeBSD")),
            }
            Ok(f)
        }

        /// What the child does before exec: enter the jail.
        pub fn child_hook(&self) -> impl FnMut() -> std::io::Result<()> + Send + Sync + 'static {
            let jid = self.jid;
            move || {
                // SAFETY: plain system call.
                if unsafe { libc::jail_attach(jid) } < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            }
        }

        pub fn down(self) {
            if let Some(a) = &self.anchor
                && let Err(e) = super::run("pfctl", &["-a", a, "-F", "rules"], None)
            {
                eprintln!("s40: {e}");
            }
            for n in &self.ipfw {
                let _ = super::run("ipfw", &["-q", "delete", n], None);
            }
            if let Err(e) = super::run("jail", &["-r", &self.name], None) {
                eprintln!("s40: {e}");
            }
        }
    }
}
