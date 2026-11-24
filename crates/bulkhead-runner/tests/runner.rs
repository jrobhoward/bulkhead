//! `bulkhead-runner` end to end, without a node: the test plays the node
//! over the runner's channel, polling where the node would sleep.

// The runner runs on Linux and FreeBSD; plugins load on Linux only.
#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::io::{BufRead, BufReader, Read};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

use bulkhead_native::channel::{self, Channel};
use bulkhead_native::protocol::{
    self, Answer, Envelope, ExitReason, FromApp, Level, Pid, Request, ToApp, VERSION,
};

const ME: Pid = Pid {
    node: 1,
    id: 42,
    incarnation: 1,
};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The fixture executable and plugin, built on first use into their own
/// target directory (the `cargo test` running this holds the main one).
fn fixtures() -> &'static (PathBuf, PathBuf) {
    static BUILT: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    BUILT.get_or_init(|| {
        let target = root().join("target/fixtures/native");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .current_dir(root())
            .args([
                "build",
                "-p",
                "bulkhead-fixture-native",
                "-p",
                "bulkhead-fixture-plugin",
                "--target-dir",
            ])
            .arg(&target)
            .status()
            .expect("run cargo");
        assert!(status.success(), "building native fixtures failed");
        (
            target.join("debug/bulkhead-fixture-native"),
            target.join("debug/libbulkhead_fixture_plugin.so"),
        )
    })
}

/// The runner's frames to the node, read as they come; the end once the
/// runner has exited (its bell's end of file) and the ring is empty.
/// One frame from the app, CBOR or hot (ROADMAP T7); `None` at the end.
/// The harness takes no keyvalue requests.
fn read_from_app(from: &mut impl Read) -> Option<FromApp> {
    let mut prefix = [0u8; 4];
    if from.read_exact(&mut prefix).is_err() {
        return None;
    }
    let mut body = vec![0; protocol::frame_len(prefix).unwrap()];
    from.read_exact(&mut body).unwrap();
    if bulkhead_native::hot::is_hot(&body) {
        match bulkhead_native::hot::parse_from_app(&body).unwrap() {
            bulkhead_native::hot::FromAppHot::Frame(frame) => Some(frame),
            bulkhead_native::hot::FromAppHot::Kv(kv) => panic!("unexpected {kv:?}"),
        }
    } else {
        Some(protocol::decode(&body).unwrap())
    }
}

struct FromRunner {
    channel: Arc<Channel>,
    bell: OwnedFd,
    door: OwnedFd,
}

impl Read for FromRunner {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let ring = self.channel.to_node();
        loop {
            let n = ring.read(buf)?;
            if n > 0 {
                channel::wake_writer(ring.writer_waiting(), self.door.as_fd());
                return Ok(n);
            }
            match rustix::io::read(&self.bell, &mut [0u8; 64]) {
                Ok(0) if !ring.is_readable() => return Ok(0),
                _ => std::thread::sleep(std::time::Duration::from_micros(50)),
            }
        }
    }
}

/// One runner, played as its node.
struct Port {
    child: Child,
    channel: Arc<Channel>,
    door: OwnedFd,
    /// The node's end of the descriptor socket.
    fds: OwnedFd,
    from: FromRunner,
    logs: Arc<Mutex<Vec<(Level, String)>>>,
    stderr: Arc<Mutex<Vec<String>>>,
    /// Copies the runner's stderr into `stderr`; done once the runner has
    /// exited and the pipe is drained.
    stderr_copy: std::thread::JoinHandle<()>,
}

impl Port {
    fn spawn(program: &str, path: &PathBuf) -> Self {
        Self::spawn_with(program, path, &[])
    }

