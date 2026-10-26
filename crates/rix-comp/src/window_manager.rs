use crate::state::{get_toplevel_info, RixState};
use smithay::{
    desktop::Window,
    output::Output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle, Serial, Size},
};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::warn;
use wayland_protocols::xdg::shell::server::xdg_toplevel;

type WindowRestore = (Point<i32, Logical>, Size<i32, Logical>);

pub(crate) fn is_fullscreen(window: &Window) -> bool {
    window.toplevel().is_some_and(|surface| {
        surface
            .current_state()
            .states
            .contains(xdg_toplevel::State::Fullscreen)
            || surface.with_pending_state(|pending| {
                pending.states.contains(xdg_toplevel::State::Fullscreen)
            })
    })
}

pub(crate) struct ModalIndex {
    children: HashMap<WlSurface, Vec<Window>>,
}

pub(crate) struct RestoreSize {
    pub(crate) size: Size<i32, Logical>,
    pub(crate) previous_size: Size<i32, Logical>,
    started: Instant,
}
impl RestoreSize {
    pub(crate) fn expired(&self) -> bool {
        self.started.elapsed() >= Duration::from_secs(10)
    }
}

pub(crate) struct PendingPlacement {
    pub(crate) target: Rectangle<i32, Logical>,
    pub(crate) serial: Serial,
    pub(crate) previous_maximized: bool,
    pub(crate) client_chosen_size: bool,
    pub(crate) previous_buffer: Option<wayland_server::protocol::wl_buffer::WlBuffer>,
    started: Instant,
}

pub(crate) struct ResizePlacement {
    pub(crate) initial: Rectangle<i32, Logical>,
    pub(crate) left: bool,
    pub(crate) top: bool,
    pub(crate) first_serial: Serial,
    pub(crate) last_serial: Serial,
    pub(crate) target_size: Size<i32, Logical>,
    pub(crate) released: bool,
    pub(crate) started: Instant,
}
impl ResizePlacement {
    pub(crate) fn location(&self, committed_size: Size<i32, Logical>) -> Point<i32, Logical> {
        Point::from((
            self.initial.loc.x
                + if self.left {
                    self.initial.size.w - committed_size.w
                } else {
                    0
                },
            self.initial.loc.y
                + if self.top {
                    self.initial.size.h - committed_size.h
                } else {
                    0
                },
        ))
    }
    pub(crate) fn expired(&self) -> bool {
        self.released && self.started.elapsed() >= Duration::from_secs(10)
    }
}
impl PendingPlacement {
    pub(crate) fn expired(&self) -> bool {
        self.started.elapsed() >= Duration::from_secs(10)
    }
}

pub(crate) struct DesktopRestore {

    windows: Vec<u64>,
    active: Option<u64>,
}

#[derive(Default)]
pub(crate) struct WindowManager {
    pub(crate) entries: Vec<(u64, Window)>,
    pub(crate) lifecycle: crate::lifecycle::WindowLifecycle,
    pub(crate) active: Option<u64>,
    pub(crate) minimized: HashMap<u64, Point<i32, Logical>>,
    pub(crate) maximized: HashMap<u64, WindowRestore>,

    pub(crate) restoring: HashMap<u64, RestoreSize>,

    pub(crate) pending_placement: HashMap<u64, PendingPlacement>,
    pub(crate) resize_placement: HashMap<u64, ResizePlacement>,
    pub(crate) fullscreen: HashMap<u64, WindowRestore>,
    pub(crate) desktop_restore: Option<DesktopRestore>,

    foreground: Vec<(String, u64)>,
}

impl RixState {
    pub(crate) fn queue_window_placement(
        &mut self,
        id: u64,
        target: Rectangle<i32, Logical>,
        serial: Serial,
        previous_maximized: bool,
    ) {
        let previous_buffer = self
            .windows
            .entries
            .iter()
            .find(|(window_id, _)| *window_id == id)
            .and_then(|(_, window)| window.toplevel())
            .and_then(|surface| {
                smithay::backend::renderer::utils::with_renderer_surface_state(
                    surface.wl_surface(),
                    |state| {
                        state
                            .buffer()
                            .map(|buffer| std::ops::Deref::deref(buffer).clone())
                    },
                )
            })
            .flatten();
        self.windows.pending_placement.insert(
            id,
            PendingPlacement {
                target,
                serial,
                previous_maximized,
                client_chosen_size: false,
                previous_buffer,
                started: Instant::now(),
            },
        );
    }

