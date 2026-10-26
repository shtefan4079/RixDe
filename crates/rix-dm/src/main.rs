mod lid;
mod logind;
mod resources;
mod running_session;
mod session_environment;
use anyhow::{ensure, Context, Result};
use clap::Parser;
use rix_auth::{PamTransaction, SecretString, UnixUser};
use rix_dm::{Catalog, LoginError, PowerAction, Reply, Request, Session, User};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    fs,
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stopped(_: i32) {
    STOP.store(true, Ordering::Relaxed);
}
fn signals() {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = stopped as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }

        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

#[derive(Parser)]
#[command(about = "Rix's own display-manager daemon and PAM session supervisor")]
struct Args {
    #[arg(long, default_value = "/etc/rix-dm/config.json")]
    config: PathBuf,

    #[arg(long)]
    check: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionConfig {
    id: String,
    name: String,
    command: PathBuf,
    args: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    greeter_user: String,
    compositor: PathBuf,
    greeter: PathBuf,
    #[serde(default = "default_vt")]
    vt: u16,
    #[serde(default = "default_min_uid")]
    min_uid: u32,
    #[serde(default)]
    allowed_users: Vec<String>,
    sessions: Vec<SessionConfig>,
}
fn default_vt() -> u16 {
    1
}
fn default_min_uid() -> u32 {
    1000
}

impl Config {
    fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).context("Cannot read display-manager configuration")?;
        ensure!(bytes.len() <= 64 * 1024, "Configuration is too large");
        let config: Self =
            serde_json::from_slice(&bytes).context("Invalid display-manager configuration")?;
        config.validate()?;
        Ok(config)
    }
    fn validate(&self) -> Result<()> {
        ensure!((1..=63).contains(&self.vt), "VT must be 1..63");
        ensure!(self.min_uid > 0, "Root login is disabled");
        ensure!(
            valid_name(&self.greeter_user),
            "Invalid greeter account name"
        );
        ensure!(
            !self.sessions.is_empty() && self.sessions.len() <= 32,
            "Configure 1..32 sessions"
        );
        let mut ids = BTreeSet::new();
        for session in &self.sessions {
            ensure!(
                valid_name(&session.id) && ids.insert(&session.id),
                "Invalid or duplicate session id"
            );
            ensure!(
                session.name.len() <= 128 && !session.name.contains(['\0', '\n', '\r']),
                "Invalid session name"
            );
            ensure!(
                session.command.is_absolute(),
                "Session commands must be absolute"
            );
            ensure!(
                session.args.len() <= 32
                    && session
                        .args
                        .iter()
                        .all(|arg| arg.len() <= 1024 && !arg.contains('\0')),
                "Invalid session arguments"
            );
        }
        ensure!(
            self.compositor.is_absolute() && self.greeter.is_absolute(),
            "Compositor and greeter paths must be absolute"
        );
        ensure!(
            self.allowed_users.len() <= 256
                && self.allowed_users.iter().all(|name| valid_name(name)),
            "Invalid account allowlist"
        );
        Ok(())
    }
    fn selected_session(&self, id: &str) -> Option<&SessionConfig> {
        self.sessions.iter().find(|session| session.id == id)
    }
    fn permits(&self, user: &UnixUser, greeter_uid: u32) -> bool {
        user.uid != 0
            && user.uid != greeter_uid
            && user.uid >= self.min_uid
            && (self.allowed_users.is_empty() || self.allowed_users.contains(&user.username))
            && user.home.is_absolute()
            && user.shell.is_absolute()
            && !user
                .shell
                .file_name()
                .is_some_and(|name| name == "nologin" || name == "false")
    }
    fn catalog(&self, greeter_uid: u32) -> Catalog {
        let mut users = BTreeMap::new();

        if let Ok(passwd) = fs::read_to_string("/etc/passwd") {
            for line in passwd.lines() {
                let name = line.split(':').next().unwrap_or_default();
                if let Ok(user) = rix_auth::user_by_name(name) {
                    if self.permits(&user, greeter_uid) && users.len() < 128 {
                        let display_name = if user.display_name.is_empty() {
                            user.username.clone()
                        } else {
                            user.display_name
                        };
                        users.insert(
                            user.username.clone(),
                            User {
                                username: user.username,
                                display_name,
                            },
                        );
                    }
                }
            }
        }
        Catalog {
            users: users.into_values().collect(),
            sessions: self
                .sessions
                .iter()
                .map(|session| Session {
                    id: session.id.clone(),
                    name: session.name.clone(),
                })
                .collect(),
        }
    }
    fn prerequisites(&self, path: &Path) -> Result<UnixUser> {
        secure_path(path, false)?;
        for binary in [&self.compositor, &self.greeter]
            .into_iter()
            .chain(self.sessions.iter().map(|session| &session.command))
        {
            secure_path(binary, true)?;
        }
        for policy in ["rix-dm", "rix-greeter", "rix-lock"] {
            secure_path(&Path::new("/etc/pam.d").join(policy), false)?;
        }
        secure_path(Path::new("/usr/bin/loginctl"), true)?;
        secure_path(Path::new("/usr/bin/systemctl"), true)?;
        let user = rix_auth::user_by_name(&self.greeter_user)
            .context("Dedicated greeter account is missing")?;
        ensure!(
            user.uid != 0 && user.uid < self.min_uid,
            "Greeter must be a dedicated system account below min_uid"
        );
        ensure!(
            Path::new("/run/systemd/seats/seat0").exists(),
            "logind seat0 is unavailable"
        );
        ensure!(
            Path::new(&format!("/dev/tty{}", self.vt)).exists(),
            "Configured VT is unavailable"
        );
        Ok(user)
    }
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}
fn secure_path(path: &Path, executable: bool) -> Result<()> {
    ensure!(path.is_absolute(), "Privileged paths must be absolute");
    ensure!(
        !path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir)),
        "Privileged paths must not contain '..'"
    );

    for part in path.ancestors() {
        let metadata = fs::metadata(part)
            .with_context(|| format!("Missing required path: {}", part.display()))?;
        ensure!(
            metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
            "Privileged path is not root-owned and protected: {}",
            part.display()
        );
    }
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("Missing required file: {}", path.display()))?;
    for part in canonical.ancestors() {
        let metadata = fs::metadata(part)?;
        ensure!(
            metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
            "Privileged path is not root-owned and protected: {}",
            part.display()
        );
    }
    let metadata = fs::metadata(&canonical)?;
    ensure!(
        metadata.is_file(),
        "Expected a regular file: {}",
        path.display()
    );
    if executable {
        ensure!(
            metadata.mode() & 0o111 != 0,
            "Executable permission is missing: {}",
            path.display()
        );
    }
    Ok(())
}

