use crate::state::RixState;
use smithay::{
    desktop::{layer_map_for_output, WindowSurfaceType},
    utils::{Logical, Point, Rectangle, Size},
};
use std::time::Duration;
use wayland_protocols::xdg::shell::server::xdg_toplevel;

fn visible_location(
    location: Point<i32, Logical>,
    size: Size<i32, Logical>,
    area: Rectangle<i32, Logical>,
) -> Point<i32, Logical> {
    (
        location
            .x
            .clamp(area.loc.x, area.loc.x + (area.size.w - size.w).max(0)),
        location
            .y
            .clamp(area.loc.y, area.loc.y + (area.size.h - size.h).max(0)),
    )
        .into()
}

impl RixState {

    pub(crate) fn detach_output(&mut self, output: &smithay::output::Output) {
        self.preserve_output_layers(output);
        self.space.unmap_output(output);
        self.space.refresh();
    }

    fn relocate_fullscreen_window(
        &mut self,
        id: u64,
        window: &smithay::desktop::Window,
        location: Point<i32, Logical>,
    ) {
        if let Some(minimized) = self.windows.minimized.get_mut(&id) {
            *minimized = location;
        } else if self.space.element_location(window).is_some() {
            let stack: Vec<_> = self.space.elements().cloned().collect();
            self.space.map_element(window.clone(), location, false);
            for mapped in stack {
                self.space.raise_element(&mapped, false);
            }
        }
    }

    pub(crate) fn preserve_output_layers(&mut self, output: &smithay::output::Output) {
        let layers: Vec<_> = layer_map_for_output(output).layers().cloned().collect();
        let mut map = layer_map_for_output(output);
        for layer in layers {
            map.unmap_layer(&layer);
            if !self
                .pending_layers
                .iter()
                .any(|pending| pending.wl_surface() == layer.wl_surface())
            {
                self.pending_layers.push(layer);
            }
        }
    }

    pub(crate) fn layer_target_output(
        &self,
        layer: &smithay::desktop::LayerSurface,
    ) -> Option<smithay::output::Output> {
        let mut outputs = self.space.outputs().cloned();
        match self.layer_outputs.get(layer.wl_surface()) {
            Some(name) => outputs.find(|output| output.name() == *name),
            None => outputs.next(),
        }
    }

    pub(crate) fn set_fullscreen(
        &mut self,
        surface: &smithay::wayland::shell::xdg::ToplevelSurface,
        requested: Option<wayland_server::protocol::wl_output::WlOutput>,
    ) {
        let window = self
            .windows
            .entries
            .iter()
            .find(|(_, w)| w.toplevel() == Some(surface))
            .cloned();
        let output = requested
            .as_ref()
            .and_then(smithay::output::Output::from_resource)
            .filter(|o| self.space.outputs().any(|current| current == o))
            .or_else(|| {
                window
                    .as_ref()
                    .and_then(|(_, w)| self.space.outputs_for_element(w).into_iter().next())
            })
            .or_else(|| self.space.outputs().next().cloned());
        let Some(output) = output else {
            surface.send_configure();
            return;
        };
        let Some(geometry) = self.space.output_geometry(&output) else {
            return;
        };
        if let Some((id, window)) = window {
            self.windows.pending_placement.remove(&id);
            self.windows.resize_placement.remove(&id);
            let location = self
                .space
                .element_location(&window)
                .or_else(|| self.windows.minimized.get(&id).copied())
                .unwrap_or(geometry.loc);
            self.windows
                .fullscreen
                .entry(id)
                .or_insert((location, window.geometry().size));
            self.relocate_fullscreen_window(id, &window, geometry.loc);
        }
        surface.with_pending_state(|pending| {
            pending.states.unset(xdg_toplevel::State::Resizing);
            pending.states.set(xdg_toplevel::State::Fullscreen);
            pending.size = Some(geometry.size);
            pending.fullscreen_output = requested;
        });
        surface.send_configure();
        self.emit_window_change();
    }

