//! One-shot PAM activation. The named socket never transports a password and
//! the helper pins the session bus itself instead of trusting its peer.
mod handoff;
use clap::Parser;
use std::{
    io::{self, Read, Write},
    os::{
        fd::AsFd,
        unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    },
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

#[derive(Parser)]
#[command(
    version,
    about = "Start a protected Clavis child with a private inherited login password"
)]
struct Args {
    /// Private activation socket; default /run/user/UID/clavis-login.sock.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Private readiness acknowledgment for the PAM parent's pipe.
    #[arg(long, hide = true)]
    pam_ready: bool,
    /// Pin one isolated fixture bus instead of the canonical user bus.
    /// Only disposable tests pass this; release PAM never does.
    #[arg(long, value_name = "ADDRESS")]
    test_bus: Option<String>,
}

struct SocketGuard {
    path: PathBuf,
    identity: (u64, u64),
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        if self
            .path
            .symlink_metadata()
            .is_ok_and(|m| (m.dev(), m.ino()) == self.identity)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            if let Some(pid) = rustix::process::Pid::from_raw(self.0.id() as i32) {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
            }
            let deadline = Instant::now() + Duration::from_millis(800);
            while Instant::now() < deadline && self.0.try_wait().ok().flatten().is_none() {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn poll_readable(fd: impl AsFd, timeout: Duration) -> io::Result<bool> {
    let timespec = rustix::event::Timespec {
        tv_sec: timeout.as_secs() as i64,
        tv_nsec: timeout.subsec_nanos() as i64,
    };
    Ok(rustix::event::poll(
        &mut [rustix::event::PollFd::from_borrowed_fd(
            fd.as_fd(),
            rustix::event::PollFlags::IN,
        )],
        Some(&timespec),
    )? != 0)
}

/// The single bus the protected child may serve. Release logins pin the
/// canonical user bus; only an explicit `--test-bus` fixture pins another
/// address. A same-UID activation peer can never choose or replace it.
fn pinned_bus(args: &Args, uid: u32) -> (String, PathBuf, PathBuf) {
    if let Some(value) = &args.test_bus {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}")));
        let address = if value.starts_with("unix:") {
            value.clone()
        } else {
            format!("unix:path={value}")
        };
        // The full address is validated against the claimed peer address;
        // this only extracts the pinned socket path for comparison.
        let path = PathBuf::from(
            value
                .strip_prefix("unix:path=")
                .unwrap_or(value)
                .split(',')
                .next()
                .unwrap_or_default(),
        );
        return (address, path, runtime);
    }
    let runtime = PathBuf::from(format!("/run/user/{uid}"));
    let path = runtime.join("bus");
    (format!("unix:path={}", path.display()), path, runtime)
}

/// Wait for the pinned bus within a bounded deadline. The peer-supplied
/// address is never substituted when the canonical bus stays unavailable.
fn wait_for_bus(
    address: &str,
    runtime: &std::path::Path,
    uid: u32,
    deadline: Instant,
) -> io::Result<()> {
    loop {
        match handoff::validate_bus(address, runtime, uid) {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("Pinned login bus did not appear: {error}"),
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error),
        }
    }
}

fn child_environment(
    address: &str,
    runtime: &std::path::Path,
    uid: u32,
) -> io::Result<Vec<(std::ffi::OsString, std::ffi::OsString)>> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| {
            path.is_absolute()
                && path
                    .symlink_metadata()
                    .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink() && m.uid() == uid)
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "PAM must provide the authenticated user's HOME",
            )
        })?;
    let mut result = vec![
        ("HOME".into(), home.as_os_str().to_owned()),
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("LANG".into(), "C.UTF-8".into()),
        ("XDG_RUNTIME_DIR".into(), runtime.as_os_str().to_owned()),
        ("DBUS_SESSION_BUS_ADDRESS".into(), address.into()),
        ("CLAVIS_PROMPT_BACKEND".into(), "clavis".into()),
        ("RIX_SECRET_STORE".into(), "clavis".into()),
        ("XDG_CURRENT_DESKTOP".into(), "RIX".into()),
        ("RUST_LOG".into(), "info".into()),
    ];
    for name in [
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
        "CLAVIS_STORAGE_DIR",
        "CLAVIS_PAM_SOCKET",
    ] {
        if let Some(value) = std::env::var_os(name) {
            if !PathBuf::from(&value).is_absolute() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Login environment paths must be absolute",
                ));
            }
            result.push((name.into(), value));
        }
    }
    // Only a private owned Unix bus may override the system bus (fixture use).
    if let Ok(system_bus) = std::env::var("DBUS_SYSTEM_BUS_ADDRESS") {
        handoff::validate_bus(&system_bus, runtime, uid)?;
        result.push(("DBUS_SYSTEM_BUS_ADDRESS".into(), system_bus.into()));
    }
    Ok(result)
}