const FAILURES_BEFORE_LONG_LOCKOUT: u32 = 10;
const LONG_LOCKOUT_START_SECS: u64 = 5 * 60;
const LONG_LOCKOUT_CAP_SECS: u64 = 30 * 60;
const GLOBAL_FAILURE_LIMIT: u32 = 50;
const GLOBAL_FAILURE_WINDOW: Duration = Duration::from_secs(5 * 60);
const GLOBAL_LOCKOUT_START_SECS: u64 = 60;
const GLOBAL_LOCKOUT_CAP_SECS: u64 = 30 * 60;
const MAX_TRACKED_USERS: usize = 256;

struct Attempts {
    failures: u32,
    until: Instant,
}
impl Attempts {
    fn new(now: Instant) -> Self {
        Self {
            failures: 0,
            until: now,
        }
    }
    fn remaining(&self, now: Instant) -> u64 {
        remaining_secs(self.until, now)
    }
}
fn remaining_secs(until: Instant, now: Instant) -> u64 {
    until
        .saturating_duration_since(now)
        .as_secs()
        .saturating_add(u64::from(until > now))
}

fn failure_delay(failures: u32) -> u64 {
    if failures < FAILURES_BEFORE_LONG_LOCKOUT {
        (1u64 << failures.min(5)).min(30)
    } else {
        LONG_LOCKOUT_START_SECS
            .saturating_mul(1u64 << (failures - FAILURES_BEFORE_LONG_LOCKOUT).min(5))
            .min(LONG_LOCKOUT_CAP_SECS)
    }
}

struct RateLimit {
    users: BTreeMap<String, Attempts>,
    global_failures: u32,
    global_since: Instant,
    global_until: Instant,
    global_lockouts: u32,
}
impl RateLimit {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            users: BTreeMap::new(),
            global_failures: 0,
            global_since: now,
            global_until: now,
            global_lockouts: 0,
        }
    }
    fn remaining(&self, username: &str) -> u64 {
        let now = Instant::now();
        self.users
            .get(username)
            .map_or(0, |attempts| attempts.remaining(now))
            .max(self.global_remaining(now))
    }
    fn fail(&mut self, username: &str) -> u64 {
        let now = Instant::now();

        if now.saturating_duration_since(self.global_since) > GLOBAL_FAILURE_WINDOW {
            self.global_failures = 0;
        }
        self.global_since = now;
        self.global_failures = self.global_failures.saturating_add(1);
        if self.global_failures >= GLOBAL_FAILURE_LIMIT {
            self.global_failures = 0;
            self.global_lockouts = self.global_lockouts.saturating_add(1);
            let seconds = GLOBAL_LOCKOUT_START_SECS
                .saturating_mul(1u64 << self.global_lockouts.min(5))
                .min(GLOBAL_LOCKOUT_CAP_SECS);
            self.global_until = now + Duration::from_secs(seconds);
        }
        let attempts = self
            .users
            .entry(username.to_owned())
            .or_insert_with(|| Attempts::new(now));
        attempts.failures = attempts.failures.saturating_add(1);
        let seconds = failure_delay(attempts.failures);
        attempts.until = now + Duration::from_secs(seconds);
        self.forget_stale(now, username);
        seconds.max(self.global_remaining(now))
    }

    fn succeed(&mut self, username: &str) {
        self.users.remove(username);
    }
    fn global_remaining(&self, now: Instant) -> u64 {
        remaining_secs(self.global_until, now)
    }

    fn forget_stale(&mut self, now: Instant, username: &str) {
        if self.users.len() <= MAX_TRACKED_USERS {
            return;
        }
        self.users.retain(|_, attempts| attempts.remaining(now) > 0);
        if self.users.len() > MAX_TRACKED_USERS {
            self.users.remove(username);
        }
    }
}

fn rate_key(username: &str) -> String {
    rix_auth::user_by_name(username)
        .map(|user| user.username)
        .unwrap_or_else(|_| username.to_lowercase())
}

