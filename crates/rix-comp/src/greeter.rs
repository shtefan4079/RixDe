use crate::state::{ClientState, RixState};
use std::{
    os::{
        fd::AsRawFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

pub(crate) fn validate_command(command: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    anyhow::ensure!(
        command.is_absolute(),
        "--greeter-command must be an absolute executable path"
    );
    let metadata = command.metadata()?;
    anyhow::ensure!(
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
        "--greeter-command must name an executable file"
    );
    Ok(())
}

pub(crate) struct GreeterProcess {
    child: Child,
    group: i32,
    exited: bool,
}
impl GreeterProcess {
    pub(crate) fn spawn(command: &Path, state: &mut RixState) -> anyhow::Result<Self> {
        anyhow::ensure!(
            state.greeter_mode,
            "Private greeter startup requires restricted compositor mode"
        );
        validate_command(command)?;
        let (server, client_socket) = UnixStream::pair()?;
        let client = state
            .display_handle
            .insert_client(server, Arc::new(ClientState::default()))?;
        let fd = client_socket.as_raw_fd();
        let mut command = Command::new(command);
        command
            .env("WAYLAND_SOCKET", fd.to_string())
            .env("XDG_SESSION_TYPE", "wayland")
            .env("RIX_GREETER", "1")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .env_remove("RIX_AUTOSTART")
            .env_remove("RIX_COMPOSITOR_HEARTBEAT")
            .env_remove("RIX_SHELL_HEARTBEAT")
            .env_remove("RIX_SESSION_BUS_ISOLATED")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdin(Stdio::null());
        let parent_pid = std::process::id() as libc::pid_t;

        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, 0) == -1
                    || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent_pid {
                    return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
                }
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags == -1 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                state.display_handle.backend_handle().kill_client(
                    client.id(),
                    smithay::reexports::wayland_server::backend::DisconnectReason::ConnectionClosed,
                );
                return Err(error.into());
            }
        };
        let group = child.id() as i32;
        state.clients.push(client);
        Ok(Self {
            child,
            group,
            exited: false,
        })
    }

    pub(crate) fn tick(&mut self, state: &mut RixState) -> anyhow::Result<Option<ExitStatus>> {
        if self.exited {
            return Ok(None);
        }

        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
        if unsafe { info.si_pid() } != 0 {
            use std::os::unix::process::ExitStatusExt;
            let raw = if info.si_code == libc::CLD_EXITED {
                (unsafe { info.si_status() }) << 8
            } else {
                unsafe { info.si_status() }
            };
            let status = ExitStatus::from_raw(raw);
            self.exited = true;
            tracing::info!(%status, "Greeter exited; retiring login compositor");
            state.running.store(false, Ordering::SeqCst);
            Ok(Some(status))
        } else {
            Ok(None)
        }
    }
}
impl Drop for GreeterProcess {
    fn drop(&mut self) {

        unsafe {
            libc::kill(-self.group, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_millis(400);
        while !self.exited && Instant::now() < deadline {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == -1 {
                break;
            }
            self.exited = unsafe { info.si_pid() } != 0;
            if !self.exited {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        unsafe {
            libc::kill(-self.group, libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

#[derive(Clone)]
pub(crate) struct BackendConfig {
    desktop: Option<Arc<rix_config::ConfigManager>>,
}
impl BackendConfig {
    pub(crate) fn new(greeter: bool) -> anyhow::Result<Self> {
        Ok(Self {
            desktop: if greeter {
                None
            } else {
                Some(Arc::new(rix_config::ConfigManager::init()?))
            },
        })
    }
    pub(crate) fn desktop(&self) -> Option<&Arc<rix_config::ConfigManager>> {
        self.desktop.as_ref()
    }
    pub(crate) fn get_theme(&self) -> rix_config::ThemeSettings {
        self.desktop
            .as_ref()
            .map_or_else(Default::default, |c| c.get_theme())
    }
    pub(crate) fn get_layout(&self) -> rix_config::LayoutConfig {
        self.desktop
            .as_ref()
            .map_or_else(rix_config::LayoutConfig::default_layout, |c| c.get_layout())
    }
    pub(crate) fn get_display(&self) -> rix_config::DisplaySettings {
        self.desktop
            .as_ref()
            .map_or_else(Default::default, |c| c.get_display())
    }
    pub(crate) fn get_desktop(&self) -> rix_config::DesktopSettings {
        let Some(config) = &self.desktop else {
            return rix_config::DesktopSettings {
                server_side_decorations: false,
                ..Default::default()
            };
        };
        config.get_desktop()
    }
    pub(crate) fn get_effects(&self) -> rix_config::appearance::MaterialEffects {
        rix_config::appearance::MaterialEffects::from(&self.get_theme())
    }
    pub(crate) fn subscribe(&self) -> tokio::sync::broadcast::Receiver<rix_config::LayoutConfig> {
        self.desktop
            .as_ref()
            .map_or_else(|| tokio::sync::broadcast::channel(1).1, |c| c.subscribe())
    }
    pub(crate) fn subscribe_theme(
        &self,
    ) -> tokio::sync::broadcast::Receiver<rix_config::ThemeSettings> {
        self.desktop.as_ref().map_or_else(
            || tokio::sync::broadcast::channel(1).1,
            |c| c.subscribe_theme(),
        )
    }
    pub(crate) fn subscribe_desktop(
        &self,
    ) -> tokio::sync::broadcast::Receiver<rix_config::DesktopSettings> {
        self.desktop.as_ref().map_or_else(
            || tokio::sync::broadcast::channel(1).1,
            |c| c.subscribe_desktop(),
        )
    }
    pub(crate) fn save_display(&self, display: rix_config::DisplaySettings) -> anyhow::Result<()> {
        if let Some(config) = &self.desktop {
            config.save_display(display)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use wayland_client::{
        globals::{registry_queue_init, GlobalListContents},
        protocol::wl_registry,
        Connection, Dispatch, QueueHandle,
    };

    struct Events;
    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Events {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    #[test]
    fn restricted_registry_excludes_capture_selection_lock_and_desktop_observers() {
        let (server_socket, socket) = UnixStream::pair().unwrap();
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let running_server = running.clone();
        let server = std::thread::spawn(move || {
            let mut display = wayland_server::Display::<RixState>::new().unwrap();
            let mut state = RixState::new_with_mode(&mut display, String::new(), true);

            let client = display
                .handle()
                .insert_client(
                    server_socket,
                    Arc::new(ClientState {
                        allow_capture: true,
                        ..Default::default()
                    }),
                )
                .unwrap();
            state.clients.push(client);
            let deadline = Instant::now() + Duration::from_secs(4);
            while running_server.load(Ordering::SeqCst) && Instant::now() < deadline {
                display.dispatch_clients(&mut state).unwrap();
                display.flush_clients().unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let connection = Connection::from_socket(socket).unwrap();
        let (globals, _) = registry_queue_init::<Events>(&connection).unwrap();
        let names: Vec<_> = globals
            .contents()
            .with_list(|list| list.iter().map(|global| global.interface.clone()).collect());
        running.store(false, Ordering::SeqCst);
        server.join().unwrap();
        assert!(names.iter().any(|name| name == "wl_compositor"));
        assert!(names.iter().any(|name| name == "xdg_wm_base"));
        for forbidden in [
            "wl_data_device_manager",
            "zwp_primary_selection_device_manager_v1",
            "ext_data_control_manager_v1",
            "ext_session_lock_manager_v1",
            "zwlr_screencopy_manager_v1",
            "ext_foreign_toplevel_list_v1",
            "ext_image_copy_capture_manager_v1",
            "ext_foreign_toplevel_image_capture_source_manager_v1",
            "rix_capture_policy_manager_v1",
            "rix_toplevel_observer_manager_v1",
            "zwp_idle_inhibit_manager_v1",
            "zxdg_exporter_v2",
            "zxdg_importer_v2",
            "xdg_wm_dialog_v1",
        ] {
            assert!(
                !names.iter().any(|name| name == forbidden),
                "restricted greeter exposed {forbidden}"
            );
        }
    }

    #[test]
    fn greeter_keyboard_never_requests_desktop_actions() {
        use smithay::{backend::input::KeyState, utils::SERIAL_COUNTER};
        let mut display = wayland_server::Display::<RixState>::new().unwrap();
        let mut state = RixState::new_with_mode(&mut display, String::new(), true);
        state
            .seat
            .add_keyboard(Default::default(), 200, 30)
            .unwrap();
        let (sender, mut events) = tokio::sync::broadcast::channel(16);
        state.ipc_event_sender = Some(sender);
        for code in [133u32, 46, 107, 64, 70, 23] {
            for key_state in [KeyState::Pressed, KeyState::Released] {
                state.handle_keyboard_key(
                    code.into(),
                    key_state,
                    SERIAL_COUNTER.next_serial(),
                    1,
                    true,
                );
            }
        }
        assert!(!state.lock.active && !state.lock.requested);
        assert!(state.screenshot_rx.try_recv().is_err());
        assert!(state.desktop_rx.try_recv().is_err());
        assert!(!state.switcher.active());
        assert!(events.try_recv().is_err());
        let mut keyboard = BackendConfig::new(true).unwrap().get_layout().keyboard;
        keyboard.layouts.push(rix_config::KeyboardLayout {
            layout: "de".into(),
            variant: String::new(),
            label: "DE".into(),
        });
        state.configure_keyboard(keyboard).unwrap();
        for code in [64u32, 50] {
            state.handle_keyboard_key(
                code.into(),
                KeyState::Pressed,
                SERIAL_COUNTER.next_serial(),
                1,
                true,
            );
        }
        assert_eq!(
            state.keyboard_state().active,
            1,
            "Alt+Shift must switch the greeter's real XKB layout"
        );
    }

    #[test]
    fn greeter_config_is_static_and_never_initializes_a_desktop_manager() {
        let config = BackendConfig::new(true).unwrap();
        assert!(config.desktop().is_none());
        assert!(!config.get_desktop().server_side_decorations);
        assert!(matches!(
            config.subscribe().try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Closed)
        ));
        assert!(config.get_desktop().enabled_plugins.is_empty());
        let keyboard = config.get_layout().keyboard;
        assert_eq!(
            keyboard
                .layouts
                .iter()
                .map(|layout| layout.layout.as_str())
                .collect::<Vec<_>>(),
            ["us"]
        );
        assert_eq!(keyboard.switch_option, "grp:alt_shift_toggle");
    }

    #[test]
    fn greeter_rescue_vt_shortcut_is_available_only_on_a_native_seat() {
        use smithay::{backend::input::KeyState, utils::SERIAL_COUNTER};
        for native in [false, true] {
            let mut display = wayland_server::Display::<RixState>::new().unwrap();
            let mut state = RixState::new_with_mode(&mut display, String::new(), true);
            state.native_session = native;
            state
                .seat
                .add_keyboard(Default::default(), 200, 30)
                .unwrap();

            for code in [37u32, 64, 68] {
                state.handle_keyboard_key(
                    code.into(),
                    KeyState::Pressed,
                    SERIAL_COUNTER.next_serial(),
                    1,
                    true,
                );
            }
            assert_eq!(state.vt_switch, native.then_some(2));
            assert!(!state.lock.requested);
            assert!(state.screenshot_rx.try_recv().is_err());
        }
    }

    #[test]
    fn private_child_inherits_a_socket_and_stops_compositor_without_respawn() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("fake-greeter");
        let evidence = dir.path().join("socket");
        std::fs::write(&executable, format!(
            "#!/bin/sh\n[ -n \"$WAYLAND_SOCKET\" ] || exit 90\n[ -z \"$WAYLAND_DISPLAY\" ] || exit 91\n[ -z \"$DISPLAY\" ] || exit 92\nreadlink /proc/self/fd/\"$WAYLAND_SOCKET\" > '{}'\nexit 7\n", evidence.display())).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut display = wayland_server::Display::<RixState>::new().unwrap();
        let mut state = RixState::new_with_mode(&mut display, String::new(), true);
        let mut process = GreeterProcess::spawn(&executable, &mut state).unwrap();
        let pid = process.child.id();
        let deadline = Instant::now() + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = process.tick(&mut state).unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "fake greeter did not exit");
            std::thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(status.code(), Some(7));
        assert!(!state.running.load(Ordering::SeqCst));
        assert_eq!(state.clients.len(), 1);
        assert!(std::fs::read_to_string(evidence)
            .unwrap()
            .starts_with("socket:["));
        assert!(process.tick(&mut state).unwrap().is_none());
        assert_eq!(process.child.id(), pid);
        drop(process);
        assert_eq!(
            unsafe { libc::kill(pid as i32, 0) },
            -1,
            "child was not reaped"
        );
    }

    #[test]
    fn greeter_arguments_require_an_absolute_command_and_private_socket() {
        use clap::Parser;
        assert!(crate::Args::try_parse_from(["rix-comp", "--greeter"]).is_err());
        assert!(
            crate::Args::try_parse_from(["rix-comp", "--greeter-command", "/bin/true"]).is_err()
        );
        assert!(crate::Args::try_parse_from([
            "rix-comp",
            "--greeter",
            "--greeter-command",
            "/bin/true",
            "--socket",
            "public"
        ])
        .is_err());
        assert!(crate::Args::try_parse_from([
            "rix-comp",
            "--greeter",
            "--greeter-command",
            "/bin/true",
            "--nested"
        ])
        .is_ok());
        assert!(validate_command(Path::new("rix-greeter")).is_err());
    }

    #[test]
    fn retiring_exited_greeter_terminates_its_remaining_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("forking-greeter");
        let child_record = dir.path().join("child-pid");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nsleep 60 &\necho $! > '{}'\nexit 0\n",
                child_record.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut display = wayland_server::Display::<RixState>::new().unwrap();
        let mut state = RixState::new_with_mode(&mut display, String::new(), true);
        let mut process = GreeterProcess::spawn(&executable, &mut state).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if process.tick(&mut state).unwrap().is_some() {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        let descendant: i32 = std::fs::read_to_string(child_record)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(descendant, 0) }, 0);
        drop(process);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let status = std::fs::read_to_string(format!("/proc/{descendant}/status"));
            if status.is_err()
                || status.as_ref().is_ok_and(|status| {
                    status.lines().any(|line| {
                        line.starts_with("State:") && (line.contains('Z') || line.contains('X'))
                    })
                })
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "greeter descendant survived process-group cleanup"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
