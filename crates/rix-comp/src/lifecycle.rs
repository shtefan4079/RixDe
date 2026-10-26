use crate::state::RixState;
use smithay::{
    backend::renderer::utils::with_renderer_surface_state,
    desktop::Window,
    output::Output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Point, Rectangle, SERIAL_COUNTER},
    wayland::{
        compositor::{get_parent, is_sync_subsurface},
        shell::xdg::ToplevelSurface,
    },
};
use std::collections::HashSet;
#[derive(Default)]
pub(crate) struct WindowLifecycle {
    known: Vec<Window>,
    next_id: u64,
    initial_maximized: HashSet<WlSurface>,
}

impl RixState {

    pub(crate) fn request_initial_maximize(&mut self, surface: &ToplevelSurface, maximized: bool) {
        if maximized {
            self.windows
                .lifecycle
                .initial_maximized
                .insert(surface.wl_surface().clone());
        } else {
            self.windows
                .lifecycle
                .initial_maximized
                .remove(surface.wl_surface());
        }
        self.configure_initial_maximize(surface, maximized);
        if surface.is_initial_configure_sent() && surface.has_pending_changes() {
            surface.send_configure();
        }
    }

    fn configure_initial_maximize(&self, surface: &ToplevelSurface, maximized: bool) {
        let size = maximized.then(|| {
            let window = self.windows.lifecycle.known.iter()
                .find(|window| window.toplevel() == Some(surface))?;
            let output = self.initial_window_output(window)?;
            let mut target = self.work_area(&output);
            let server_frame = surface.with_pending_state(|pending| {
                pending.decoration_mode == Some(wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode::ServerSide)
                    && !pending.states.contains(wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen)
            });
            if server_frame {
                target.size.h = (target.size.h - self.decoration_for_window(window).height).max(1);
            }
            Some(target.size)
        }).flatten();
        surface.with_pending_state(|pending| {
            use wayland_protocols::xdg::shell::server::xdg_toplevel::State;
            if maximized {
                pending.states.set(State::Maximized);
            } else {
                pending.states.unset(State::Maximized);
            }
            if !pending.states.contains(State::Fullscreen) {
                pending.size = size;
            }
        });
    }

    fn initial_window_anchor(
        &self,
        window: &Window,
    ) -> Option<Rectangle<i32, smithay::utils::Logical>> {
        window
            .toplevel()
            .and_then(|surface| surface.parent())
            .and_then(|surface| {
                self.space
                    .elements()
                    .find(|known| {
                        known
                            .toplevel()
                            .is_some_and(|top| top.wl_surface() == &surface)
                    })
                    .and_then(|parent| self.space.element_geometry(parent))
            })
    }

    fn initial_window_output(&self, window: &Window) -> Option<Output> {
        self.initial_window_anchor(window)
            .and_then(|geometry| {
                let center = geometry.loc + Point::from((geometry.size.w / 2, geometry.size.h / 2));
                self.space
                    .outputs()
                    .find(|output| {
                        self.space
                            .output_geometry(output)
                            .is_some_and(|bounds| bounds.contains(center))
                    })
                    .cloned()
            })
            .or_else(|| self.space.outputs().next().cloned())
    }

    pub(crate) fn initial_surface_output(&self, surface: &WlSurface) -> Option<Output> {
        self.windows
            .lifecycle
            .known
            .iter()
            .find(|window| {
                window
                    .toplevel()
                    .is_some_and(|top| top.wl_surface() == surface)
            })
            .and_then(|window| self.initial_window_output(window))
    }

    pub(crate) fn initial_window_location(
        &self,
        window: &Window,
    ) -> Point<i32, smithay::utils::Logical> {
        let parent = self.initial_window_anchor(window);
        let Some(output) = self.initial_window_output(window) else {
            return (0, 0).into();
        };
        let usable = self.work_area(&output);
        let geometry = window.geometry();
        let outer = if crate::decoration::enabled(window) {
            self.decoration_for_window(window).bounds(geometry)
        } else {
            geometry
        };
        let anchor = parent.unwrap_or(usable);
        let desired = anchor.loc
            + Point::from((
                (anchor.size.w - outer.size.w) / 2,
                (anchor.size.h - outer.size.h) / 2,
            ));
        Point::from((
            desired.x.clamp(
                usable.loc.x,
                usable.loc.x + (usable.size.w - outer.size.w).max(0),
            ),
            desired.y.clamp(
                usable.loc.y,
                usable.loc.y + (usable.size.h - outer.size.h).max(0),
            ),
        )) - outer.loc
    }

