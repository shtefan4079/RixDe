use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

const SHELL_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(8);
const SHELL_STARTUP_GRACE: Duration = Duration::from_secs(45);

struct SupervisedProcess {
    command: Command,
    child: Option<Child>,
    started_at: Instant,
    retry_at: Instant,
    failures: u32,
    heartbeat_path: Option<PathBuf>,
    last_health_check: Option<Instant>,
    last_heartbeat: Option<u64>,
    kill_requested: bool,
}

impl SupervisedProcess {
    fn new(command: Command) -> Self {
        Self::with_heartbeat(command, None)
    }

    fn with_heartbeat(command: Command, heartbeat_path: Option<PathBuf>) -> Self {
        let now = Instant::now();
        Self {
            command,
            child: None,
            started_at: now,
            retry_at: now,
            failures: 0,
            heartbeat_path,
            last_health_check: None,
            last_heartbeat: None,
            kill_requested: false,
        }
    }

    fn retry_later(&mut self, now: Instant) {
        self.retry_at = now + Duration::from_millis((250_u64 << self.failures.min(7)).min(30_000));
        self.failures = self.failures.saturating_add(1);
    }

    fn tick(&mut self, now: Instant) {
        if let Some(mut child) = self.child.take() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    warn!(program = ?self.command.get_program(), %status, "Session process exited; scheduling restart");
                    if now.duration_since(self.started_at) >= Duration::from_secs(60) {
                        self.failures = 0;
                    }
                    self.kill_requested = false;
                    self.last_heartbeat = None;
                    self.last_health_check = None;
                    self.retry_later(now);
                }
                Ok(None) => {
                    if !self.kill_requested && self.heartbeat_is_stale(now) {
                        warn!(
                            program = ?self.command.get_program(),
                            "Session UI stopped reporting progress; terminating it for restart"
                        );
                        if let Err(error) = child.kill() {
                            warn!(%error, "Could not terminate stalled session process");
                        }
                        self.kill_requested = true;
                    }
                    self.child = Some(child);
                    return;
                }
                Err(error) => {

                    warn!(%error, "Could not reap session process");
                    self.child = Some(child);
                    return;
                }
            }
        }
        if now < self.retry_at {
            return;
        }
        if let Some(path) = self.heartbeat_path.as_ref() {
            let _ = std::fs::remove_file(path);
        }
        self.last_heartbeat = None;
        self.last_health_check = None;
        self.kill_requested = false;
        match self.command.spawn() {
            Ok(child) => {
                info!(program = ?self.command.get_program(), pid = child.id(), "Started session process");
                self.started_at = now;
                self.child = Some(child);
            }
            Err(error) => {
                warn!(program = ?self.command.get_program(), %error, "Could not start session process");
                self.retry_later(now);
            }
        }
    }

    fn heartbeat_is_stale(&mut self, now: Instant) -> bool {
        let Some(path) = self.heartbeat_path.as_ref() else {
            return false;
        };
        if self
            .last_health_check
            .is_some_and(|last| now.duration_since(last) < Duration::from_secs(1))
        {
            return false;
        }
        self.last_health_check = Some(now);

        if let Ok(value) = std::fs::read_to_string(path) {
            if let Ok(timestamp) = value.trim().parse::<u64>() {
                self.last_heartbeat = Some(timestamp);
            }
        }
        let now_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if let Some(last_heartbeat) = self.last_heartbeat {
            now_epoch >= last_heartbeat
                && Duration::from_secs(now_epoch - last_heartbeat) >= SHELL_HEARTBEAT_TIMEOUT
        } else {
            now.duration_since(self.started_at) >= SHELL_STARTUP_GRACE
        }
    }
}

struct PanelProcess {
    target: rix_config::PanelTarget,
    process: SupervisedProcess,
}

#[derive(Default)]
pub struct AutostartManager {
    children: Vec<Child>,
    shells: Vec<PanelProcess>,
    services: Vec<SupervisedProcess>,
    locale_updates: Option<std::sync::mpsc::Sender<(String, String)>>,
    cursor: Option<(String, u32)>,
}

