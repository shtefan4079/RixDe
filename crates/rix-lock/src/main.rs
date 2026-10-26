use rix_auth::{AuthError, SecretString, UnixUser};
use rix_login_ui::LoginWindow;
use rix_shell_adapter::SessionLockController;
use slint::ComponentHandle;
use std::{
    cell::RefCell,
    fs::OpenOptions,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const QUICK_DELAY_CAP: u64 = 30;
const LONG_DELAY_CAP: u64 = 30 * 60;
const LONG_DELAY_AFTER_FAILURES: u32 = 5;

const MAX_SESSION_ATTEMPTS: u32 = 30;

const ATTEMPTS_FILE: &str = "rix-lock-attempts";

const ATTEMPT_DECAY_STEP: Duration = Duration::from_secs(5 * 60);

#[derive(Default)]
struct Attempts {
    in_flight: bool,
    failures: u32,
    retry_after: Option<Instant>,
    status: String,
}
impl Attempts {

    fn restored(failures: u32, now: Instant) -> Self {
        let failures = failures.min(MAX_SESSION_ATTEMPTS);
        Self {
            in_flight: false,
            failures,
            retry_after: (failures > 0).then(|| now + retry_delay(failures)),
            status: if failures >= MAX_SESSION_ATTEMPTS {
                exhausted_status()
            } else {
                String::new()
            },
        }
    }
    fn begin(&mut self, now: Instant, locked: bool) -> bool {
        if !locked
            || self.in_flight
            || self.exhausted()
            || self.retry_after.is_some_and(|deadline| now < deadline)
        {
            return false;
        }
        self.in_flight = true;
        self.status.clear();
        true
    }
    fn failed(&mut self, now: Instant, status: String) {
        self.in_flight = false;
        self.failures = self.failures.saturating_add(1);
        self.retry_after = Some(now + retry_delay(self.failures));
        self.status = if self.exhausted() {
            exhausted_status()
        } else {
            status
        };
    }
    fn busy(&self, now: Instant) -> bool {
        self.in_flight
            || self.exhausted()
            || self.retry_after.is_some_and(|deadline| now < deadline)
    }
    fn exhausted(&self) -> bool {
        self.failures >= MAX_SESSION_ATTEMPTS
    }
}
fn exhausted_status() -> String {
    "Too many attempts for this lock session. Sign in from a text console.".into()
}
fn retry_delay(failures: u32) -> Duration {
    let cap = QUICK_DELAY_CAP
        .saturating_mul(1u64 << failures.saturating_sub(LONG_DELAY_AFTER_FAILURES).min(6))
        .min(LONG_DELAY_CAP);
    Duration::from_secs((1u64 << failures.min(12)).min(cap))
}

fn attempts_path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let runtime = PathBuf::from(runtime);
    runtime.is_absolute().then(|| runtime.join(ATTEMPTS_FILE))
}
fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
fn decayed_failures(failures: u32, elapsed: Duration) -> u32 {
    failures.saturating_sub((elapsed.as_secs() / ATTEMPT_DECAY_STEP.as_secs()) as u32)
}
fn restore_failures(path: &Path) -> u32 {
    let Ok(mut file) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    else {
        return 0;
    };
    let _ = file.lock();
    let mut text = String::new();
    if file.read_to_string(&mut text).is_err() {
        return 0;
    }
    let mut fields = text.split_whitespace();
    let failures = fields.next().and_then(|value| value.parse::<u32>().ok());
    let last = fields.next().and_then(|value| value.parse::<u64>().ok());
    match (failures, last) {
        (Some(failures), Some(last)) => decayed_failures(
            failures,
            Duration::from_secs(unix_seconds().saturating_sub(last)),
        ),
        _ => 0,
    }
}
fn store_failures(path: &Path, failures: u32) {
    let Ok(mut file) = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    else {
        return;
    };
    let _ = file.lock();
    let _ = writeln!(file, "{failures} {}", unix_seconds());
    let _ = file.sync_data();
}
enum AuthOutcome {
    Accepted,
    Rejected(String),
}
type Windows = Rc<RefCell<Vec<LoginWindow>>>;

fn harden_process() {
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
        let limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        libc::setrlimit(libc::RLIMIT_CORE, &limit);
    }
}
struct UiState {
    windows: Windows,
    clocks: RefCell<Vec<slint::Timer>>,
    attempts: Rc<RefCell<Attempts>>,
    attempts_path: Option<PathBuf>,
    controller: SessionLockController,
    user: UnixUser,
    worker: mpsc::Sender<SecretString>,
}
impl UiState {