    fn dismiss_popup_for_activation(&mut self) {
        use smithay::desktop::{PopupKeyboardGrab, PopupPointerGrab, PopupUngrabStrategy};
        let Some(mut grab) = self.active_popup_grab.take() else {
            return;
        };
        let serial = grab.serial();
        let previous = grab.previous_serial();
        let owns = |current| current == serial || Some(current) == previous;
        grab.ungrab(PopupUngrabStrategy::All);
        if let Some(pointer) = self.seat.get_pointer() {
            let matches = pointer.with_grab(|serial, handler| {
                owns(serial) && handler.downcast_ref::<PopupPointerGrab<Self>>().is_some()
            });
            if matches == Some(true) {
                pointer.unset_grab(
                    self,
                    smithay::utils::SERIAL_COUNTER.next_serial(),
                    self.start_time.elapsed().as_millis() as u32,
                );
                pointer.frame(self);
            }
        }
        if let Some(keyboard) = self.seat.get_keyboard() {
            let matches = keyboard.with_grab(|serial, handler| {
                owns(serial) && handler.downcast_ref::<PopupKeyboardGrab<Self>>().is_some()
            });
            if matches == Some(true) {
                keyboard.unset_grab(self);
            }
        }
        self.popups.cleanup();
    }

    pub(crate) fn prune_popup_grab(&mut self) {
        let Some(grab) = self.active_popup_grab.as_ref() else {
            return;
        };

        let live_popup = grab
            .current_grab()
            .map(|surface| self.popups.find_popup(&surface).is_some())
            .unwrap_or(false);
        if grab.has_ended() || !live_popup {
            self.active_popup_grab = None;
            self.popups.cleanup();
        }
    }

    fn foreground_windows(&self) -> Vec<(String, &Window)> {

        self.space
            .outputs()
            .filter_map(|output| {
                let bounds = self.space.output_geometry(output)?;
                let window = self.space.elements().rev().find(|window| {
                    window.toplevel().is_some()
                        && self.space.element_geometry(window).is_some_and(|geometry| {
                            geometry.size.w > 0 && geometry.size.h > 0 && geometry.overlaps(bounds)
                        })
                })?;
                Some((output.name(), window))
            })
            .collect()
    }

    pub(crate) fn refresh_foreground_windows(&mut self) {
        let foreground: Vec<_> = self
            .foreground_windows()
            .into_iter()
            .filter_map(|(output, window)| {
                self.windows
                    .entries
                    .iter()
                    .find(|(_, known)| known == window)
                    .map(|(id, _)| (output, *id))
            })
            .collect();
        if self.windows.foreground != foreground {
            self.windows.foreground = foreground;
            self.emit_window_change();
        }
    }

    pub fn list_windows(&self) -> Vec<rix_ipc::WindowInfo> {
        let foreground = self.foreground_windows();
        self.windows
            .entries
            .iter()
            .filter_map(|(id, window)| {
                let toplevel = window.toplevel()?;
                let (title, app_id) = get_toplevel_info(toplevel);
                let app_id = app_id.unwrap_or_default();
                let title = title.filter(|t| !t.is_empty()).unwrap_or_else(|| {
                    if app_id.is_empty() {
                        "Application".into()
                    } else {
                        app_id.clone()
                    }
                });
                Some(rix_ipc::WindowInfo {
                    id: *id,
                    title,
                    app_id,
                    outputs: self
                        .space
                        .outputs_for_element(window)
                        .iter()
                        .map(|o| o.name())
                        .collect(),
                    foreground_outputs: Some(
                        foreground
                            .iter()
                            .filter(|(_, topmost)| *topmost == window)
                            .map(|(name, _)| name.clone())
                            .collect(),
                    ),
                    is_active: self.windows.active == Some(*id),
                    is_minimized: self.windows.minimized.contains_key(id),
                    is_maximized: self.windows.maximized.contains_key(id),
                    is_fullscreen: toplevel.with_pending_state(|pending| {
                        pending.states.contains(xdg_toplevel::State::Fullscreen)
                    }),
                })
            })
            .collect()
    }

    pub(crate) fn modal_child(&self, parent: &Window) -> Option<Window> {
        let index = self.modal_index();
        self.modal_child_in(parent, &index)
    }