    fn spawn_with(program: &str, path: &PathBuf, extra: &[&str]) -> Self {
        let (channel, memfd) = Channel::create().unwrap();
        let (bell, bell_app) = channel::bell().unwrap();
        rustix::fs::fcntl_setfl(&bell, rustix::fs::OFlags::NONBLOCK).unwrap();
        let (door_app, door) = channel::door().unwrap();
        let (fds, fds_app) = channel::descriptors().unwrap();
        let (log, log_app) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bulkhead-runner"));
        cmd.arg(program)
            .arg(path)
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_app.try_clone().unwrap()))
            .stderr(Stdio::from(log_app));
        // SAFETY: the closure makes system calls only.
        unsafe {
            cmd.pre_exec(move || {
                channel::install(
                    memfd.as_fd(),
                    bell_app.as_fd(),
                    door_app.as_fd(),
                    fds_app.as_fd(),
                )
            });
        }
        let child = cmd.spawn().unwrap();
        // The child's ends close with the command.
        drop(cmd);
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let lines = std::fs::File::from(log);
        let sink = stderr.clone();
        let stderr_copy = std::thread::spawn(move || {
            for line in BufReader::new(lines).lines().map_while(Result::ok) {
                sink.lock().unwrap().push(line);
            }
        });
        let channel = Arc::new(channel);
        let mut port = Self {
            from: FromRunner {
                channel: channel.clone(),
                bell,
                door: door.try_clone().unwrap(),
            },
            channel,
            door,
            fds,
            child,
            logs: Arc::default(),
            stderr,
            stderr_copy,
        };
        assert_eq!(port.next(), FromApp::Hello { version: VERSION });
        port
    }

    fn send(&mut self, frame: &ToApp) {
        let ring = self.channel.to_app();
        let bytes = protocol::encode(frame);
        let mut sent = 0;
        while sent < bytes.len() {
            sent += ring.write(&bytes[sent..]).unwrap();
            channel::wake_reader(ring.reader_waiting(), self.door.as_fd());
            if sent < bytes.len() {
                std::thread::yield_now();
            }
        }
    }

    /// The next frame that is not a log or a request; requests are
    /// answered as a node would for pid `ME`.
    fn next(&mut self) -> FromApp {
        loop {
            let frame = read_from_app(&mut self.from).unwrap_or_else(|| {
                panic!("runner exited; stderr: {:?}", self.stderr.lock().unwrap())
            });
            match frame {
                FromApp::Log { level, message } => self.logs.lock().unwrap().push((level, message)),
                FromApp::Request { id, request } => {
                    let answer = match request {
                        Request::SelfPid => Answer::Pid(ME),
                        Request::Monitor(_) | Request::SendAfter { .. } => Answer::Ref(7),
                        _ => Answer::Unit,
                    };
                    self.send(&ToApp::Answer { id, answer });
                }
                other => return other,
            }
        }
    }

    fn init(&mut self, args: Vec<u8>) -> FromApp {
        self.send(&ToApp::Init { args });
        self.next()
    }

    fn call(&mut self, msg: Envelope) -> FromApp {
        self.send(&ToApp::Call { call_ref: 1, msg });
        self.next()
    }

    /// A text command's text reply.
    fn command(&mut self, command: &str) -> String {
        match self.call(Envelope::new("text/plain", command.as_bytes().to_vec())) {
            FromApp::Reply(Some(env)) => String::from_utf8(env.body).unwrap(),
            other => panic!("{command}: {other:?}"),
        }
    }

    /// Wait for the runner to exit, and for every line it wrote to stderr
    /// to reach `stderr`.
    fn exit_code(mut self) -> Option<i32> {
        let ring = self.channel.to_app();
        ring.close();
        channel::wake_reader(ring.reader_waiting(), self.door.as_fd());
        let code = self.child.wait().unwrap().code();
        self.stderr_copy.join().unwrap();
        code
    }
}

fn executable() -> Port {
    let mut port = Port::spawn("--executable", &fixtures().0);
    assert_eq!(port.init(Vec::new()), FromApp::Init(Ok(())));
    port
}

// For the plugin's tests, which are Linux's.
#[cfg(target_os = "linux")]
fn json(schema: &str, body: &str) -> Envelope {
    Envelope {
        content_type: "application/json".into(),
        schema: Some(schema.into()),
        body: body.as_bytes().to_vec(),
        trace: None,
    }
}

#[test]
fn executable____calls_requests_and_terminate____served_in_order() {
    let mut port = executable();

    let echo = port.command("echo hello");
    let me = port.command("self");
    port.send(&ToApp::Terminate {
        reason: ExitReason::Shutdown,
    });
    let done = port.next();

    assert_eq!(echo, "hello");
    assert_eq!(me, "1.42.1");
    assert_eq!(done, FromApp::Done);
    assert_eq!(port.exit_code(), Some(0));
}