    fn failed_attempt(&self, status: String) {
        let failures = {
            let mut attempts = self.attempts.borrow_mut();
            attempts.failed(Instant::now(), status);
            attempts.failures
        };
        if let Some(path) = &self.attempts_path {
            store_failures(path, failures);
        }
    }
    fn cleared_attempts(&self) {
        if let Some(path) = &self.attempts_path {
            let _ = std::fs::remove_file(path);
        }
    }
    fn add_window(self: &Rc<Self>) -> Result<(), slint::PlatformError> {
        let window = LoginWindow::new()?;
        let timer = rix_login_ui::initialize(&window, true);
        window.set_username(self.user.username.clone().into());
        window.set_display_name(self.user.display_name.clone().into());
        window.set_secondary_output(!self.windows.borrow().is_empty());
        window
            .set_busy(!self.controller.is_locked() || self.attempts.borrow().busy(Instant::now()));
        window.set_status(self.attempts.borrow().status.clone().into());
        let weak = Rc::downgrade(self);
        window.on_submit(move |password| {
            let secret = SecretString::new(password.to_string());
            let Some(state) = weak.upgrade() else {
                return;
            };
            if !state
                .attempts
                .borrow_mut()
                .begin(Instant::now(), state.controller.is_locked())
            {
                return;
            }
            for window in state.windows.borrow().iter() {
                window.set_password("".into());
                window.set_busy(true);
                window.set_status("".into());
            }
            if state.worker.send(secret).is_err() {

                state.failed_attempt(
                    "Password verification is unavailable. Try again.".into(),
                );
            }
        });
        window.show()?;
        window.invoke_focus_password();
        self.clocks.borrow_mut().push(timer);
        self.windows.borrow_mut().push(window);
        Ok(())
    }
}

fn main() -> anyhow::Result<()> {
    harden_process();
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments.iter().any(|a| a == "--help" || a == "-h") {
        println!("rix-lock [--preview]\n\nLock the current session using Rix's own screen and Linux PAM.\n--preview opens a normal window without locking or authenticating.");
        return Ok(());
    }
    if arguments == ["--preview"] {
        return preview();
    }
    anyhow::ensure!(arguments.is_empty(), "Unknown arguments; use --help");

    let uid = unsafe { libc::getuid() };
    let euid = unsafe { libc::geteuid() };
    anyhow::ensure!(
        uid != 0 && uid == euid,
        "rix-lock must run as the ordinary session user"
    );
    let user = rix_auth::current_user()?;
    let (request_tx, request_rx) = mpsc::channel::<SecretString>();
    let (result_tx, result_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("rix-lock-pam".into())
        .spawn(move || {
            while let Ok(secret) = request_rx.recv() {
                let result = match rix_auth::authenticate_current_user(secret) {
                    Ok(()) => AuthOutcome::Accepted,
                    Err(AuthError::PasswordExpired) => AuthOutcome::Rejected(
                        "Your password has expired. Change it from a console login.".into(),
                    ),
                    Err(_) => AuthOutcome::Rejected(
                        "Could not unlock. Check your password and try again.".into(),
                    ),
                };
                if result_tx.send(result).is_err() {
                    break;
                }
            }
        })?;
    let controller = rix_shell_adapter::install_session_lock()?;
    let attempts_path = attempts_path();
    let restored = attempts_path.as_deref().map_or(0, restore_failures);
    let state = Rc::new(UiState {
        windows: Rc::new(RefCell::new(Vec::new())),
        clocks: RefCell::new(Vec::new()),
        attempts: Rc::new(RefCell::new(Attempts::restored(restored, Instant::now()))),
        attempts_path,
        controller: controller.clone(),
        user,
        worker: request_tx,
    });
    for _ in 0..controller.output_count() {
        state.add_window()?;
    }
    let weak = Rc::downgrade(&state);
    controller.on_output_added(move || {
        weak.upgrade()
            .ok_or_else(|| slint::PlatformError::Other("Lock UI was dropped".into()))?
            .add_window()
    });
    let weak = Rc::downgrade(&state);
    controller.on_locked(move || {
        if let Some(state) = weak.upgrade() {
            for window in state.windows.borrow().iter() {
                window.set_busy(false);
            }
        }
    });
    let weak = Rc::downgrade(&state);
    controller.on_layout_changed(move |layout, caps_lock| {
        if let Some(state) = weak.upgrade() {
            for window in state.windows.borrow().iter() {
                window.set_layout_text(layout.clone().into());
                window.set_caps_lock(caps_lock);
            }
        }
    });
    let weak = Rc::downgrade(&state);
    controller.on_focus_changed(move |index| {
        if let Some(state) = weak.upgrade() {
            for (i, window) in state.windows.borrow().iter().enumerate() {
                window.set_secondary_output(i != index);
                if i == index {
                    window.invoke_focus_password();
                } else {
                    window.set_password("".into());
                }
            }
        }
    });
    let weak = Rc::downgrade(&state);
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            let Some(state) = weak.upgrade() else {
                return;
            };
            while let Ok(result) = result_rx.try_recv() {
                match result {
                    AuthOutcome::Accepted => {
                        if state.controller.unlock().is_err() {
                            state.failed_attempt(
                                "The lock is not confirmed yet. Try again.".into(),
                            );
                        } else {
                            state.cleared_attempts();
                        }
                    }
                    AuthOutcome::Rejected(status) => state.failed_attempt(status),
                }
            }
            let attempts = state.attempts.borrow();
            for window in state.windows.borrow().iter() {
                window.set_busy(!state.controller.is_locked() || attempts.busy(Instant::now()));
                window.set_status(attempts.status.clone().into());
            }
        },
    );
    slint::run_event_loop()?;

    Ok(())
}