#[derive(Serialize, Deserialize)]
enum SessionEvent {
    Opened(String),
    Started,
    LidPolicy(bool),
}
struct Supervisor {
    pid: i32,
    monitor: Option<UnixStream>,
    events: Vec<u8>,
    session_id: Option<String>,
    started: bool,
    lid_policy: bool,
    since: Instant,
}
impl Supervisor {
    fn new(pid: i32, monitor: Option<UnixStream>) -> Self {
        if let Some(stream) = &monitor {
            let _ = stream.set_nonblocking(true);
        }
        Self {
            pid,
            monitor,
            events: Vec::new(),
            session_id: None,
            started: false,

            lid_policy: true,
            since: Instant::now(),
        }
    }
    fn monitor(&mut self) {
        let Some(stream) = &mut self.monitor else {
            return;
        };
        let mut bytes = [0u8; 512];
        loop {
            match stream.read(&mut bytes) {
                Ok(0) => break,
                Ok(count) => {
                    self.events.extend_from_slice(&bytes[..count]);
                    if self.events.len() > 4096 {
                        self.events.clear();
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        while self.events.len() >= 4 {
            let length = u32::from_be_bytes(self.events[..4].try_into().unwrap()) as usize;
            if length == 0 || length > 1024 {
                self.events.clear();
                break;
            }
            if self.events.len() < length + 4 {
                break;
            }
            match serde_json::from_slice(&self.events[4..length + 4]) {
                Ok(SessionEvent::Opened(id)) if valid_name(&id) => self.session_id = Some(id),
                Ok(SessionEvent::Started) => self.started = true,
                Ok(SessionEvent::LidPolicy(enabled)) => self.lid_policy = enabled,
                _ => {}
            }
            self.events.drain(..length + 4);
        }
    }
    fn terminate_logind_session(&self) {
        if let Some(id) = &self.session_id {
            let _ = bounded_command(
                Command::new("/usr/bin/loginctl").args(["terminate-session", id]),
                Duration::from_secs(2),
            );
        }
    }
    fn finished(&mut self) -> bool {
        if self.pid == 0 {
            return true;
        }
        self.monitor();
        let exited = match child_exited(self.pid) {
            Ok(exited) => exited,
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                self.pid = 0;
                self.terminate_logind_session();
                return true;
            }
            Err(_) => false,
        };
        if !exited {
            return false;
        }

        unsafe {
            libc::kill(-self.pid, libc::SIGKILL);
        }
        self.terminate_logind_session();
        let mut status = 0;
        unsafe {
            libc::waitpid(self.pid, &mut status, 0);
        }
        self.pid = 0;
        true
    }
    fn stop(&mut self) {
        if self.pid == 0 {
            return;
        }
        self.monitor();
        unsafe {
            libc::kill(self.pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if self.finished() {
                return;
            }
            if Instant::now() >= deadline {
                self.monitor();
                unsafe {
                    libc::kill(-self.pid, libc::SIGKILL);
                    libc::kill(self.pid, libc::SIGKILL);
                }
                self.terminate_logind_session();
                let mut status = 0;
                unsafe {
                    libc::waitpid(self.pid, &mut status, 0);
                }
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        self.pid = 0;
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Greeter,
    User,
}
fn close_worker_fds(descriptors: &[i32]) {
    for &descriptor in descriptors {
        if descriptor >= 0 {
            unsafe {
                libc::close(descriptor);
            }
        }
    }
}
fn start_greeter_supervisor(
    config: &Config,
    user: &UnixUser,
    listener_fd: i32,
    lock_fd: i32,
    lid_fd: Option<i32>,
) -> Result<Supervisor> {

    let (parent, mut monitor) = UnixStream::pair()?;
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error().into());
    }
    if pid == 0 {
        drop(parent);
        close_worker_fds(&[listener_fd, lock_fd, lid_fd.unwrap_or(-1)]);
        STOP.store(false, Ordering::Relaxed);
        let result = session_supervisor(config, user, Kind::Greeter, None, None, &mut monitor);
        if let Err(error) = &result {
            eprintln!("rix-dm: session setup failed: {error:#}");
        }
        unsafe {
            libc::_exit(i32::from(result.is_err()));
        }
    }
    drop(monitor);
    Ok(Supervisor::new(pid, Some(parent)))
}

fn login_worker(
    config: &Config,
    greeter_uid: u32,
    username: &str,
    password: SecretString,
    session: String,
    close_fds: [i32; 4],
) -> Result<(Supervisor, UnixStream)> {
    let (parent, mut worker) = UnixStream::pair()?;
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error().into());
    }
    if pid == 0 {
        drop(parent);
        close_worker_fds(&close_fds);
        unsafe {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
        }

        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
        }
        if unsafe { libc::getppid() } <= 1 {
            unsafe {
                libc::_exit(1);
            }
        }
        if unsafe { libc::setsid() } < 0 {
            unsafe {
                libc::_exit(1);
            }
        }
        let authenticated = authenticate_login(config, greeter_uid, username, password, &session);
        let code = match authenticated {
            Ok((user, pam)) => {
                let started = rix_dm::send(&mut worker, &Reply::Authenticated).and_then(|_| {
                    let reply: Reply = rix_dm::receive_before(
                        &mut worker,
                        Instant::now() + Duration::from_secs(15),
                    )?;
                    if reply != Reply::Ok {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "Session start was cancelled",
                        ));
                    }
                    Ok(())
                });
                if started.is_ok() {

                    signals();
                    STOP.store(false, Ordering::Relaxed);
                    match session_supervisor(
                        config,
                        &user,
                        Kind::User,
                        Some(pam),
                        config.selected_session(&session),
                        &mut worker,
                    ) {
                        Ok(()) => 0,
                        Err(error) => {
                            eprintln!("rix-dm: session setup failed: {error:#}");
                            1
                        }
                    }
                } else {
                    drop(pam);
                    1
                }
            }
            Err(reason) => {
                let _ = rix_dm::send(&mut worker, &rejection(reason, 0));
                1
            }
        };
        unsafe {
            libc::_exit(code);
        }
    }

    drop(password);
    drop(worker);
    Ok((Supervisor::new(pid, None), parent))
}
fn authentication_result(worker: &mut UnixStream) -> io::Result<Reply> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if STOP.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Display manager is stopping",
            ));
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "Authentication timed out"))?;
        let mut descriptor = libc::pollfd {
            fd: worker.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result =
            unsafe { libc::poll(&mut descriptor, 1, remaining.as_millis().min(100) as i32) };
        if result > 0 {
            return rix_dm::receive_before(worker, deadline);
        }
        if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error());
        }
    }
}