    pub(crate) fn unset_fullscreen(
        &mut self,
        surface: &smithay::wayland::shell::xdg::ToplevelSurface,
    ) {
        let window = self
            .windows
            .entries
            .iter()
            .find(|(_, w)| w.toplevel() == Some(surface))
            .cloned();
        let restore = window
            .as_ref()
            .and_then(|(id, _)| self.windows.fullscreen.remove(id));
        if let Some((id, _)) = &window {
            self.windows.pending_placement.remove(id);
            self.windows.resize_placement.remove(id);
        }
        surface.with_pending_state(|pending| {
            pending.states.unset(xdg_toplevel::State::Fullscreen);
            pending.fullscreen_output = None;
            pending.size = restore.map(|(_, size)| size);
        });
        if let (Some((id, window)), Some((location, _))) = (window, restore) {
            self.relocate_fullscreen_window(id, &window, location);
        }
        surface.send_configure();
        self.reflow_output_layout();
    }

    pub(crate) fn reflow_output_layout(&mut self) {
        self.sync_lock_outputs();

        self.windows
            .restoring
            .retain(|_, restore| !restore.expired());
        self.windows
            .resize_placement
            .retain(|_, resize| !resize.expired());
        self.prune_expired_placements(Duration::from_secs(10));
        let outputs: Vec<_> = self.space.outputs().cloned().collect();
        let pending = std::mem::take(&mut self.pending_layers);
        for layer in pending {
            let Some(output) = self.layer_target_output(&layer) else {

                self.pending_layers.push(layer);
                continue;
            };
            let mut layers = layer_map_for_output(&output);
            layers.cleanup();
            if layers
                .layer_for_surface(layer.wl_surface(), WindowSurfaceType::TOPLEVEL)
                .is_some()
            {
                continue;
            }
            if let Err(error) = layers.map_layer(&layer) {
                tracing::warn!(%error, "Could not map pending layer");

                self.pending_layers.push(layer);
            }
        }
        for output in &outputs {
            let mut layers = layer_map_for_output(output);
            layers.arrange();
            for layer in layers.layers() {
                if layer.layer_surface().has_pending_changes() {
                    layer.layer_surface().send_configure();
                }
            }
        }
        let Some(fallback) = outputs.first() else {
            return;
        };

        let windows: Vec<_> = self
            .space
            .elements()
            .filter_map(|window| {
                self.windows
                    .entries
                    .iter()
                    .find(|(_, known)| known == window)
                    .cloned()
            })
            .chain(
                self.windows
                    .entries
                    .iter()
                    .filter(|(id, _)| self.windows.minimized.contains_key(id))
                    .cloned(),
            )
            .collect();
        for (id, window) in windows {
            let Some(toplevel) = window.toplevel() else {
                continue;
            };

            if self
                .windows
                .pending_placement
                .get(&id)
                .is_some_and(|pending| pending.client_chosen_size)
            {
                continue;
            }
            let output = self
                .space
                .outputs_for_element(&window)
                .into_iter()
                .next()
                .unwrap_or_else(|| fallback.clone());
            let area = self.work_area(&output);
            let is_fullscreen = toplevel.with_pending_state(|pending| {
                pending.states.contains(xdg_toplevel::State::Fullscreen)
            });
            let mut target = area;
            if is_fullscreen {
                target = self.space.output_geometry(&output).unwrap_or(area);
            } else if self.windows.maximized.contains_key(&id)
                && crate::decoration::enabled(&window)
            {
                target.loc.y += self.decoration_for_window(&window).height;
                target.size.h = (target.size.h - self.decoration_for_window(&window).height).max(1);
            }
            let location = if is_fullscreen || self.windows.maximized.contains_key(&id) {
                toplevel.with_pending_state(|pending| pending.size = Some(target.size));
                if toplevel.has_pending_changes() {
                    let serial = toplevel.send_configure();
                    if let Some(pending) = self.windows.pending_placement.get_mut(&id) {
                        pending.serial = serial;
                    }
                }
                target.loc
            } else {

                let normal_size = self
                    .windows
                    .restoring
                    .get(&id)
                    .map(|restore| restore.size)
                    .or_else(|| {
                        self.windows
                            .pending_placement
                            .get(&id)
                            .map(|pending| pending.target.size)
                    })
                    .unwrap_or(window.geometry().size);
                let normal_geometry = Rectangle::new(window.geometry().loc, normal_size);
                let outer = if crate::decoration::enabled(&window) {
                    self.decoration_for_window_layout(&window)
                        .bounds(normal_geometry)
                } else {
                    normal_geometry
                };
                let location = self
                    .windows
                    .pending_placement
                    .get(&id)
                    .map(|pending| pending.target.loc)
                    .or_else(|| self.space.element_location(&window))
                    .or_else(|| self.windows.minimized.get(&id).copied())
                    .unwrap_or(area.loc);
                if outer.size.w > area.size.w || outer.size.h > area.size.h {
                    let content = normal_size;
                    let available: Size<i32, Logical> = Size::from((
                        (area.size.w - (outer.size.w - content.w)).max(1),
                        (area.size.h - (outer.size.h - content.h)).max(1),
                    ));
                    let size = Size::from((content.w.min(available.w), content.h.min(available.h)));
                    if let Some(restoring) = self.windows.restoring.get_mut(&id) {
                        restoring.size = size;
                    }
                    toplevel.with_pending_state(|pending| pending.size = Some(size));
                    if toplevel.has_pending_changes() {
                        let serial = toplevel.send_configure();
                        if let Some(pending) = self.windows.pending_placement.get_mut(&id) {
                            pending.serial = serial;
                        }
                    }
                }
                visible_location(location + outer.loc, outer.size, area) - outer.loc
            };
            if let Some(pending) = self.windows.pending_placement.get_mut(&id) {
                pending.target.loc = location;
                if let Some(size) = toplevel.with_pending_state(|state| state.size) {
                    pending.target.size = size;
                }

                continue;
            }
            if let Some(minimized) = self.windows.minimized.get_mut(&id) {
                *minimized = location;
            } else {
                self.space.map_element(window, location, false);
            }
            if let Some((location, size)) = self.windows.maximized.get_mut(&id) {
                *location = visible_location(*location, *size, area);
            }
        }
        if let Some(pointer) = self.seat.get_pointer() {
            let current = pointer.current_location();
            if !outputs.iter().any(|o| {
                self.space
                    .output_geometry(o)
                    .is_some_and(|geo| geo.to_f64().contains(current))
            }) {
                let location = self.work_area(fallback).loc.to_f64();
                let focus = self.surface_under(location);
                pointer.motion(
                    self,
                    focus,
                    &smithay::input::pointer::MotionEvent {
                        location,
                        serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                        time: self.start_time.elapsed().as_millis() as u32,
                    },
                );
                pointer.frame(self);
            }
        }
        self.space.refresh();
        self.emit_window_change();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unplugged_and_oversized_windows_keep_their_top_left_reachable() {
        let area = Rectangle::new((0, 0).into(), (1280, 720).into());
        assert_eq!(
            visible_location((2500, 900).into(), (800, 600).into(), area),
            (480, 120).into()
        );
        assert_eq!(
            visible_location((-150, -10).into(), (1800, 1000).into(), area),
            (0, 0).into()
        );
        let left = Rectangle::new((-1920, 40).into(), (1920, 1040).into());
        assert_eq!(
            visible_location((-1800, 100).into(), (900, 600).into(), left),
            (-1800, 100).into()
        );
    }
}

#[cfg(test)]
#[path = "output_layout_tests.rs"]
mod protocol_tests;
