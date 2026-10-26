use crate::state::RixState;
use smithay::{
    output::Output,
    utils::{Logical, Point, SERIAL_COUNTER},
    wayland::session_lock::{
        ExtLockSurfaceUserData, LockSurface, SessionLockHandler, SessionLockManagerGlobalData,
        SessionLockManagerState, SessionLockState, SessionLocker,
    },
};
use std::{
    collections::HashSet,
    os::{
        fd::AsRawFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    sync::Arc,
    time::{Duration, Instant},
};
use wayland_protocols::ext::session_lock::v1::server::{
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_surface_v1::ExtSessionLockSurfaceV1,
    ext_session_lock_v1::{self, ExtSessionLockV1},
};
use wayland_server::{
    protocol::{wl_output::WlOutput, wl_surface::WlSurface},
    Client, DataInit, Dispatch, DisplayHandle, Resource,
};

#[derive(Default)]
pub(crate) struct Lock {
    pub requested: bool,
    pub active: bool,
    pub generation: u64,
    owner: Option<ExtSessionLockV1>,
    pending: Option<SessionLocker>,
    waiting: HashSet<String>,
    outputs: HashSet<String>,
    pub surfaces: Vec<(Output, LockSurface)>,
    previous_focus: Option<WlSurface>,
    confirmed: bool,
    child: Option<std::process::Child>,
    managed_client: Option<Client>,
    retry_at: Option<Instant>,
    failures: u32,
    pub logind_hint: Option<tokio::sync::watch::Sender<bool>>,
    requests: Vec<tokio::sync::oneshot::Sender<Result<rix_ipc::IpcResponse, String>>>,
}

impl RixState {
    pub(crate) fn restore_supervisor_lock(&mut self, locked: bool) {
        if locked && !self.lock.active {
            self.begin_lock();
        }
    }

    pub(crate) fn launch_locker(
        &mut self,
        config: &rix_config::ConfigManager,
    ) -> Result<(), String> {
        if self.lock.owner.as_ref().is_some_and(Resource::is_alive) || self.lock.child.is_some() {
            return Ok(());
        }

        if self.native_session && !self.lock.active {
            self.begin_lock();
        }
        let settings = config.get_desktop().session;
        let Some(binary) = settings.lock_command.first() else {
            return Err("Screen locker is not configured".into());
        };

        let (server, child_socket) = UnixStream::pair().map_err(|e| e.to_string())?;
        let client = self
            .display_handle
            .clone()
            .insert_client(server, Arc::new(crate::state::ClientState::default()))
            .map_err(|e| e.to_string())?;
        let fd = child_socket.as_raw_fd();
        let mut command = std::process::Command::new(binary);
        command
            .args(&settings.lock_command[1..])
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env("WAYLAND_SOCKET", fd.to_string())
            .stdin(std::process::Stdio::null());

        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .map_err(|error| format!("Could not start the lock screen ({binary}): {error}"))?;
        self.lock.child = Some(child);
        self.lock.managed_client = Some(client);
        self.lock.retry_at = None;

        if !self.lock.active {
            self.begin_lock();
        }
        Ok(())
    }

    pub(crate) fn request_lock(
        &mut self,
        config: &rix_config::ConfigManager,
        reply: tokio::sync::oneshot::Sender<Result<rix_ipc::IpcResponse, String>>,
    ) {
        if self.lock.confirmed {
            let _ = reply.send(Ok(rix_ipc::IpcResponse::Ok));
        } else if let Err(error) = self.launch_locker(config) {
            let _ = reply.send(Err(error));
        } else {
            self.lock.requests.push(reply);
        }
    }

    pub(crate) fn tick_locker(&mut self, config: &rix_config::ConfigManager) {
        self.lock.requests.retain(|reply| !reply.is_closed());
        if let Some(child) = self.lock.child.as_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                self.lock.child = None;
                self.lock.managed_client = None;
                for reply in self.lock.requests.drain(..) {
                    let _ = reply.send(Err(format!("The lock screen exited: {status}")));
                }
            }
        }
        if self.lock.active
            && self.lock.child.is_none()
            && self
                .lock
                .owner
                .as_ref()
                .is_none_or(|owner| !owner.is_alive())
        {
            let now = Instant::now();
            if self.lock.retry_at.is_none() {
                self.lock.failures = self.lock.failures.saturating_add(1);
                self.lock.retry_at = Some(
                    now + Duration::from_millis((250u64 << self.lock.failures.min(7)).min(30_000)),
                );
                tracing::warn!("Locker disconnected; keeping session locked while restarting it");
            }
            if self.lock.retry_at.is_some_and(|deadline| now >= deadline) {
                if let Err(error) = self.launch_locker(config) {
                    tracing::error!(%error, "Could not restart screen locker");
                    self.lock.retry_at = None;
                }
            }
        }
    }
    pub(crate) fn lock_surface_at(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.lock.surfaces.iter().find_map(|(output, surface)| {
            let geometry = self.space.output_geometry(output)?;
            (surface.alive() && geometry.to_f64().contains(pos))
                .then(|| (surface.wl_surface().clone(), geometry.loc.to_f64()))
        })
    }

    pub(crate) fn focus_lock(&mut self, surface: Option<WlSurface>) {
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, surface, SERIAL_COUNTER.next_serial());
        }
    }

    pub(crate) fn lock_frame_presented(&mut self, output: &Output, generation: u64) {
        if self.lock.active && self.lock.generation == generation {
            self.lock.waiting.remove(&output.name());
            self.confirm_lock_if_ready();
        }
    }

    pub(crate) fn sync_lock_outputs(&mut self) {
        if !self.lock.active {
            return;
        }
        let outputs: Vec<_> = self.space.outputs().cloned().collect();
        self.lock
            .outputs
            .retain(|name| outputs.iter().any(|o| o.name() == *name));
        for output in &outputs {
            if self.lock.outputs.insert(output.name()) {

                self.lock.waiting.insert(output.name());
            }
        }
        self.lock
            .waiting
            .retain(|name| outputs.iter().any(|o| o.name() == *name));
        self.lock
            .surfaces
            .retain(|(o, s)| outputs.contains(o) && s.alive());
        for (output, surface) in &self.lock.surfaces {
            if let Some(geometry) = self.space.output_geometry(output) {
                surface.with_pending_state(|pending| {
                    pending.size = Some((geometry.size.w as u32, geometry.size.h as u32).into())
                });
                surface.send_configure();
            }
        }
        self.confirm_lock_if_ready();
    }

    fn confirm_lock_if_ready(&mut self) {
        if self.lock.waiting.is_empty() {
            if let Some(hint) = &self.lock.logind_hint {
                hint.send_if_modified(|locked| {
                    if *locked {
                        false
                    } else {
                        *locked = true;
                        true
                    }
                });
            }
            if let Some(confirmation) = self.lock.pending.take() {
                confirmation.lock();
                self.lock.confirmed = true;
                for reply in self.lock.requests.drain(..) {
                    let _ = reply.send(Ok(rix_ipc::IpcResponse::Ok));
                }
            }
        }
    }

    fn begin_lock(&mut self) {
        self.lock.previous_focus = self.seat.get_keyboard().and_then(|k| k.current_focus());
        crate::remote_input::revoke_all(
            self,
            rix_protocols::remote_input_server::rix_remote_input_session_v1::CloseReason::Locked,
        );
        self.cancel_input();
        self.lock.active = true;
        self.lock.generation = self.lock.generation.wrapping_add(1);
        self.lock.waiting = self.space.outputs().map(|o| o.name()).collect();
        self.lock.outputs = self.lock.waiting.clone();
        self.lock.confirmed = false;
        self.cursor_status = smithay::input::pointer::CursorImageStatus::default_named();
        if let Some(pointer) = self.seat.get_pointer() {
            pointer.motion(
                self,
                None,
                &smithay::input::pointer::MotionEvent {
                    location: pointer.current_location(),
                    serial: SERIAL_COUNTER.next_serial(),
                    time: self.start_time.elapsed().as_millis() as u32,
                },
            );
            pointer.frame(self);
        }
        self.previews.cancel("Session is locked");
        self.screenshot_writer.cancel("Session is locked");
        self.screencopy.cancel_pending();
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl SessionLockHandler for RixState {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        let recovery = self.lock.active
            && self
                .lock
                .owner
                .as_ref()
                .is_none_or(|owner| !owner.is_alive())
            && self.lock.managed_client.as_ref().is_some_and(|client| {
                confirmation.ext_session_lock().client().as_ref() == Some(client)
            });
        if self.lock.active && !recovery {
            return;
        }
        if !recovery {
            self.begin_lock();
        } else {
            self.cancel_input();
        }
        self.lock.surfaces.clear();
        self.lock.owner = Some(confirmation.ext_session_lock().clone());
        self.lock.pending = Some(confirmation);
        self.lock.confirmed = false;
        self.confirm_lock_if_ready();
    }

    fn unlock(&mut self) {

        if !self.lock.confirmed {
            return;
        }
        self.cancel_input();
        let focus = self.lock.previous_focus.take().filter(Resource::is_alive);
        self.lock.active = false;
        self.lock.confirmed = false;
        self.lock.owner = None;
        self.lock.pending = None;
        self.lock.surfaces.clear();
        self.lock.retry_at = None;
        self.lock.failures = 0;
        if let Some(hint) = &self.lock.logind_hint {
            hint.send_replace(false);
        }
        let fallback = self.space.elements().next_back().cloned();
        if focus.is_some() {
            self.focus_lock(focus);
        } else if let Some(window) = fallback {
            self.focus_window(&window);
        }
    }

    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        let Some(output) = Output::from_resource(&output) else {
            return;
        };
        let Some(geometry) = self.space.output_geometry(&output) else {
            return;
        };
        surface.with_pending_state(|pending| {
            pending.size = Some((geometry.size.w as u32, geometry.size.h as u32).into())
        });
        self.lock.surfaces.push((output, surface.clone()));
        self.focus_lock(Some(surface.wl_surface().clone()));
    }
}