    pub(crate) fn position_attached_dialog(&mut self, surface: &ToplevelSurface) {
        if surface.parent().is_none() || self.lock.active {
            return;
        }
        let window = self
            .space
            .elements()
            .find(|window| window.toplevel() == Some(surface))
            .cloned();
        if let Some(window) = window {
            let location = self.initial_window_location(&window);
            self.space.map_element(window, location, false);
            self.refresh_modal_pointer_focus();
        }
    }

    pub(crate) fn register_toplevel(&mut self, surface: ToplevelSurface) {
        if let Some((_, mode)) = self.kde_decorations.get(surface.wl_surface()) {
            crate::decoration::set_legacy_mode(&surface, *mode);
        }
        self.windows
            .lifecycle
            .known
            .push(Window::new_wayland_window(surface));

    }

    pub(crate) fn commit_toplevel(&mut self, surface: &WlSurface) {
        if is_sync_subsurface(surface) {
            return;
        }
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        let Some(window) = self
            .windows
            .lifecycle
            .known
            .iter()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &root))
            .cloned()
        else {
            return;
        };
        window.on_commit();
        let Some(toplevel) = window.toplevel() else {

            return;
        };
        let has_content =
            with_renderer_surface_state(&root, |s| s.buffer().is_some()).unwrap_or(false);
        let registered = self.windows.entries.iter().any(|(_, w)| w == &window);
        if !has_content {
            if registered {
                self.unmap_toplevel(&window);
                self.windows.lifecycle.initial_maximized.remove(&root);
                toplevel.reset_initial_configure_sent();
                toplevel.with_pending_state(|state| {
                    state.states = Default::default();
                    state.size = None;
                    state.fullscreen_output = None;
                });
            } else if !toplevel.is_initial_configure_sent() {
                self.prepare_initial_surface_scale(&root);
                if self.windows.lifecycle.initial_maximized.contains(&root) {
                    self.configure_initial_maximize(toplevel, true);
                }
                toplevel.send_configure();
            }
            return;
        }
        if !toplevel.ensure_configured() {
            return;
        }
        if !registered {
            let location = self.initial_window_location(&window);
            self.space.map_element(window.clone(), location, true);

            self.space.refresh();
            self.windows.lifecycle.next_id += 1;
            let id = self.windows.lifecycle.next_id;
            self.windows.entries.push((id, window.clone()));
            let requested_maximized = self.windows.lifecycle.initial_maximized.remove(&root);
            let committed_maximized = toplevel
                .current_state()
                .states
                .contains(wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized);
            if committed_maximized {

                self.windows.maximized.insert(id, (location, (0, 0).into()));
            }
            if requested_maximized != committed_maximized {
                let _ = self.toggle_maximize(id, None);
            }
            let (_, app_id) = crate::state::get_toplevel_info(toplevel);
            tracing::info!(
                window_id = id,
                app_id = app_id.as_deref().unwrap_or_default(),
                since_start_ms = self.start_time.elapsed().as_millis(),
                "Window's first buffer mapped by compositor"
            );

            self.reflow_output_layout();
            if let (Some(sender), Some(info)) = (
                &self.ipc_event_sender,
                self.list_windows().into_iter().find(|w| w.id == id),
            ) {
                let _ = sender.send(rix_ipc::IpcEvent::WindowOpened(info));
            }
            self.focus_window(&window);
            self.refresh_modal_pointer_focus();
            self.emit_window_change();
        }
        if let Some((id, _)) = self
            .windows
            .entries
            .iter()
            .find(|(_, mapped)| mapped == &window)
        {
            let id = *id;
            let committed_serial = smithay::wayland::compositor::with_states(&root, |states| {
                states
                    .data_map
                    .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                    .and_then(|data| match data.lock() {
                        Ok(data) => data.current_serial,
                        Err(poisoned) => poisoned.into_inner().current_serial,
                    })
            });
            if let Some(resize) = self.windows.resize_placement.get(&id) {
                if committed_serial.is_some_and(|serial| serial >= resize.first_serial) {
                    let location = resize.location(window.geometry().size);
                    let finished = resize.expired()
                        || (resize.released
                            && committed_serial.is_some_and(|serial| serial >= resize.last_serial)
                            && window.geometry().size == resize.target_size);
                    if let Some(minimized) = self.windows.minimized.get_mut(&id) {
                        *minimized = location;
                    } else {
                        let stack: Vec<_> = self.space.elements().cloned().collect();
                        self.space.map_element(window.clone(), location, false);
                        for mapped in stack {
                            self.space.raise_element(&mapped, false);
                        }
                    }
                    if finished {
                        self.windows.resize_placement.remove(&id);
                    }
                    self.space.refresh();
                }
            }
            let placement_ready = self
                .windows
                .pending_placement
                .get(&id)
                .is_some_and(|pending| {
                    let client_size_ready = pending.client_chosen_size
                        && (window.geometry().size != pending.target.size
                            || with_renderer_surface_state(&root, |state| {
                                state.buffer().is_some_and(|buffer| {
                                    pending
                                        .previous_buffer
                                        .as_ref()
                                        .is_none_or(|previous| !buffer.eq(previous))
                                })
                            })
                            .unwrap_or(false));
                    committed_serial.is_some_and(|serial| serial >= pending.serial)
                        && (client_size_ready
                            || (!pending.client_chosen_size
                                && window.geometry().size == pending.target.size)
                            || pending.expired())
                });
            if placement_ready {
                if let Some(pending) = self.windows.pending_placement.remove(&id) {
                    if let Some(location) = self.windows.minimized.get_mut(&id) {
                        *location = pending.target.loc;
                    } else {

                        let stack: Vec<_> = self.space.elements().cloned().collect();
                        self.space
                            .map_element(window.clone(), pending.target.loc, false);
                        for mapped in stack {
                            self.space.raise_element(&mapped, false);
                        }
                    }
                    self.space.refresh();
                    if pending.client_chosen_size {
                        self.reflow_output_layout();
                    }
                }
            }
            if !self.windows.pending_placement.contains_key(&id)
                && self.windows.restoring.get(&id).is_some_and(|restore| {
                    let size = window.geometry().size;
                    size == restore.size || size != restore.previous_size || restore.expired()
                })
                && !toplevel
                    .current_state()
                    .states
                    .contains(wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized)
            {
                self.windows.restoring.remove(&id);
            }
            let (title, app_id) = crate::state::get_toplevel_info(toplevel);
            let app_id = app_id.unwrap_or_default();
            let title = title.filter(|title| !title.is_empty()).unwrap_or_else(|| {
                if app_id.is_empty() {
                    "Application".into()
                } else {
                    app_id.clone()
                }
            });
            self.window_capture.announce(id, &title, &app_id);
        }
    }

    fn unmap_toplevel(&mut self, window: &Window) {
        self.space.unmap_elem(window);
        let Some(index) = self.windows.entries.iter().position(|(_, w)| w == window) else {
            return;
        };
        let (id, _) = self.windows.entries.remove(index);
        self.window_capture.close_window(id);
        self.windows.minimized.remove(&id);
        self.windows.maximized.remove(&id);
        self.windows.restoring.remove(&id);
        self.windows.pending_placement.remove(&id);
        self.windows.resize_placement.remove(&id);
        self.windows.fullscreen.remove(&id);
        if let Some(sender) = &self.ipc_event_sender {
            let _ = sender.send(rix_ipc::IpcEvent::WindowClosed(id));
        }
        if self.windows.active == Some(id) {
            self.windows.active = None;
            if self.lock.active {
                self.emit_window_change();
                return;
            }
            let next = self
                .space
                .elements()
                .rev()
                .find(|w| self.windows.entries.iter().any(|(_, mapped)| mapped == *w))
                .cloned();
            if let Some(next) = next {
                self.focus_window(&next);
            } else if let Some(keyboard) = self.seat.get_keyboard() {
                self.keyboard_focus_lost();
                keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
            }
        }
        self.emit_window_change();
    }

    pub(crate) fn destroy_toplevel(&mut self, surface: &ToplevelSurface) {
        self.windows
            .lifecycle
            .initial_maximized
            .remove(surface.wl_surface());
        self.decoration_preferences.remove(surface.wl_surface());
        self.kde_decorations.remove(surface.wl_surface());
        if let Some(index) = self
            .windows
            .lifecycle
            .known
            .iter()
            .position(|w| w.toplevel() == Some(surface))
        {
            let window = self.windows.lifecycle.known.remove(index);
            self.unmap_toplevel(&window);
        }
        self.space.refresh();
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
pub(crate) mod tests;