fn session_supervisor(
    config: &Config,
    user: &UnixUser,
    kind: Kind,
    transaction: Option<PamTransaction>,
    selected: Option<&SessionConfig>,
    monitor: &mut UnixStream,
) -> Result<()> {
    if unsafe { libc::getsid(0) } != unsafe { libc::getpid() } {
        ensure!(
            unsafe { libc::setsid() } >= 0,
            "Cannot create session supervisor"
        );
    }
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
    }

    ensure!(
        unsafe { libc::getppid() } > 1,
        "Display manager exited during session setup"
    );
    let tty = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("/dev/tty{}", config.vt))
        .context("Cannot open login VT")?;
    ensure!(
        unsafe { libc::ioctl(tty.as_raw_fd(), libc::TIOCSCTTY, 0) } == 0,
        "Cannot take controlling login VT"
    );

    let vt_control = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty0")?;
    const VT_ACTIVATE: libc::c_ulong = 0x5606;
    const VT_WAITACTIVE: libc::c_ulong = 0x5607;
    ensure!(
        unsafe {
            libc::ioctl(
                vt_control.as_raw_fd(),
                VT_ACTIVATE,
                config.vt as libc::c_int,
            )
        } == 0,
        "Cannot activate login VT"
    );
    ensure!(
        unsafe {
            libc::ioctl(
                vt_control.as_raw_fd(),
                VT_WAITACTIVE,
                config.vt as libc::c_int,
            )
        } == 0,
        "Login VT activation did not complete"
    );
    let mut pam = match transaction {
        Some(pam) => pam,
        None => {
            let mut pam = PamTransaction::start(
                "rix-greeter",
                &user.username,
                SecretString::new(String::new()),
            )?;
            pam.account()?;
            pam
        }
    };
    pam.set_item(rix_auth::PAM_TTY, &format!("tty{}", config.vt))?;
    let class = match kind {
        Kind::Greeter => "greeter",
        Kind::User => "user",
    };
    for value in [
        format!("XDG_SESSION_CLASS={class}"),
        "XDG_SESSION_TYPE=wayland".into(),
        "XDG_SESSION_DESKTOP=RIX".into(),
        "XDG_CURRENT_DESKTOP=RIX".into(),
        "XDG_SEAT=seat0".into(),
        format!("XDG_VTNR={}", config.vt),
    ] {
        pam.put_env(&value)?;
    }
    let opened = match kind {
        Kind::Greeter => pam.open_greeter_session(),
        Kind::User => pam.open_session(),
    };

    let session_id =
        match logind::verified_session(user.uid, unsafe { libc::getpid() }, config.vt, class) {
            Ok(id) => {
                if let Err(error) = pam.put_env(&format!("XDG_SESSION_ID={id}")) {

                    std::mem::forget(pam);
                    return Err(error.into());
                }
                id
            }
            Err(error) => {
                if let Err(reset_error) = pam.remove_env("XDG_SESSION_ID") {
                    std::mem::forget(pam);
                    return Err(reset_error)
                        .context("Cannot clear unverified PAM session identity");
                }
                opened.context("Cannot open PAM/logind session")?;
                return Err(error.context("Cannot verify the worker's logind identity"));
            }
        };
    monitor.set_write_timeout(Some(Duration::from_secs(2)))?;
    rix_dm::send(monitor, &SessionEvent::Opened(session_id.clone()))?;
    opened.context("Cannot open PAM/logind session")?;
    let environment: BTreeMap<_, _> = pam.environment().into_iter().collect();
    let rix_session = matches!(kind, Kind::User)
        && selected.is_some_and(|selected| {
            selected
                .command
                .file_name()
                .is_some_and(|name| name == "rix-session" || name == "rix-comp")
        });
    if rix_session {
        if let Err(error) =
            resources::configure_scope(user.uid, unsafe { libc::getpid() }, config.vt, &session_id)
        {
            eprintln!(
                "rix-dm: session resource preference unavailable; continuing login: {error:#}"
            );
        }
    }
    let user_config = environment
        .get("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&user.home).join(".config"));
    let user_config = if user_config.is_absolute() {
        user_config
    } else {
        Path::new(&user.home).join(user_config)
    };
    let lid_config = user_config.join("rix/desktop.json");
    let mut lid_policy =
        matches!(kind, Kind::Greeter) || rix_session && lid::keep_running_from(&lid_config);
    rix_dm::send(monitor, &SessionEvent::LidPolicy(lid_policy))?;
    let runtime = PathBuf::from(
        environment
            .get("XDG_RUNTIME_DIR")
            .context("PAM did not provide XDG_RUNTIME_DIR; enable pam_systemd")?,
    );
    ensure!(
        runtime == Path::new(&format!("/run/user/{}", user.uid)),
        "PAM returned an unexpected runtime directory"
    );
    let runtime_info =
        fs::symlink_metadata(&runtime).context("PAM runtime directory is missing")?;
    ensure!(
        runtime_info.is_dir() && runtime_info.uid() == user.uid && runtime_info.mode() & 0o077 == 0,
        "PAM runtime directory has unsafe ownership or permissions"
    );
    let mut command = match kind {
        Kind::Greeter => {
            let mut command = Command::new(&config.compositor);
            command
                .args([
                    "--drm",
                    "--experimental-kms",
                    "--greeter",
                    "--greeter-command",
                ])
                .arg(&config.greeter);
            command
        }
        Kind::User => {
            let selected = selected.context("No selected session")?;
            let mut command = Command::new(&selected.command);
            command.args(&selected.args);
            command
        }
    };
    command
        .env_clear()
        .env(
            "PATH",
            session_environment::path(Path::new(&user.home), matches!(kind, Kind::User)),
        )
        .env("HOME", &user.home)
        .env("USER", &user.username)
        .env("LOGNAME", &user.username)
        .env("SHELL", &user.shell)
        .env("LANG", "C.UTF-8");

    for (key, value) in &environment {
        if key.starts_with("XDG_")
            || ["LANG", "LANGUAGE", "LC_ALL", "TZ", "KRB5CCNAME"].contains(&key.as_str())
        {
            command.env(key, value);
        }
    }
    command
        .env("XDG_SESSION_ID", &session_id)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_SESSION_TYPE", "wayland")
        .env("XDG_SESSION_CLASS", class)
        .env("XDG_SESSION_DESKTOP", "RIX")
        .env("XDG_CURRENT_DESKTOP", "RIX")
        .env("XDG_SEAT", "seat0")
        .env("XDG_VTNR", config.vt.to_string());
    if rix_session {
        command.env("RIX_RESOURCE_POLICY", "1");
    }
    if matches!(kind, Kind::Greeter) {
        command
            .env("RIX_DM_SOCKET", rix_dm::DEFAULT_SOCKET)
            .env("RIX_GREETER_SESSION", "1")

            .env("RUST_LOG", "info");

        command.current_dir(&runtime);
    } else {
        command.current_dir(&user.home);
    }
    command
        .stdin(Stdio::from(tty.try_clone()?))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let groups = supplementary_groups(user)?;
    let uid = user.uid;
    let gid = user.gid;
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            if rix_session {
                resources::prepare_child();
            }
            if libc::setgroups(groups.len(), groups.as_ptr()) != 0
                || libc::setresgid(gid, gid, gid) != 0
                || libc::setresuid(uid, uid, uid) != 0
            {
                return Err(io::Error::last_os_error());
            }
            if libc::getuid() != uid || libc::geteuid() != uid || libc::getgid() != gid {
                return Err(io::Error::from_raw_os_error(libc::EPERM));
            }
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            if libc::getppid() <= 1 {
                return Err(io::Error::from_raw_os_error(libc::EPIPE));
            }
            libc::umask(0o077);

            Ok(())
        });
    }
    let mut child = command.spawn().context("Cannot start graphical session")?;

    let _ = rix_dm::send(monitor, &SessionEvent::Started);
    unsafe {
        libc::tcsetpgrp(tty.as_raw_fd(), child.id() as libc::pid_t);
    }
    eprintln!(
        "rix-dm: {class} session {session_id} started; graphical child pid={}",
        child.id()
    );
    let mut lid_check = Instant::now();
    let result = loop {
        if STOP.load(Ordering::Relaxed) {
            break Ok(());
        }
        match child_exited(child.id() as libc::pid_t) {
            Ok(true) => break Ok(()),
            Ok(false) => {}
            Err(error) => break Err(error.into()),
        }
        if rix_session && lid_check.elapsed() >= Duration::from_secs(1) {
            lid_check = Instant::now();
            let desired = lid::keep_running_from(&lid_config);
            if desired != lid_policy {
                lid_policy = desired;
                let _ = rix_dm::send(monitor, &SessionEvent::LidPolicy(desired));
            }
        }
        thread::sleep(Duration::from_millis(50));
    };
    eprintln!("rix-dm: {class} session {session_id} ending; returning seat to display manager");
    cleanup_session(&mut child, &session_id);

    drop(pam);

    let _ = bounded_command(
        Command::new("/usr/bin/loginctl").args([
            "kill-session",
            "--kill-whom=all",
            "--signal=KILL",
            &session_id,
        ]),
        Duration::from_secs(2),
    );
    result
}
fn supplementary_groups(user: &UnixUser) -> Result<Vec<libc::gid_t>> {
    let username = CString::new(user.username.as_str())?;
    let mut groups = vec![0; 16];
    for _ in 0..3 {
        let mut count = groups.len() as libc::c_int;
        let status = unsafe {
            libc::getgrouplist(username.as_ptr(), user.gid, groups.as_mut_ptr(), &mut count)
        };
        ensure!((1..=65536).contains(&count), "Invalid NSS group count");
        if status >= 0 {
            groups.truncate(count as usize);
            return Ok(groups);
        }
        groups.resize(count as usize, 0);
    }
    anyhow::bail!("NSS group membership changed repeatedly during session setup")
}
fn child_exited(pid: libc::pid_t) -> io::Result<bool> {

    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let status = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if status < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { info.si_pid() } == pid)
}
fn cleanup_session(child: &mut Child, session_id: &str) {
    let pgid = child.id() as libc::pid_t;
    unsafe {
        libc::kill(-pgid, libc::SIGTERM);
    }

    let _ = bounded_command(
        Command::new("/usr/bin/loginctl").args([
            "kill-session",
            "--kill-whom=all",
            "--signal=TERM",
            session_id,
        ]),
        Duration::from_secs(1),
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && unsafe { libc::kill(-pgid, 0) } == 0 {
        thread::sleep(Duration::from_millis(25));
    }
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
    let _ = child.wait();
}
fn bounded_command(command: &mut Command, timeout: Duration) -> io::Result<bool> {
    let mut child = command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.success());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn bind_socket(greeter: &UnixUser) -> Result<(UnixListener, SocketGuard, fs::File)> {
    let root = Path::new("/run/rix-dm");
    if !root.exists() {
        fs::create_dir(root)?;
    }
    let info = fs::symlink_metadata(root)?;
    ensure!(
        info.is_dir() && info.uid() == 0 && info.mode() & 0o022 == 0,
        "Unsafe display-manager runtime directory"
    );
    let path = root.join("control.sock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join("daemon.lock"))?;
    let lock_info = lock.metadata()?;
    ensure!(
        lock_info.is_file() && lock_info.uid() == 0 && lock_info.mode() & 0o077 == 0,
        "Unsafe display-manager lock file"
    );
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "Another display-manager daemon is running"
    );
    if let Ok(info) = fs::symlink_metadata(&path) {
        use std::os::unix::fs::FileTypeExt;
        ensure!(
            info.file_type().is_socket() && info.uid() == 0,
            "Unsafe stale display-manager socket"
        );
        fs::remove_file(&path)?;
    }
    let name = CString::new(root.as_os_str().as_encoded_bytes())?;
    ensure!(
        unsafe { libc::chown(name.as_ptr(), 0, greeter.gid) } == 0,
        "Cannot assign greeter runtime group"
    );
    fs::set_permissions(root, fs::Permissions::from_mode(0o750))?;
    let listener = UnixListener::bind(&path)?;
    let guard = SocketGuard(path.clone());
    let name = CString::new(path.as_os_str().as_encoded_bytes())?;
    ensure!(
        unsafe { libc::chown(name.as_ptr(), 0, greeter.gid) } == 0,
        "Cannot assign greeter socket group"
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o660))?;
    listener.set_nonblocking(true)?;
    Ok((listener, guard, lock))
}