impl AutostartManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_cursor_environment(&mut self, theme: &str, size: u32) {
        self.cursor = Some((theme.to_string(), size));
        for shell in &mut self.shells {
            apply_cursor(self.cursor.as_ref(), &mut shell.process.command);
        }
        for service in &mut self.services {
            apply_cursor(self.cursor.as_ref(), &mut service.command);
        }
    }

    fn apply_cursor_env(&self, command: &mut Command) {
        apply_cursor(self.cursor.as_ref(), command);
    }

    pub fn spawn_shells(
        &mut self,
        socket: &str,
        native: bool,
        language: &str,
        targets: &[rix_config::PanelTarget],
    ) {
        publish_activation_environment(socket, language);
        self.sync_shells(socket, native, language, targets);
    }

    pub fn sync_shells(
        &mut self,
        socket: &str,
        native: bool,
        language: &str,
        targets: &[rix_config::PanelTarget],
    ) {
        let mut retained = Vec::new();
        for mut shell in self.shells.drain(..) {
            if targets.contains(&shell.target) {
                retained.push(shell);
            } else if let Some(mut child) = shell.process.child.take() {

                let _ = child.kill();
                self.children.push(child);
            }
        }
        self.shells = retained;
        for target in targets {
            if self.shells.iter().any(|s| &s.target == target) {
                continue;
            }
            let mut command = Command::new(find_binary("rix-shell"));
            command.args(["--panel", &target.panel, "--output", &target.output]);
            if native {
                command.env("RIX_NATIVE_SESSION", "1");
            } else {
                command.env_remove("RIX_NATIVE_SESSION");
            }
            command
                .env("WAYLAND_DISPLAY", socket)
                .env("XDG_SESSION_DESKTOP", "RIX")
                .env("XDG_SESSION_TYPE", "wayland")
                .env_remove("WAYLAND_SOCKET")
                .stdin(std::process::Stdio::null());
            self.apply_cursor_env(&mut command);
            rix_services::locale::apply_to_command(&mut command, language);
            let heartbeat_path = std::env::var_os("RIX_COMPOSITOR_HEARTBEAT").map(|path| {
                use std::hash::{Hash, Hasher};
                let mut key = std::collections::hash_map::DefaultHasher::new();
                target.hash(&mut key);
                PathBuf::from(path).with_file_name(format!(
                    "shell-{}-{:016x}.heartbeat",
                    std::process::id(),
                    key.finish()
                ))
            });
            if let Some(path) = &heartbeat_path {
                command.env("RIX_SHELL_HEARTBEAT", path);
            }
            let mut process = SupervisedProcess::with_heartbeat(command, heartbeat_path);
            process.tick(Instant::now());
            self.shells.push(PanelProcess {
                target: target.clone(),
                process,
            });
        }
    }

    pub fn spawn_session_apps(
        &mut self,
        config_dir: &Path,
        socket: &str,
        native: bool,
        language: &str,
    ) {
        if !native && std::env::var("RIX_AUTOSTART").as_deref() != Ok("1") {
            return;
        }
        let store = rix_services::autostart::AutostartStore::new(config_dir);
        match store.list() {
            Ok(entries) => {
                for entry in entries.into_iter().filter(|e| e.enabled && e.available) {
                    let launched =
                        rix_services::autostart::AutostartStore::launch_command(&entry, socket)
                            .and_then(|mut cmd| {
                                self.apply_cursor_env(&mut cmd);
                                rix_services::locale::apply_to_command(&mut cmd, language);
                                cmd.spawn().map_err(Into::into)
                            });
                    match launched {
                        Ok(child) => self.children.push(child),
                        Err(error) => tracing::warn!(id = %entry.id, %error, "Autostart failed"),
                    }
                }
            }
            Err(error) => tracing::warn!(%error, "Could not load autostart"),
        }
    }

    pub fn reap(&mut self) {
        for shell in &mut self.shells {
            shell.process.tick(Instant::now());
        }
        for service in &mut self.services {
            service.tick(Instant::now());
        }
        self.children
            .retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
    }

    pub fn shutdown(&mut self) {
        for mut service in self.services.drain(..) {
            if let Some(child) = service.child.take() {
                self.children.push(child);
            }
        }
        for mut shell in self.shells.drain(..) {
            if let Some(child) = shell.process.child.take() {
                self.children.push(child);
            }
        }

        self.children
            .retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
        for child in &self.children {

            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self.children.is_empty() && Instant::now() < deadline {
            self.children
                .retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
            if !self.children.is_empty() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        for mut child in self.children.drain(..) {
            info!("Stopping background process PID {}", child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub fn update_language(&mut self, socket: &str, language: &str) {
        for shell in &mut self.shells {
            rix_services::locale::apply_to_command(&mut shell.process.command, language);
        }
        for service in &mut self.services {
            rix_services::locale::apply_to_command(&mut service.command, language);
        }
        let sender = self.locale_updates.get_or_insert_with(|| {
            let (sender, receiver) = std::sync::mpsc::channel::<(String, String)>();
            std::thread::spawn(move || {
                while let Ok((socket, language)) = receiver.recv() {
                    publish_activation_environment(&socket, &language);
                }
            });
            sender
        });
        if sender
            .send((socket.to_string(), language.to_string()))
            .is_err()
        {
            self.locale_updates = None;
            warn!("Could not queue the session language update");
        }
    }
}

impl AutostartManager {

    pub fn spawn_native_services(
        &mut self,
        socket: &str,
        settings: &rix_config::desktop::SessionSettings,
        language: &str,
    ) {
        let authentication = if settings.authentication_command.is_empty() {
            [
                "lxqt-policykit-agent",
                "/usr/lib/polkit-gnome/polkit-gnome-authentication-agent-1",
                "/usr/libexec/polkit-gnome-authentication-agent-1",
                "/usr/lib/polkit-kde-authentication-agent-1",
            ]
            .into_iter()
            .find(|candidate| executable(candidate))
            .map(|candidate| vec![candidate.to_string()])
            .unwrap_or_default()
        } else {
            settings.authentication_command.clone()
        };
        for (enabled, name, argv) in [
            (
                settings.notifications,
                "notifications",
                &settings.notification_command,
            ),
            (
                settings.authentication_agent,
                "authentication",
                &authentication,
            ),
        ] {
            if !enabled {
                continue;
            }
            let Some(binary) = argv.first().filter(|binary| executable(binary)) else {
                warn!(
                    service = name,
                    "Session component is not installed; run rix-doctor"
                );
                continue;
            };
            let mut command = Command::new(binary);
            command
                .args(&argv[1..])
                .env("WAYLAND_DISPLAY", socket)
                .env_remove("WAYLAND_SOCKET")
                .stdin(std::process::Stdio::null());
            self.apply_cursor_env(&mut command);
            rix_services::locale::apply_to_command(&mut command, language);
            let mut process = SupervisedProcess::new(command);
            process.tick(Instant::now());
            self.services.push(process);
        }
    }
}

fn apply_cursor(cursor: Option<&(String, u32)>, command: &mut Command) {
    if let Some((theme, size)) = cursor {
        rix_services::resources::apply_cursor_environment(command, theme, *size);
    }
}

fn executable(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let check = |path: &Path| {
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if name.contains('/') {
        check(Path::new(name))
    } else {
        std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| check(&dir.join(name))))
    }
}

fn publish_activation_environment(socket: &str, language: &str) {
    if std::env::var("RIX_SESSION_BUS_ISOLATED").as_deref() != Ok("1") {
        return;
    }
    let mut command = Command::new("dbus-update-activation-environment");
    command.env("WAYLAND_DISPLAY", socket);
    let mut variables = vec![format!("WAYLAND_DISPLAY={socket}")];
    variables.extend(
        [
            "XDG_CURRENT_DESKTOP",
            "KDE_SESSION_VERSION",
            "XDG_SESSION_DESKTOP",
            "XDG_SESSION_TYPE",
            "XDG_DATA_DIRS",
            "PATH",
        ]
        .into_iter()
        .map(str::to_string),
    );
    for (name, value) in rix_services::locale::activation_environment(language) {
        command.env(&name, &value);
        variables.push(format!("{name}={value}"));
    }
    let child = command
        .args(variables)
        .stdin(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        warn!("Could not update session bus activation environment");
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    warn!(%status, "Activation environment update failed");
                }
                break;
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                warn!("Activation environment update timed out");
                break;
            }
        }
    }
}