impl Dispatch<ExtSessionLockV1, SessionLockState> for RixState {
    fn request(
        state: &mut Self,
        client: &Client,
        resource: &ExtSessionLockV1,
        request: ext_session_lock_v1::Request,
        data: &SessionLockState,
        display: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        let owner = state.lock.owner.as_ref() == Some(resource);
        match &request {
            ext_session_lock_v1::Request::UnlockAndDestroy if !owner || !state.lock.confirmed => {
                resource.post_error(
                    ext_session_lock_v1::Error::InvalidUnlock,
                    "Only the confirmed session lock can unlock",
                );
                return;
            }
            ext_session_lock_v1::Request::GetLockSurface { .. } if !owner => {
                resource.post_error(
                    ext_session_lock_v1::Error::InvalidUnlock,
                    "Session lock was not accepted",
                );
                return;
            }
            _ => {}
        }
        <SessionLockManagerState as Dispatch<ExtSessionLockV1, SessionLockState, RixState>>::request(
            state, client, resource, request, data, display, init);
    }
}
wayland_server::delegate_global_dispatch!(RixState: [ExtSessionLockManagerV1: SessionLockManagerGlobalData] => SessionLockManagerState);
wayland_server::delegate_dispatch!(RixState: [ExtSessionLockManagerV1: ()] => SessionLockManagerState);
wayland_server::delegate_dispatch!(RixState: [ExtSessionLockSurfaceV1: ExtLockSurfaceUserData] => SessionLockManagerState);

#[cfg(test)]
#[path = "session_lock_tests.rs"]
mod tests;