fn rejection(reason: LoginError, seconds: u64) -> Reply {
    Reply::Rejected {
        reason,
        retry_after_secs: seconds,
    }
}
fn authenticate_login(
    config: &Config,
    greeter_uid: u32,
    username: &str,
    password: SecretString,
    session: &str,
) -> std::result::Result<(UnixUser, PamTransaction), LoginError> {

    if config.selected_session(session).is_none() {
        return Err(LoginError::InvalidSession);
    }
    if !valid_name(username)
        || password.expose().len() > rix_dm::MAX_PASSWORD
        || password.expose().contains('\0')
    {
        return Err(LoginError::InvalidRequest);
    }
    let user = rix_auth::user_by_name(username).map_err(|_| LoginError::AuthenticationFailed)?;
    if !config.permits(&user, greeter_uid) {
        return Err(LoginError::AuthenticationFailed);
    }
    let mut pam = PamTransaction::start("rix-dm", username, password)
        .map_err(|_| LoginError::AuthenticationFailed)?;
    pam.set_item(rix_auth::PAM_TTY, &format!("tty{}", config.vt))
        .map_err(|_| LoginError::AuthenticationFailed)?;
    pam.authenticate()
        .map_err(|_| LoginError::AuthenticationFailed)?;
    pam.account().map_err(|error| match error {
        rix_auth::AuthError::PasswordExpired => LoginError::PasswordExpired,
        _ => LoginError::AuthenticationFailed,
    })?;
    let canonical = rix_auth::user_by_name(
        &pam.canonical_user()
            .map_err(|_| LoginError::AuthenticationFailed)?,
    )
    .map_err(|_| LoginError::AuthenticationFailed)?;
    if canonical.uid != user.uid || !config.permits(&canonical, greeter_uid) {
        return Err(LoginError::AuthenticationFailed);
    }

    if config.selected_session(session).is_some_and(|selected| {
        selected
            .command
            .file_name()
            .is_some_and(|name| name == "rix-session" || name == "rix-comp")
    }) && running_session::has_compositor(canonical.uid)
    {
        eprintln!(
            "rix-dm: login rejected: Rix session is already running for uid {}",
            canonical.uid
        );
        return Err(LoginError::SessionAlreadyRunning);
    }
    Ok((canonical, pam))
}