fn preview() -> anyhow::Result<()> {
    let window = LoginWindow::new()?;
    let _clock = rix_login_ui::initialize(&window, true);
    if let Ok(user) = rix_auth::current_user() {
        window.set_username(user.username.into());
        window.set_display_name(user.display_name.into());
    }
    let weak = window.as_weak();
    window.on_submit(move |_| {
        if let Some(window) = weak.upgrade() {
            window.set_status(
                "Preview: the session is not locked.".into(),
            );
        }
    });
    window.run()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authentication_requires_confirmation_and_serializes_attempts() {
        let now = Instant::now();
        let mut attempts = Attempts::default();
        assert!(!attempts.begin(now, false));
        assert!(attempts.begin(now, true));
        assert!(!attempts.begin(now, true));
        attempts.failed(now, "Rejected".into());
        assert!(!attempts.begin(now, true));
        assert!(attempts.begin(now + retry_delay(1), true));
    }
    #[test]
    fn retry_delay_grows_past_the_quick_cap_and_stays_bounded() {
        assert_eq!(retry_delay(1), Duration::from_secs(2));
        assert_eq!(
            retry_delay(LONG_DELAY_AFTER_FAILURES),
            Duration::from_secs(30)
        );
        assert_eq!(
            retry_delay(LONG_DELAY_AFTER_FAILURES + 1),
            Duration::from_secs(60)
        );
        assert_eq!(retry_delay(u32::MAX), Duration::from_secs(LONG_DELAY_CAP));
    }
    #[test]
    fn a_lock_session_stops_accepting_attempts_after_the_total_limit() {
        let mut attempts = Attempts::default();
        let mut now = Instant::now();
        for index in 0..MAX_SESSION_ATTEMPTS {
            assert!(attempts.begin(now, true), "attempt {index} must start");
            attempts.failed(now, "Rejected".into());
            now += retry_delay(index + 1) + Duration::from_secs(1);
        }
        assert!(attempts.exhausted());
        assert!(
            !attempts.begin(now, true),
            "an exhausted lock session must refuse further attempts"
        );

        assert!(
            attempts.status.contains("console"),
            "unexpected exhausted status: {}",
            attempts.status
        );
    }
    #[test]
    fn the_owner_can_still_unlock_after_a_short_burst() {
        let now = Instant::now();
        let mut attempts = Attempts::default();
        attempts.failed(now, "Rejected".into());
        assert!(!attempts.begin(now, true));
        assert!(attempts.begin(now + retry_delay(1), true));
    }
    #[test]
    fn restored_attempts_keep_the_budget_and_decay_over_time() {
        assert_eq!(decayed_failures(20, Duration::ZERO), 20);
        assert_eq!(decayed_failures(20, ATTEMPT_DECAY_STEP), 19);
        assert_eq!(decayed_failures(3, ATTEMPT_DECAY_STEP * 3), 0);
        let now = Instant::now();
        let mut restored = Attempts::restored(3, now);
        assert_eq!(restored.failures, 3);
        assert!(
            !restored.begin(now, true),
            "restored failures must keep the retry delay"
        );
        assert!(restored.begin(now + retry_delay(3), true));
        let exhausted = Attempts::restored(MAX_SESSION_ATTEMPTS, now);
        assert!(exhausted.exhausted());
        assert!(
            !exhausted.status.is_empty(),
            "a restored lockout must explain itself"
        );
    }
    #[test]
    fn persisted_failures_round_trip_with_owner_only_permissions() {
        use std::os::unix::fs::MetadataExt;
        let path =
            std::env::temp_dir().join(format!("rix-lock-attempts-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        store_failures(&path, 7);
        assert_eq!(restore_failures(&path), 7);
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        std::fs::remove_file(&path).unwrap();
    }
}
