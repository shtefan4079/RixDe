use crate::state::ClientState;
use rix_services::plugins::{Capability, Plugin};
use std::{
    collections::HashMap,
    os::{
        fd::AsRawFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    process::Child,
    sync::Arc,
    time::{Duration, Instant},
};
use wayland_server::{Client, DisplayHandle};
struct Running {
    child: Child,
    client: Client,
    stopping: Option<Instant>,
    started: Instant,
}
struct Slot {
    capture_fps: u32,
    plugin: Plugin,
    installed: bool,
    enabled: bool,
    running: Option<Running>,
    failures: u32,
    next_start: Instant,
    restart_suppressed: bool,
    start_blocked: bool,
    message: String,
}
impl Slot {
    fn reset_retry(&mut self, now: Instant) {
        self.failures = 0;
        self.next_start = now;
        self.start_blocked = false;
        if !self.restart_suppressed {
            self.message.clear();
        }
    }
    fn retry_after_failure(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        self.next_start = now + retry_backoff(self.failures);
    }
}
fn retry_backoff(failures: u32) -> Duration {
    Duration::from_secs(1 << failures.min(5))
}
#[derive(Default)]
pub struct PluginManager {
    slots: HashMap<String, Slot>,
    last_scan: Option<Instant>,
    discovery: Option<std::sync::mpsc::Receiver<Vec<Plugin>>>,
}
impl PluginManager {
    pub fn stop_capture(&mut self, display: &DisplayHandle) -> Vec<String> {
        self.slots
            .iter_mut()
            .filter_map(|(id, slot)| {
                if !slot
                    .plugin
                    .manifest
                    .capabilities
                    .contains(&Capability::ScreenCapture)
                {
                    return None;
                }
                stop(display, &mut slot.running);

                slot.restart_suppressed = true;
                slot.message = "Screen sharing stopped by the user".into();
                Some(id.clone())
            })
            .collect()
    }
    pub fn tick(
        &mut self,
        display: &DisplayHandle,
        socket: &str,
        settings: &rix_config::DesktopSettings,
    ) {
        let enabled = &settings.enabled_plugins;
        let now = Instant::now();
        if self.discovery.is_none()
            && self
                .last_scan
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
        {
            self.last_scan = Some(now);
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            match std::thread::Builder::new()
                .name("rix-plugin-catalog".into())
                .spawn(move || {
                    let _ = sender.send(rix_services::plugins::discover());
                }) {
                Ok(_) => self.discovery = Some(receiver),
                Err(error) => tracing::warn!(%error, "Could not scan extensions"),
            }
        }
        let discovered = self.discovery.as_ref().and_then(|r| match r.try_recv() {
            Ok(plugins) => Some(plugins),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Vec::new()),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
        });
        if let Some(discovered) = discovered {
            self.discovery = None;
            for plugin in &discovered {
                if !enabled.contains(&plugin.manifest.id) {
                    continue;
                }
                self.slots
                    .entry(plugin.manifest.id.clone())
                    .or_insert_with(|| Slot {
                        capture_fps: settings.screen_cast_fps,
                        plugin: plugin.clone(),
                        installed: true,
                        enabled: true,
                        running: None,
                        failures: 0,
                        next_start: now,
                        restart_suppressed: false,
                        start_blocked: false,
                        message: String::new(),
                    });
            }
            for (id, slot) in &mut self.slots {
                if let Some(plugin) = discovered.iter().find(|p| &p.manifest.id == id) {
                    if !slot.installed
                        || plugin.manifest != slot.plugin.manifest
                        || plugin.directory != slot.plugin.directory
                        || plugin.unavailable != slot.plugin.unavailable
                        || (slot.capture_fps != settings.screen_cast_fps
                            && plugin
                                .manifest
                                .capabilities
                                .contains(&Capability::ScreenCapture))
                    {
                        stop(display, &mut slot.running);
                        slot.plugin = plugin.clone();
                        slot.capture_fps = settings.screen_cast_fps;
                        slot.reset_retry(now);
                    }
                    slot.installed = true;
                } else {
                    slot.installed = false;
                    if !slot.restart_suppressed {
                        slot.message = "Plugin is missing or its manifest is invalid".into();
                    }
                    stop(display, &mut slot.running);
                }
            }
        }
        self.tick_running(display, socket, settings, now);
    }
    fn tick_running(
        &mut self,
        display: &DisplayHandle,
        socket: &str,
        settings: &rix_config::DesktopSettings,
        now: Instant,
    ) {
        let enabled = &settings.enabled_plugins;
        for (id, slot) in &mut self.slots {
            let was_enabled = slot.enabled;
            slot.enabled = enabled.contains(id);
            if slot.enabled && !was_enabled {

                slot.restart_suppressed = false;
                slot.reset_retry(now);
            }
            let wanted = slot.enabled && slot.installed;
            if !wanted {
                stop(display, &mut slot.running);
            }
            if let Some(running) = &mut slot.running {
                if running
                    .stopping
                    .is_some_and(|t| now.saturating_duration_since(t) >= Duration::from_secs(1))
                {
                    signal(running.child.id(), libc::SIGKILL);
                }
                if let Ok(Some(status)) = running.child.try_wait() {
                    revoke(display, &running.client);
                    let stopping = running.stopping.is_some();
                    slot.running = None;
                    if !stopping {
                        slot.retry_after_failure(now);
                        tracing::warn!(
                            plugin = %id,
                            %status,
                            retry_seconds = retry_backoff(slot.failures).as_secs(),
                            "Extension exited unexpectedly; restart scheduled"
                        );
                        if !slot.restart_suppressed {
                            slot.message = format!("Process exited: {status}");
                        }
                    }
                } else if running.stopping.is_none()
                    && now.saturating_duration_since(running.started) >= Duration::from_secs(30)
                {

                    slot.failures = 0;
                }
            }
            if wanted
                && slot.running.is_none()
                && !slot.restart_suppressed
                && !slot.start_blocked
                && now >= slot.next_start
            {
                if let Err(error) = start_preconditions(&slot.plugin) {
                    slot.message = error.to_string();
                    slot.start_blocked = true;
                    continue;
                }
                match start(&slot.plugin, display, socket, slot.capture_fps, now) {
                    Ok(running) => {
                        slot.running = Some(running);
                        slot.message.clear();
                    }
                    Err(error) => {
                        slot.message = error.to_string();
                        slot.retry_after_failure(now);
                        tracing::warn!(
                            plugin = %id,
                            %error,
                            retry_seconds = retry_backoff(slot.failures).as_secs(),
                            "Could not start extension; retry scheduled"
                        );
                    }
                }
            }
        }
        self.slots
            .retain(|id, slot| enabled.contains(id) || slot.running.is_some());
    }
    pub fn statuses(&self) -> Vec<rix_ipc::PluginStatus> {
        self.slots
            .iter()
            .map(|(id, slot)| rix_ipc::PluginStatus {
                id: id.clone(),
                running: slot.running.as_ref().is_some_and(|r| r.stopping.is_none()),
                message: slot.message.clone(),
            })
            .collect()
    }
}
impl Drop for PluginManager {
    fn drop(&mut self) {

        for slot in self.slots.values_mut() {
            if let Some(r) = &slot.running {
                signal(r.child.id(), libc::SIGTERM);
            }
        }
        let deadline = Instant::now() + Duration::from_millis(300);
        for slot in self.slots.values_mut() {
            if let Some(mut r) = slot.running.take() {
                while Instant::now() < deadline {
                    if r.child.try_wait().ok().flatten().is_some() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                if r.child.try_wait().ok().flatten().is_none() {
                    signal(r.child.id(), libc::SIGKILL);
                }
                let _ = r.child.wait();
            }
        }
    }
}
fn signal(pid: u32, signal: i32) {
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}
fn revoke(display: &DisplayHandle, client: &Client) {
    client.kill(
        display,
        wayland_server::backend::protocol::ProtocolError {
            code: 0,
            object_id: 1,
            object_interface: "wl_display".into(),
            message: "Rix extension stopped".into(),
        },
    );
}
fn stop(display: &DisplayHandle, running: &mut Option<Running>) {
    if let Some(r) = running {
        if r.stopping.is_none() {
            revoke(display, &r.client);
            signal(r.child.id(), libc::SIGTERM);
            r.stopping = Some(Instant::now());
        }
    }
}
fn start(
    plugin: &Plugin,
    display: &DisplayHandle,
    socket: &str,
    fps: u32,
    now: Instant,
) -> anyhow::Result<Running> {
    start_preconditions(plugin)?;
    let (server, client_socket) = UnixStream::pair()?;
    let raw = client_socket.as_raw_fd();
    let caps = &plugin.manifest.capabilities;
    let data = ClientState {
        allow_capture: caps.contains(&Capability::ScreenCapture),
        allow_remote_input: caps.contains(&Capability::RemoteInput),
        ..Default::default()
    };
    let mut handle = display.clone();
    let client = handle.insert_client(server, Arc::new(data))?;
    let mut command = plugin.command(false)?;
    command
        .env("WAYLAND_DISPLAY", socket)
        .env("WAYLAND_SOCKET", raw.to_string())
        .env_remove("DISPLAY")
        .env("XDG_SESSION_DESKTOP", "RIX")
        .env("XDG_SESSION_TYPE", "wayland")
        .env("RIX_PLUGIN_ID", &plugin.manifest.id)
        .env("RIX_PLUGIN_API", "1")
        .env("RIX_SCREENCAST_FPS", fps.to_string())
        .process_group(0);

    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(raw, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    rix_services::resources::ordinary_command(&mut command);
    match command.spawn() {
        Ok(child) => Ok(Running {
            child,
            client,
            stopping: None,
            started: now,
        }),
        Err(e) => {
            revoke(display, &client);
            Err(e.into())
        }
    }
}
fn start_preconditions(plugin: &Plugin) -> anyhow::Result<()> {
    anyhow::ensure!(
        plugin.unavailable.is_empty(),
        "Missing: {}",
        plugin.unavailable.join(", ")
    );
    anyhow::ensure!(
        !plugin.manifest.session_bus
            || std::env::var("RIX_SESSION_BUS_ISOLATED").as_deref() == Ok("1")
            || std::env::var("RIX_NATIVE_SESSION").as_deref() == Ok("1"),
        "This plugin requires a dedicated Rix session or --isolated mode"
    );
    Ok(())
}

#[cfg(test)]
#[path = "plugin_supervision_tests.rs"]
mod tests;