#[test]
fn executable____descriptors_passed____each_read_by_the_id_its_frame_names() {
    let mut port = executable();
    let holding = |text: &str| {
        let (read, write) = rustix::pipe::pipe().unwrap();
        rustix::io::write(&write, text.as_bytes()).unwrap();
        read
    };
    channel::send_fd(port.fds.as_fd(), 1, holding("first").as_fd()).unwrap();
    channel::send_fd(port.fds.as_fd(), 2, holding("second").as_fd()).unwrap();

    let second = port.command("fd 2");
    let first = port.command("fd 1");
    let never = port.command("fd 3");

    assert_eq!((first.as_str(), second.as_str()), ("first", "second"));
    assert!(never.starts_with("err:"), "{never}");
}

#[test]
fn executable____init_fails____error_and_exit() {
    let mut port = Port::spawn("--executable", &fixtures().0);

    let found = port.init(b"fail".to_vec());

    assert_eq!(found, FromApp::Init(Err("asked to fail".into())));
    assert_eq!(port.exit_code(), Some(1));
}

#[test]
fn executable____noreply_then_cast____reply_requested_later() {
    let mut port = executable();

    let first = port.call(Envelope::new("text/plain", b"noreply".to_vec()));
    port.send(&ToApp::Cast {
        msg: Envelope::new("text/plain", b"reply later".to_vec()),
    });
    let mut requested = None;
    let done = loop {
        match read_from_app(&mut port.from).unwrap() {
            FromApp::Request { id, request } => {
                requested = Some(request);
                port.send(&ToApp::Answer {
                    id,
                    answer: Answer::Unit,
                });
            }
            other => break other,
        }
    };

    assert_eq!(first, FromApp::Reply(None));
    assert_eq!(
        requested,
        Some(Request::Reply {
            call_ref: 1,
            msg: Envelope::new("text/plain", b"later".to_vec())
        })
    );
    assert_eq!(done, FromApp::Done);
}

#[test]
fn executable____panic____failed_with_message_and_exit() {
    let mut port = executable();

    let found = port.call(Envelope::new("text/plain", b"panic".to_vec()));

    assert_eq!(found, FromApp::Failed("panic: asked to".into()));
    assert_eq!(port.exit_code(), Some(101));
}

#[test]
fn executable____prints_and_logs____stderr_and_log_frames_not_frames_corrupted() {
    let mut port = executable();

    let printed = port.command("print to stdout");
    let logged = port.command("log careful");
    let after = port.command("echo still framed");
    let logs = port.logs.lock().unwrap().clone();
    port.send(&ToApp::Terminate {
        reason: ExitReason::Normal,
    });
    port.next();
    let stderr = port.stderr.clone();
    port.exit_code();

    assert_eq!((printed.as_str(), logged.as_str()), ("ok", "ok"));
    assert_eq!(after, "still framed");
    assert_eq!(logs, vec![(Level::Warn, "careful".to_string())]);
    assert!(stderr.lock().unwrap().iter().any(|l| l == "to stdout"));
}

#[test]
fn executable____sandboxed____files_exec_sockets_and_signals_refused() {
    let mut port = executable();
    let own = fixtures().0.display().to_string();
    let parent = std::process::id();
    let tmp = std::env::temp_dir().join(format!("bulkhead-native-{parent}"));

    let hostname = port.command("read /etc/hostname");
    let itself = port.command(&format!("read {own}"));
    let write = port.command(&format!("write {}", tmp.display()));
    let exec = port.command("exec");
    let connect = port.command("connect 127.0.0.1:9");
    let signal_parent = port.command(&format!("kill {parent}"));
    let signal_group = port.command("kill 0");

    assert!(!tmp.exists());
    // Linux: Landlock (its own directory readable) and the seccomp
    // allow-list; FreeBSD: capability mode, where no path opens at all
    // (ROADMAP FB2).
    if cfg!(target_os = "freebsd") {
        for refused in [
            &hostname,
            &itself,
            &write,
            &exec,
            &connect,
            &signal_parent,
            &signal_group,
        ] {
            assert!(
                refused.contains("Not permitted in capability mode"),
                "{refused}"
            );
        }
    } else {
        assert!(hostname.contains("Permission denied"), "{hostname}");
        assert_eq!(itself, "ok");
        assert!(write.contains("Permission denied"), "{write}");
        assert!(exec.contains("Permission denied"), "{exec}");
        assert!(connect.contains("Operation not permitted"), "{connect}");
        assert!(
            signal_parent.contains("Operation not permitted"),
            "{signal_parent}"
        );
        assert!(
            signal_group.contains("Operation not permitted"),
            "{signal_group}"
        );
    }
}