impl Drop for AutostartManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn find_binary(name: &str) -> PathBuf {

    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            let candidate = parent.join(name);
            if candidate.exists() {
                return candidate;
            }
        }
    }

    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crashed_shell_recovers_without_spawning_during_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("starts");
        let mut command = Command::new("sh");
        command
            .args(["-c", "echo started >> \"$1\"; exit 7", "test"])
            .arg(&log);
        let mut process = SupervisedProcess::new(command);
        let now = Instant::now();
        process.tick(now);
        process.child.as_mut().unwrap().wait().unwrap();
        process.tick(now);
        assert!(process.child.is_none());
        process.tick(now + Duration::from_millis(249));
        assert!(process.child.is_none());
        process.tick(now + Duration::from_millis(250));
        process.child.as_mut().unwrap().wait().unwrap();
        assert_eq!(std::fs::read_to_string(log).unwrap(), "started\nstarted\n");
    }

    #[test]
    fn stalled_shell_is_terminated_without_blocking_the_compositor_tick() {
        let directory = tempfile::tempdir().unwrap();
        let heartbeat = directory.path().join("shell.heartbeat");
        let stale = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(30);

        let mut command = Command::new("sleep");
        command.arg("60");
        let mut process = SupervisedProcess::with_heartbeat(command, Some(heartbeat.clone()));
        let now = Instant::now();
        process.tick(now);
        let pid = process.child.as_ref().unwrap().id();
        std::fs::write(&heartbeat, format!("{stale}\n")).unwrap();

        process.tick(now + Duration::from_secs(1));
        assert!(process.kill_requested);

        let deadline = Instant::now() + Duration::from_secs(2);
        while process.child.is_some() && Instant::now() < deadline {
            process.tick(Instant::now());
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            process.child.is_none(),
            "stalled shell child was not reaped"
        );
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    }

    #[test]
    fn removing_panel_does_not_restart_other_panel_or_block_reaping() {
        let target = |panel: &str| rix_config::PanelTarget {
            panel: panel.into(),
            output: "test-output".into(),
        };
        let process = |panel: &str| {
            let mut process = SupervisedProcess::new(Command::new("sleep"));
            process.command.arg("60");
            process.tick(Instant::now());
            PanelProcess {
                target: target(panel),
                process,
            }
        };
        let mut manager = AutostartManager::default();
        manager.shells = vec![process("first"), process("second")];
        let retained = manager.shells[0].process.child.as_ref().unwrap().id();
        let removed = manager.shells[1].process.child.as_ref().unwrap().id();
        manager.sync_shells("test", false, "", &[target("first")]);
        assert_eq!(manager.shells.len(), 1);
        assert_eq!(
            manager.shells[0].process.child.as_ref().unwrap().id(),
            retained
        );
        assert_eq!(manager.children.len(), 1);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !manager.children.is_empty() && Instant::now() < deadline {
            manager.reap();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(manager.children.is_empty());
        assert_eq!(unsafe { libc::kill(removed as i32, 0) }, -1);
        manager.shutdown();
    }

    #[test]
    fn shutdown_reaps_shell_and_disables_restart() {
        let mut process = SupervisedProcess::new(Command::new("sleep"));
        process.command.arg("60");
        process.tick(Instant::now());
        let pid = process.child.as_ref().unwrap().id();
        let mut manager = AutostartManager {
            shells: vec![PanelProcess {
                target: rix_config::PanelTarget {
                    panel: "test".into(),
                    output: "test-output".into(),
                },
                process,
            }],
            children: Vec::new(),
            services: Vec::new(),
            locale_updates: None,
            cursor: None,
        };
        manager.shutdown();
        manager.reap();
        assert!(manager.shells.is_empty());
        assert!(manager.children.is_empty());

        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    }
}
