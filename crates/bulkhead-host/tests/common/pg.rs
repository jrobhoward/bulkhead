//! A scratch Postgres for tests: `initdb` and `pg_ctl` in a temporary
//! directory, as the current user, on a Unix socket only. Shared by
//! `bulkhead-host`'s and `bulkhead`'s tests (the latter include this file by
//! path). `BULKHEAD_PG_BIN` names the directory holding `initdb` and
//! `pg_ctl`; otherwise `/usr/lib/postgresql/*/bin` and `PATH` are searched.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// The socket's port number (no TCP listener).
pub const PORT: u16 = 5433;

/// The directory holding `initdb` and `pg_ctl`, if any.
pub fn pg_bin() -> Option<PathBuf> {
    let has = |d: &Path| d.join("initdb").is_file() && d.join("pg_ctl").is_file();
    if let Some(dir) = std::env::var_os("BULKHEAD_PG_BIN") {
        return Some(PathBuf::from(dir)).filter(|d| has(d));
    }
    let mut versions: Vec<PathBuf> = std::fs::read_dir("/usr/lib/postgresql")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("bin"))
        .filter(|d| has(d))
        .collect();
    versions.sort();
    versions.pop().or_else(|| {
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .find(|d| has(d))
    })
}

/// How a [`Scratch`] server is set up.
#[derive(Debug, Default, Clone, Copy)]
pub struct Options<'a> {
    /// Listen on loopback TCP too, on a free port.
    pub tcp: bool,
    /// The user `bulkhead`'s password, asked for over TCP (SCRAM); the
    /// socket stays trusted.
    pub password: Option<&'a str>,
    /// TLS over TCP: the certificate and key, PEM.
    pub tls: Option<(&'a str, &'a str)>,
}

/// A Postgres server of its own, in a temporary directory, listening on a
/// Unix socket (and on loopback TCP if asked); stopped when dropped.
pub struct Scratch {
    bin: PathBuf,
    dir: tempfile::TempDir,
    /// Its port: [`PORT`] on the socket alone, a free one with TCP.
    port: u16,
}

impl Scratch {
    /// Start one, or `None` (with a note) when Postgres is not installed.
    pub fn start() -> Option<Self> {
        Self::start_with(&Options::default())
    }

    /// Start one listening on 127.0.0.1 too, on a free port
    /// ([`Scratch::port`]); `None` without Postgres.
    pub fn start_tcp() -> Option<Self> {
        Self::start_with(&Options {
            tcp: true,
            ..Options::default()
        })
    }

    /// Start one as `options` say; `None` without Postgres.
    pub fn start_with(options: &Options) -> Option<Self> {
        let tcp = options.tcp.then(|| {
            std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port()
        });
        let Some(bin) = pg_bin() else {
            eprintln!(
                "skipped: no Postgres (initdb, pg_ctl) found; install it or set BULKHEAD_PG_BIN"
            );
            return None;
        };
        // Short: the socket path must fit in sun_path.
        let dir = tempfile::Builder::new()
            .prefix("pg")
            .tempdir_in("/tmp")
            .unwrap();
        let data = dir.path().join("data");
        let mut initdb = Command::new(bin.join("initdb"));
        initdb.args(["-A", "trust", "-U", "bulkhead", "-E", "UTF8", "--no-sync"]);
        if let Some(password) = options.password {
            // After `-A`, which sets both: the socket stays trusted.
            let file = dir.path().join("pwfile");
            std::fs::write(&file, password).unwrap();
            initdb
                .arg("--auth-host=scram-sha-256")
                .arg(format!("--pwfile={}", file.display()));
        }
        let ok = initdb.arg("-D").arg(&data).output().unwrap();
        assert!(
            ok.status.success(),
            "initdb: {}",
            String::from_utf8_lossy(&ok.stderr)
        );
        let (listen, port) = match tcp {
            Some(port) => ("127.0.0.1", port),
            None => ("", PORT),
        };
        let mut opts = format!(
            "-k {} -c listen_addresses='{listen}' -c fsync=off -p {port}",
            dir.path().display()
        );
        if let Some((cert, key)) = options.tls {
            let (c, k) = (dir.path().join("server.crt"), dir.path().join("server.key"));
            std::fs::write(&c, cert).unwrap();
            std::fs::write(&k, key).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&k, std::fs::Permissions::from_mode(0o600)).unwrap();
            opts.push_str(&format!(
                " -c ssl=on -c ssl_cert_file={} -c ssl_key_file={}",
                c.display(),
                k.display()
            ));
        }
        let ok = Command::new(bin.join("pg_ctl"))
            .arg("-D")
            .arg(&data)
            .args(["-o", &opts, "-l"])
            .arg(dir.path().join("log"))
            .args(["-w", "start"])
            .output()
            .unwrap();
        assert!(
            ok.status.success(),
            "pg_ctl start: {}",
            String::from_utf8_lossy(&ok.stderr)
        );
        Some(Self { bin, dir, port })
    }

    /// Its port, on the socket and on TCP if it listens there.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The directory holding its Unix socket, as `host` for a client.
    pub fn socket_dir(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = Command::new(self.bin.join("pg_ctl"))
            .arg("-D")
            .arg(self.dir.path().join("data"))
            .args(["-m", "immediate", "stop"])
            .output();
    }
}