/// `--unconfined` (ROADMAP AP4): the fences of the test above are off, so
/// files the user may read open, and a socket of the app's own connects.
/// (The node passes the flag only under the helper, which keeps the app's
/// own user, cgroup or jail; here it runs as the test's user.)
#[test]
fn executable____unconfined____files_and_own_sockets_reached() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let mut port = Port::spawn_with("--executable", &fixtures().0, &["--unconfined"]);
    assert_eq!(port.init(Vec::new()), FromApp::Init(Ok(())));

    let hosts = port.command("read /etc/hosts");
    let connect = port.command(&format!("connect {address}"));

    assert_eq!(hosts, "ok");
    assert_eq!(connect, "ok");
}

/// `--own-sockets` (ROADMAP AP2), without the helper's fence: the runner's
/// own fences only. On Linux Landlock lets TCP reach the listed ports
/// alone, seccomp refuses raw sockets, and `listen` without `--listen`
/// fails with an error rather than ending the app; files stay fenced. On
/// FreeBSD capability mode is off, so sockets work; the jail and ipfw
/// rules that hold them are the helper's.
#[test]
fn executable____own_sockets____listed_ports_reached_rest_refused() {
    let allowed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let (a, o) = (allowed.local_addr().unwrap(), other.local_addr().unwrap());
    let ports = format!("{}", a.port());
    let mut port = Port::spawn_with(
        "--executable",
        &fixtures().0,
        &[
            "--own-sockets",
            "--connect-ports",
            &ports,
            "--bind-ports",
            "any",
        ],
    );
    assert_eq!(port.init(Vec::new()), FromApp::Init(Ok(())));

    let reached = port.command(&format!("connect {a}"));
    let refused = port.command(&format!("connect {o}"));
    let udp = port.command(&format!("udp-own {o}"));
    let raw = port.command("raw");
    let listen = port.command("listen 127.0.0.1:0");
    let alive = port.command("self");

    assert_eq!(reached, "ok");
    assert!(!alive.is_empty(), "the app runs on after a refused listen");
    if cfg!(target_os = "linux") {
        assert!(refused.contains("Permission denied"), "{refused}");
        assert_eq!(udp, "ok", "UDP is the helper's fence alone");
        assert!(raw.contains("Operation not permitted"), "{raw}");
        assert!(listen.contains("Operation not permitted"), "{listen}");
        let hostname = port.command("read /etc/hostname");
        assert!(hostname.contains("Permission denied"), "{hostname}");
    } else {
        assert_eq!(refused, "ok");
        assert!(raw.starts_with("err:"), "{raw}");
    }
}

// FreeBSD's runner refuses plugins: they load by path, which capability
// mode forbids (`bulkhead-host`'s `native.rs` checks the refusal).
#[cfg(target_os = "linux")]
#[test]
fn library____json_calls____plugin_answers_with_schema() {
    let mut port = Port::spawn("--library", &fixtures().1);

    let init = port.init(br#"{"k":1}"#.to_vec());
    let echo = port.call(json("echo", r#"{"a":[1,2]}"#));
    let config = port.call(json("config", "{}"));
    let fail = port.call(json("fail", "{}"));
    let text = port.call(Envelope::new("text/plain", b"hi".to_vec()));
    let read = port.call(json("read", r#"{"path":"/etc/hostname"}"#));
    let logs = port.logs.lock().unwrap().clone();

    assert_eq!(init, FromApp::Init(Ok(())));
    assert_eq!(echo, FromApp::Reply(Some(json("echo", r#"{"a":[1,2]}"#))));
    assert_eq!(config, FromApp::Reply(Some(json("config", r#"{"k":1}"#))));
    for (what, found) in [("fail", &fail), ("text", &text), ("read", &read)] {
        let FromApp::Reply(Some(env)) = found else {
            panic!("{what}: {found:?}");
        };
        assert_eq!(env.schema.as_deref(), Some("rustbridge.error"), "{what}");
    }
    let FromApp::Reply(Some(read)) = read else {
        unreachable!()
    };
    assert!(String::from_utf8_lossy(&read.body).contains("Permission denied"));
    assert!(
        logs.iter()
            .any(|(_, m)| m.contains("fixture plugin started")),
        "{logs:?}"
    );
}