    pub(crate) fn modal_index(&self) -> ModalIndex {
        let mut children: HashMap<WlSurface, Vec<Window>> = HashMap::new();

        for window in self.space.elements().rev() {
            let Some(toplevel) = window.toplevel() else {
                continue;
            };
            let Some(parent) = toplevel.parent() else {
                continue;
            };
            let modal =
                smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
                    states
                        .data_map
                        .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                        .is_some_and(|data| match data.lock() {
                            Ok(data) => data.modal,
                            Err(poisoned) => poisoned.into_inner().modal,
                        })
                });
            if modal {
                children.entry(parent).or_default().push(window.clone());
            }
        }
        ModalIndex { children }
    }

    pub(crate) fn modal_child_in(&self, parent: &Window, index: &ModalIndex) -> Option<Window> {
        if index.children.is_empty() {
            return None;
        }
        let mut current = parent.clone();
        let mut visited = vec![current.clone()];
        for _ in 0..32 {
            let Some(root) = current
                .toplevel()
                .map(|surface| surface.wl_surface().clone())
            else {
                break;
            };
            let Some(child) = index
                .children
                .get(&root)
                .and_then(|children| children.iter().find(|child| !visited.contains(child)))
                .cloned()
            else {
                break;
            };
            visited.push(child.clone());
            current = child;
        }
        (current != *parent).then_some(current)
    }

    pub(crate) fn modal_blocked_at(&self, position: Point<f64, Logical>) -> Option<Window> {
        let window = self.space.elements().rev().find(|window| {
            let Some(body) = self.space.element_geometry(window) else {
                return false;
            };
            let outer = if crate::decoration::enabled(window) {
                self.decoration_for_window(window).bounds(body)
            } else {
                body
            };
            let origin =
                self.space.element_location(window).unwrap_or(body.loc) - window.geometry().loc;
            outer.to_f64().contains(position)
                || window
                    .surface_under(
                        position - origin.to_f64(),
                        smithay::desktop::WindowSurfaceType::ALL,
                    )
                    .is_some()
        })?;
        self.modal_child(window)
    }

    pub(crate) fn refresh_modal_pointer_focus(&mut self) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        if self.surface_under(pointer.current_location()).is_some()
            || self.modal_blocked_at(pointer.current_location()).is_none()
            || pointer.current_focus().is_none()
        {
            return;
        }
        pointer.motion(
            self,
            None,
            &smithay::input::pointer::MotionEvent {
                location: pointer.current_location(),
                serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                time: self.start_time.elapsed().as_millis() as u32,
            },
        );
        pointer.frame(self);
    }

    pub fn focus_window(&mut self, window: &Window) {
        if self.lock.active {
            return;
        }
        let window = self.modal_child(window).unwrap_or_else(|| window.clone());
        let Some(toplevel) = window.toplevel().cloned() else {
            return;
        };
        if self.space.element_location(&window).is_none() {
            return;
        }
        self.space.raise_element(&window, true);

        let previous = self.windows.active.and_then(|previous_id| {
            self.windows
                .entries
                .iter()
                .find(|(id, _)| *id == previous_id)
                .map(|(_, window)| window.clone())
        });
        if let Some(previous) = previous.filter(|previous| previous != &window) {
            if let Some(surface) = previous.toplevel() {
                if previous.set_activated(false) {
                    surface.send_configure();
                }
            }
        }
        if window.set_activated(true) {
            if let Some(surface) = window.toplevel() {
                surface.send_configure();
            }
        }
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(
                self,
                Some(toplevel.wl_surface().clone()),
                smithay::utils::SERIAL_COUNTER.next_serial(),
            );
        }
        let id = self
            .windows
            .entries
            .iter()
            .find(|(_, w)| w == &window)
            .map(|(id, _)| *id);
        if let Some(id) = id {
            self.switcher.focused(id);
        }
        if self.windows.active != id {
            self.windows.active = id;
            if let (Some(sender), Some(id)) = (&self.ipc_event_sender, id) {
                let _ = sender.send(rix_ipc::IpcEvent::WindowFocused(id));
            }
            self.emit_window_change();
        }
        let _ = self.display_handle.flush_clients();
    }

    pub fn process_window_commands(&mut self) {
        for _ in 0..rix_ipc::COMMANDS_PER_TICK {
            let Ok(command) = self.window_rx.try_recv() else {
                break;
            };
            let result = self.apply_window_action(command.action);
            let _ = command.responder.send(result.map(|()| self.list_windows()));
        }
    }

    pub(crate) fn apply_window_action(
        &mut self,
        action: rix_ipc::WindowAction,
    ) -> Result<(), String> {
        if self.lock.active {
            return Err("Session is locked".into());
        }
        use rix_ipc::WindowAction;
        let id = match action {
            WindowAction::List => return Ok(()),
            WindowAction::ShowDesktop => return self.toggle_desktop(),
            WindowAction::Focus(id)
            | WindowAction::Close(id)
            | WindowAction::Minimize(id)
            | WindowAction::Maximize(id) => id,
        };
        let window = self
            .windows
            .entries
            .iter()
            .find(|(window_id, _)| *window_id == id)
            .map(|(_, w)| w.clone())
            .ok_or_else(|| format!("Window {id} does not exist"))?;
        let toplevel = window
            .toplevel()
            .cloned()
            .ok_or_else(|| format!("Window {id} has no XDG toplevel"))?;
        match action {
            WindowAction::List | WindowAction::ShowDesktop => {

                warn!(
                    "Window action {action:?} reached the late dispatch; early return is missing"
                );
                return Ok(());
            }
            WindowAction::Focus(_) => {
                self.dismiss_popup_for_activation();
                if let Some(location) = self.windows.minimized.remove(&id) {
                    self.space.map_element(window.clone(), location, true);
                    self.space.refresh();
                }
                self.focus_window(&window);
            }
            WindowAction::Close(_) => {
                toplevel.send_close();
                self.display_handle
                    .flush_clients()
                    .map_err(|e| e.to_string())?;
            }
            WindowAction::Minimize(_) => {
                if !self.windows.minimized.contains_key(&id) {
                    let location = self
                        .space
                        .element_location(&window)
                        .ok_or_else(|| format!("Window {id} is not mapped"))?;
                    self.windows.minimized.insert(id, location);
                    self.space.unmap_elem(&window);
                    self.space.refresh();
                    if self.windows.active == Some(id) {

                        if window.set_activated(false) {
                            toplevel.send_configure();
                        }
                        self.windows.active = None;
                        if let Some(keyboard) = self.seat.get_keyboard() {
                            keyboard.set_focus(
                                self,
                                None,
                                smithay::utils::SERIAL_COUNTER.next_serial(),
                            );
                        }
                        let next = self
                            .space
                            .elements()
                            .rev()
                            .find(|w| self.windows.entries.iter().any(|(_, mapped)| mapped == *w))
                            .cloned();
                        if let Some(next) = next {
                            self.focus_window(&next);
                        }
                    }
                    self.emit_window_change();
                }
            }
            WindowAction::Maximize(_) => return self.toggle_maximize(id, None),
        }
        Ok(())
    }

    fn toggle_desktop(&mut self) -> Result<(), String> {
        self.dismiss_popup_for_activation();
        let visible: Vec<_> = self
            .space
            .elements()
            .filter_map(|window| {
                let id = self
                    .windows
                    .entries
                    .iter()
                    .find(|(_, known)| known == window)?
                    .0;
                let location = self.space.element_location(window).unwrap_or((0, 0).into());
                Some((id, window.clone(), location))
            })
            .collect();

        let showing = if !visible.is_empty() {
            let mut restore_windows = self
                .windows
                .desktop_restore
                .take()
                .map(|r| r.windows)
                .unwrap_or_default();
            for (id, _, _) in &visible {
                if !restore_windows.contains(id) {
                    restore_windows.push(*id);
                }
            }
            self.windows.desktop_restore = Some(DesktopRestore {
                windows: restore_windows,
                active: self.windows.active,
            });
            for (id, window, location) in visible {
                self.windows.minimized.insert(id, location);
                self.space.unmap_elem(&window);
                if window.set_activated(false) {
                    if let Some(toplevel) = window.toplevel() {
                        toplevel.send_configure();
                    }
                }
            }
            self.space.refresh();
            self.windows.active = None;
            self.keyboard_focus_lost();
            if let Some(keyboard) = self.seat.get_keyboard() {
                keyboard.set_focus(self, None, smithay::utils::SERIAL_COUNTER.next_serial());
            }
            true
        } else if let Some(restore) = self.windows.desktop_restore.take() {
            for id in restore.windows {
                let window = self
                    .windows
                    .entries
                    .iter()
                    .find(|(known, _)| *known == id)
                    .map(|(_, window)| window.clone());
                if let (Some(window), Some(location)) = (window, self.windows.minimized.remove(&id))
                {
                    self.space.map_element(window, location, false);
                }
            }
            self.space.refresh();
            let active = restore.active.and_then(|id| {
                self.windows
                    .entries
                    .iter()
                    .find(|(known, window)| {
                        *known == id && self.space.element_location(window).is_some()
                    })
                    .map(|(_, window)| window.clone())
            });
            let active = active.or_else(|| self.space.elements().next_back().cloned());
            if let Some(active) = active {
                self.focus_window(&active);
            }
            false
        } else {
            false
        };
        self.emit_window_change();
        if let Some(sender) = &self.ipc_event_sender {
            let _ = sender.send(rix_ipc::IpcEvent::ShowDesktopChanged(showing));
        }
        self.display_handle
            .flush_clients()
            .map_err(|error| error.to_string())
    }

    pub(crate) fn toggle_maximize(
        &mut self,
        id: u64,
        target_output: Option<&Output>,
    ) -> Result<(), String> {
        if self.windows.minimized.contains_key(&id) {
            return Err(format!(
                "Window {id} is minimized; focus it before maximizing"
            ));
        }
        let window = self
            .windows
            .entries
            .iter()
            .find(|(window_id, _)| *window_id == id)
            .map(|(_, window)| window.clone())
            .ok_or_else(|| format!("Window {id} does not exist"))?;
        let toplevel = window
            .toplevel()
            .cloned()
            .ok_or_else(|| format!("Window {id} has no XDG toplevel"))?;
        let previous_maximized = self
            .windows
            .pending_placement
            .get(&id)
            .map_or(self.windows.maximized.contains_key(&id), |pending| {
                pending.previous_maximized
            });
        self.windows.resize_placement.remove(&id);
        if let Some((location, size)) = self.windows.maximized.remove(&id) {
            let client_chosen_size = size.w == 0 || size.h == 0;
            if !client_chosen_size {
                self.windows.restoring.insert(
                    id,
                    RestoreSize {
                        size,
                        previous_size: window.geometry().size,
                        started: Instant::now(),
                    },
                );
            }
            toplevel.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Maximized);
                state.size = (!client_chosen_size).then_some(size);
            });
            let serial = toplevel.send_configure();
            self.queue_window_placement(
                id,
                Rectangle::new(
                    location,
                    if client_chosen_size {
                        window.geometry().size
                    } else {
                        size
                    },
                ),
                serial,
                previous_maximized,
            );
            if let Some(pending) = self.windows.pending_placement.get_mut(&id) {
                pending.client_chosen_size = client_chosen_size;
            }
        } else {
            let location = self
                .windows
                .pending_placement
                .get(&id)
                .map(|pending| pending.target.loc)
                .or_else(|| self.space.element_location(&window))
                .ok_or_else(|| format!("Window {id} is not mapped"))?;
            let size = self
                .windows
                .restoring
                .remove(&id)
                .map(|restore| restore.size)
                .unwrap_or(window.geometry().size);
            let output = target_output
                .cloned()
                .or_else(|| self.space.outputs_for_element(&window).into_iter().next())
                .or_else(|| self.space.outputs().next().cloned())
                .ok_or("No output available for maximization")?;
            let mut output_geometry = self.work_area(&output);
            if crate::decoration::enabled(&window) {
                output_geometry.loc.y += self.decoration_for_window(&window).height;
                output_geometry.size.h -= self.decoration_for_window(&window).height;
            }
            self.windows.maximized.insert(id, (location, size));
            toplevel.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Maximized);
                state.size = Some(output_geometry.size);
            });
            let serial = toplevel.send_configure();
            self.queue_window_placement(id, output_geometry, serial, previous_maximized);
        }
        self.space.refresh();
        self.focus_window(&window);
        self.emit_window_change();
        Ok(())
    }

    pub(crate) fn prune_expired_placements(&mut self, max_age: Duration) {

        let dragged_id = self
            .seat
            .get_pointer()
            .and_then(|pointer| {
                pointer.with_grab(|_, handler| {
                    handler
                        .downcast_ref::<crate::grabs::MoveSurfaceGrab>()
                        .and_then(|grab| {
                            self.windows
                                .entries
                                .iter()
                                .find(|(_, window)| window == &grab.window)
                                .map(|(id, _)| *id)
                        })
                })
            })
            .flatten();
        let expired: Vec<u64> = self
            .windows
            .pending_placement
            .iter()
            .filter(|(id, pending)| {
                pending.started.elapsed() >= max_age && Some(**id) != dragged_id
            })
            .map(|(id, _)| *id)
            .collect();
        if expired.is_empty() {
            return;
        }
        for id in expired {
            let Some(pending) = self.windows.pending_placement.remove(&id) else {
                continue;
            };
            let Some(window) = self
                .windows
                .entries
                .iter()
                .find(|(window_id, _)| *window_id == id)
                .map(|(_, window)| window.clone())
            else {
                continue;
            };
            if let Some(location) = self.windows.minimized.get_mut(&id) {
                *location = pending.target.loc;
            } else if self.space.element_location(&window).is_some() {

                let stack: Vec<_> = self.space.elements().cloned().collect();
                self.space.map_element(window, pending.target.loc, false);
                for mapped in stack {
                    self.space.raise_element(&mapped, false);
                }
            }
        }
        self.space.refresh();
    }
}