fn daemon(config: Config, greeter: UnixUser) -> Result<()> {
    ensure!(
        unsafe { libc::getuid() } == 0 && unsafe { libc::geteuid() } == 0,
        "The display-manager daemon must run as root"
    );
    signals();
    let (listener, _socket, lock) = bind_socket(&greeter)?;
    let catalog = config.catalog(greeter.uid);
    let mut rate = RateLimit::new();

    let mut lid_guard = lid::LidGuard::default();
    if let Err(error) = lid_guard.update(lid::external_display_connected(), true) {
        eprintln!("rix-dm: initial lid protection unavailable: {error:#}");
    }
    let mut lid_check = Instant::now();
    let mut current = start_greeter_supervisor(
        &config,
        &greeter,
        listener.as_raw_fd(),
        lock.as_raw_fd(),
        lid_guard.fd(),
    )?;
    let mut current_kind = Kind::Greeter;
    let mut last_start = Instant::now();
    let mut crash_backoff = 1u64;
    while !STOP.load(Ordering::Relaxed) {
        current.monitor();
        if lid_check.elapsed() >= Duration::from_secs(1) {
            lid_check = Instant::now();
            if let Err(error) =
                lid_guard.update(lid::external_display_connected(), current.lid_policy)
            {
                eprintln!("rix-dm: lid protection unavailable: {error:#}");

                lid_check = Instant::now() + Duration::from_secs(4);
            }
        }
        if !current.started && current.since.elapsed() > Duration::from_secs(45) {
            eprintln!("rix-dm: graphical session startup timed out");
            current.stop();
        }
        if current.finished() {
            current.pid = 0;
            if matches!(current_kind, Kind::User) {
                eprintln!("rix-dm: user session ended; starting the login screen");
            }
            if matches!(current_kind, Kind::Greeter) {
                if last_start.elapsed() < Duration::from_secs(30) {
                    crash_backoff = (crash_backoff * 2).min(30);
                } else {
                    crash_backoff = 1;
                }
                eprintln!("rix-dm: greeter exited; restarting in {crash_backoff}s");
                let deadline = Instant::now() + Duration::from_secs(crash_backoff);
                while Instant::now() < deadline && !STOP.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(50));
                }
                if STOP.load(Ordering::Relaxed) {
                    break;
                }
            }
            if let Err(error) = lid_guard.update(lid::external_display_connected(), true) {
                eprintln!("rix-dm: greeter lid protection unavailable: {error:#}");
            }
            current = start_greeter_supervisor(
                &config,
                &greeter,
                listener.as_raw_fd(),
                lock.as_raw_fd(),
                lid_guard.fd(),
            )?;
            current_kind = Kind::Greeter;
            last_start = Instant::now();
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let peer = match rix_dm::peer_credentials(&stream) {
                    Ok(peer) => peer,
                    Err(_) => continue,
                };
                if !matches!(current_kind, Kind::Greeter)
                    || !rix_dm::authorize_peer(peer, greeter.uid, current.pid)
                {
                    continue;
                }
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                let request: Request = match rix_dm::receive_before(
                    &mut stream,
                    Instant::now() + Duration::from_secs(5),
                ) {
                    Ok(request) => request,
                    Err(_) => {
                        let _ =
                            rix_dm::send(&mut stream, &rejection(LoginError::InvalidRequest, 0));
                        continue;
                    }
                };
                match request {
                    Request::GetCatalog => {
                        let _ = rix_dm::send(&mut stream, &Reply::Catalog(catalog.clone()));
                    }
                    Request::Power { action } => {
                        let verb = match action {
                            PowerAction::PowerOff => "poweroff",
                            PowerAction::Reboot => "reboot",
                        };
                        let result = bounded_command(
                            Command::new("/usr/bin/systemctl").args(["--no-block", verb]),
                            Duration::from_secs(2),
                        );
                        let reply = if result.is_ok_and(|success| success) {
                            Reply::Ok
                        } else {
                            rejection(LoginError::SessionFailed, 0)
                        };
                        let _ = rix_dm::send(&mut stream, &reply);
                    }
                    Request::Login {
                        username,
                        password,
                        session,
                    } => {

                        let rate_user = rate_key(&username);
                        let wait = rate.remaining(&rate_user);
                        if wait > 0 {
                            let _ = rix_dm::send(
                                &mut stream,
                                &rejection(LoginError::RateLimited, wait),
                            );
                            continue;
                        }
                        let (mut authentication, mut channel) = match login_worker(
                            &config,
                            greeter.uid,
                            &username,
                            password,
                            session,
                            [
                                listener.as_raw_fd(),
                                lock.as_raw_fd(),
                                stream.as_raw_fd(),
                                lid_guard.fd().unwrap_or(-1),
                            ],
                        ) {
                            Ok(worker) => worker,
                            Err(_) => {
                                let _ = rix_dm::send(
                                    &mut stream,
                                    &rejection(LoginError::SessionFailed, rate.fail(&rate_user)),
                                );
                                continue;
                            }
                        };
                        match authentication_result(&mut channel) {
                            Ok(Reply::Authenticated) => {

                                if rix_dm::send(&mut stream, &Reply::Authenticated).is_err() {
                                    authentication.stop();
                                    continue;
                                }
                                current.stop();
                                if rix_dm::send(&mut channel, &Reply::Ok).is_err() {
                                    authentication.stop();
                                    continue;
                                }
                                channel.set_nonblocking(true)?;
                                authentication.monitor = Some(channel);
                                authentication.since = Instant::now();
                                current = authentication;
                                current_kind = Kind::User;
                                last_start = Instant::now();
                                rate.succeed(&rate_user);
                            }
                            Ok(Reply::Rejected { reason, .. }) => {
                                authentication.stop();
                                let wait = if reason == LoginError::SessionAlreadyRunning {

                                    rate.succeed(&rate_user);
                                    0
                                } else {
                                    rate.fail(&rate_user)
                                };
                                let _ = rix_dm::send(&mut stream, &rejection(reason, wait));
                            }
                            _ => {
                                authentication.stop();
                                let wait = rate.fail(&rate_user);
                                let _ = rix_dm::send(
                                    &mut stream,
                                    &rejection(LoginError::SessionFailed, wait),
                                );
                            }
                        }
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50))
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    current.stop();
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config = Config::load(&args.config)?;
    let greeter = config.prerequisites(&args.config)?;
    if args.check {
        writeln!(io::stdout(), "Rix DM configuration, protected executables, PAM policies, greeter account, VT and logind seat0 are present.")?;
        writeln!(io::stdout(), "No services or accounts were changed. Native graphics still require hardware validation.")?;
        return Ok(());
    }
    daemon(config, greeter)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> Config {
        Config {
            greeter_user: "rix-greeter".into(),
            compositor: "/usr/bin/rix-comp".into(),
            greeter: "/usr/bin/rix-greeter".into(),
            vt: 1,
            min_uid: 1000,
            allowed_users: vec![],
            sessions: vec![SessionConfig {
                id: "rix".into(),
                name: "RixDE".into(),
                command: "/usr/bin/rix-session".into(),
                args: vec!["--native".into()],
            }],
        }
    }
    #[test]
    fn rejects_unknown_session_without_touching_pam() {
        assert_eq!(
            authenticate_login(
                &config(),
                991,
                "nobody",
                SecretString::new("secret".into()),
                "/bin/sh"
            )
            .err(),
            Some(LoginError::InvalidSession)
        );
    }
    #[test]
    fn root_and_greeter_accounts_are_never_sessions() {
        let root = rix_auth::user_by_uid(0).unwrap();
        assert!(!config().permits(&root, 991));
        let user = UnixUser {
            username: "test".into(),
            display_name: "Test".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/test".into(),
            shell: "/bin/sh".into(),
        };
        assert!(config().permits(&user, 991));
        assert!(!config().permits(&user, 1000));
        let mut restricted = config();
        restricted.allowed_users.push("other".into());
        assert!(!restricted.permits(&user, 991));
    }
    #[test]
    fn configuration_rejects_relative_commands_and_duplicate_ids() {
        let mut value = config();
        value.sessions[0].command = "sh".into();
        assert!(value.validate().is_err());
        let mut value = config();
        value.sessions.push(value.sessions[0].clone());
        assert!(value.validate().is_err());
        let mut value = config();
        value.vt = 0;
        assert!(value.validate().is_err());
    }
    #[test]
    fn authentication_retries_are_bounded_and_resettable() {
        let mut rate = RateLimit::new();
        assert_eq!(rate.remaining("owner"), 0);
        assert_eq!(rate.fail("owner"), 2);
        assert!(rate.remaining("owner") >= 1);
        for _ in 0..5 {
            assert!(rate.fail("owner") <= 30);
        }
        rate.succeed("owner");
        assert_eq!(rate.remaining("owner"), 0);
    }
    #[test]
    fn one_users_success_does_not_forgive_another_users_failures() {
        let mut rate = RateLimit::new();
        for _ in 0..FAILURES_BEFORE_LONG_LOCKOUT {
            rate.fail("attacker");
        }
        assert!(rate.remaining("attacker") >= LONG_LOCKOUT_START_SECS);
        assert_eq!(rate.remaining("victim"), 0);

        rate.succeed("victim");
        assert!(rate.remaining("attacker") >= LONG_LOCKOUT_START_SECS);
        assert_eq!(rate.remaining("victim"), 0);
    }
    #[test]
    fn user_series_escalates_from_minutes_to_the_cap() {
        assert!(failure_delay(FAILURES_BEFORE_LONG_LOCKOUT - 1) <= 30);
        assert_eq!(
            failure_delay(FAILURES_BEFORE_LONG_LOCKOUT),
            LONG_LOCKOUT_START_SECS
        );
        assert_eq!(
            failure_delay(FAILURES_BEFORE_LONG_LOCKOUT + 1),
            LONG_LOCKOUT_START_SECS * 2
        );
        assert_eq!(failure_delay(u32::MAX), LONG_LOCKOUT_CAP_SECS);
    }
    #[test]
    fn distributed_failures_trip_one_global_lockout_for_every_username() {
        let mut rate = RateLimit::new();
        for index in 0..GLOBAL_FAILURE_LIMIT - 1 {
            rate.fail(&format!("guess{index}"));
            assert_eq!(rate.remaining("untouched"), 0);
        }
        rate.fail("guess-last");
        assert!(rate.remaining("untouched") >= GLOBAL_LOCKOUT_START_SECS);
        assert!(rate.remaining("guess0") >= GLOBAL_LOCKOUT_START_SECS);
    }
    #[test]
    fn tracked_usernames_stay_bounded() {
        let mut rate = RateLimit::new();
        for index in 0..MAX_TRACKED_USERS * 2 {
            rate.fail(&format!("user{index}"));
        }
        assert!(rate.users.len() <= MAX_TRACKED_USERS);
    }
    #[test]
    fn rate_keys_follow_the_resolved_account_and_fold_unknown_names() {
        let user = rix_auth::current_user().unwrap();
        assert_eq!(rate_key(&user.username), user.username);
        assert_eq!(rate_key("RixDmNoSuchUser"), "rixdmnosuchuser");
        assert_eq!(
            rate_key("RixDmNoSuchUser"),
            rate_key(&"RixDmNoSuchUser".to_uppercase())
        );
    }
    #[test]
    fn eviction_keeps_active_lockouts_and_drops_only_released_records() {
        let now = Instant::now();
        let locked = |until: Instant| Attempts { failures: 1, until };
        let mut room = RateLimit::new();
        for index in 0..MAX_TRACKED_USERS - 1 {
            room.users.insert(
                format!("locked{index}"),
                locked(now + Duration::from_secs(600)),
            );
        }
        room.users.insert(
            "released".into(),
            Attempts {
                failures: 0,
                until: now,
            },
        );
        room.fail("newcomer");
        assert!(room.users.contains_key("locked0"));
        assert!(!room.users.contains_key("released"));
        assert!(room.users.contains_key("newcomer"));
        assert!(room.users.len() <= MAX_TRACKED_USERS);

        let mut full = RateLimit::new();
        for index in 0..MAX_TRACKED_USERS {
            full.users.insert(
                format!("locked{index}"),
                locked(now + Duration::from_secs(600)),
            );
        }
        full.fail("untracked");
        assert!(!full.users.contains_key("untracked"));
        assert!(full.users.contains_key("locked0"));
        assert!(full.users.len() <= MAX_TRACKED_USERS);
    }
    #[test]
    fn exited_child_stays_reserved_until_group_cleanup() {
        let mut child = Command::new("/bin/true").spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !child_exited(child.id() as libc::pid_t).unwrap() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            unsafe { libc::kill(child.id() as libc::pid_t, 0) },
            0,
            "WNOWAIT must reserve the zombie PID"
        );
        assert!(child.wait().unwrap().success());
    }
    #[test]
    fn supervisor_decodes_partial_session_events_and_retains_crash_cleanup_identity() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let mut supervisor = Supervisor::new(0, Some(reader));
        let mut frame = Vec::new();
        rix_dm::send(&mut frame, &SessionEvent::Opened("fixture-session".into())).unwrap();
        writer.write_all(&frame[..3]).unwrap();
        supervisor.monitor();
        assert!(supervisor.session_id.is_none());
        writer.write_all(&frame[3..]).unwrap();
        supervisor.monitor();
        assert_eq!(supervisor.session_id.as_deref(), Some("fixture-session"));
        rix_dm::send(&mut writer, &SessionEvent::Started).unwrap();
        supervisor.monitor();
        assert!(supervisor.started);
        assert!(
            supervisor.lid_policy,
            "handoff defaults to a protected greeter"
        );
        rix_dm::send(&mut writer, &SessionEvent::LidPolicy(false)).unwrap();
        supervisor.monitor();
        assert!(
            !supervisor.lid_policy,
            "the daemon must honor the user's lid preference"
        );
        rix_dm::send(&mut writer, &SessionEvent::LidPolicy(true)).unwrap();
        supervisor.monitor();
        assert!(supervisor.lid_policy);
        assert!(
            supervisor.finished(),
            "A stopped supervisor must let the daemon restart the greeter"
        );
    }
    #[test]
    fn memberships_are_resolved_without_changing_process_credentials() {
        let user = rix_auth::current_user().unwrap();
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        assert!(supplementary_groups(&user).unwrap().contains(&user.gid));
        assert_eq!(unsafe { libc::getuid() }, uid);
        assert_eq!(unsafe { libc::getgid() }, gid);
    }
    #[test]
    fn pam_workers_do_not_keep_the_daemon_lid_lease_alive() {
        let (lease, mut peer) = UnixStream::pair().unwrap();
        let (mut parent_gate, worker_gate) = UnixStream::pair().unwrap();
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {

            close_worker_fds(&[lease.as_raw_fd(), parent_gate.as_raw_fd(), -1]);
            let mut byte = 0u8;
            unsafe {
                libc::write(worker_gate.as_raw_fd(), &byte as *const u8 as *const _, 1);
                libc::read(worker_gate.as_raw_fd(), &mut byte as *mut u8 as *mut _, 1);
                libc::_exit(0);
            }
        }
        drop(worker_gate);
        parent_gate
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        parent_gate.read_exact(&mut [0]).unwrap();
        drop(lease);
        peer.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let released = peer.read(&mut [0]);
        parent_gate.write_all(&[0]).unwrap();
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert_eq!(
            released.unwrap(),
            0,
            "an idle PAM worker must not retain the DM's fd"
        );
    }
}