fn start_child(
    daemon: &std::path::Path,
    address: &str,
    runtime: &std::path::Path,
    uid: u32,
    secret: &mut Zeroizing<Vec<u8>>,
) -> io::Result<ChildGuard> {
    // A user cannot replace the selected executable or any directory component.
    handoff::protected_executable(daemon)?;
    let mut command = Command::new(daemon);
    command
        .arg("--inherited-login")
        .arg("--login-parent-pid")
        .arg(std::process::id().to_string())
        .env_clear()
        .envs(child_environment(address, runtime, uid)?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = ChildGuard(command.spawn()?);
    let stdout = child.0.stdout.as_mut().unwrap();
    if !poll_readable(&*stdout, Duration::from_secs(10))?
        || handoff::read_line(stdout, 40)? != handoff::READY
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Protected daemon did not acknowledge secure readiness",
        ));
    }
    // The actual child is now NotDumpable, untraced, and owns its private pipe.
    let mut stdin = child.0.stdin.take().unwrap();
    stdin.write_all(secret)?;
    zeroize::Zeroize::zeroize(&mut **secret);
    drop(stdin);
    let stdout = child.0.stdout.as_mut().unwrap();
    if !poll_readable(&*stdout, Duration::from_secs(20))?
        || handoff::read_line(stdout, 40)? != handoff::RUNNING
    {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "Protected daemon could not start its Secret Service",
        ));
    }
    Ok(child)
}

fn run(args: Args) -> io::Result<()> {
    let uid = rustix::process::getuid();
    if uid.as_raw() == 0 || rustix::process::geteuid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Clavis login must run as the authenticated user",
        ));
    }
    rustix::process::setrlimit(
        rustix::process::Resource::Core,
        rustix::process::Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    )?;
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)?;
    handoff::require_untraced()?;
    rustix::thread::set_no_new_privs(true)?;
    let daemon = PathBuf::from(
        option_env!("CLAVIS_DAEMON_PATH").unwrap_or("/usr/libexec/rixde/clavis-daemon"),
    );
    handoff::protected_executable(&daemon)?;
    let _ = rustix::mm::mlockall(
        rustix::mm::MlockAllFlags::CURRENT | rustix::mm::MlockAllFlags::FUTURE,
    );
    if args.pam_ready {
        io::stdout().write_all(b"CLAVIS-LOGIN-READY\n")?;
        io::stdout().flush()?;
    }
    let mut secret = Zeroizing::new(Vec::new());
    io::stdin().lock().take(65_537).read_to_end(&mut secret)?;
    if secret.is_empty() || secret.len() > 65_536 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "A nonempty password of at most 64 KiB is required",
        ));
    }
    let (bus_address, bus_path, runtime) = pinned_bus(&args, uid.as_raw());
    let path = args
        .socket
        .or_else(|| std::env::var_os("CLAVIS_LOGIN_SOCKET").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}/clavis-login.sock", uid.as_raw())));
    handoff::private_directory(
        path.parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid socket path"))?,
        uid.as_raw(),
    )?;
    if let Ok(metadata) = path.symlink_metadata() {
        if !metadata.file_type().is_socket() || metadata.uid() != uid.as_raw() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Unsafe existing activation socket",
            ));
        }
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Login helper is already waiting",
            ));
        }
        std::fs::remove_file(&path)?;
    }
    let listener = std::os::unix::net::UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let metadata = path.symlink_metadata()?;
    let guard = SocketGuard {
        path,
        identity: (metadata.dev(), metadata.ino()),
    };
    if args.pam_ready {
        io::stdout().write_all(b"CLAVIS-LOGIN-WAITING\n")?;
        io::stdout().flush()?;
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        if !poll_readable(
            &listener,
            deadline.saturating_duration_since(Instant::now()),
        )? {
            break;
        }
        let (mut stream, _) = listener.accept()?;
        if rustix::net::sockopt::socket_peercred(&stream)?.uid != uid {
            continue;
        }
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let request = handoff::read_line(&mut stream, 4096).and_then(|bytes| {
            String::from_utf8(bytes).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Activation address is not UTF-8",
                )
            })
        });
        let claimed = match request {
            Ok(value) => value.trim_end_matches('\n').to_owned(),
            Err(_) => {
                let _ = stream.write_all(b"CLAVIS-ERROR invalid-address\n");
                continue;
            }
        };
        // The peer may only confirm the pinned bus. A different bus, even one
        // inside this user's runtime, is rejected without consuming the login.
        if !handoff::validate_bus(&claimed, &runtime, uid.as_raw())
            .is_ok_and(|path| path == bus_path)
        {
            let _ = stream.write_all(b"CLAVIS-ERROR invalid-address\n");
            continue;
        }
        let bus_deadline = deadline.min(Instant::now() + Duration::from_secs(30));
        if let Err(error) = wait_for_bus(&bus_address, &runtime, uid.as_raw(), bus_deadline) {
            let _ = stream.write_all(b"CLAVIS-ERROR bus-unavailable\n");
            return Err(error);
        }
        let mut child =
            match start_child(&daemon, &bus_address, &runtime, uid.as_raw(), &mut secret) {
                Ok(child) => child,
                Err(error) => {
                    let _ = stream.write_all(b"CLAVIS-ERROR child-start\n");
                    return Err(error);
                }
            };
        // Activation is consumed, so later daemon starts cannot reuse this login.
        drop(guard);
        writeln!(stream, "CLAVIS-SPAWNED {}", child.0.id())?;
        stream.set_read_timeout(None)?;
        let mut byte = [0u8; 1];
        loop {
            if child.0.try_wait()?.is_some() {
                return Ok(());
            }
            if poll_readable(&stream, Duration::from_millis(100))? {
                // EOF or any lifecycle message ends only our own child.
                let _ = stream.read(&mut byte)?;
                return Ok(());
            }
        }
    }
    Ok(())
}

fn main() -> io::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    run(Args::parse())
}
