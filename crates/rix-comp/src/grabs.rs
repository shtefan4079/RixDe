use crate::state::RixState;
use smithay::{
    desktop::Window,
    input::{
        pointer::{
            AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent,
            GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
            GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData,
            MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
        },
        SeatHandler,
    },
    output::Output,
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge,
    utils::{Logical, Point, Size},
};
use std::time::Instant;

fn ordered(a: f64, b: f64) -> (f64, f64) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

fn contains_top_edge_zone(
    geometry: smithay::utils::Rectangle<i32, Logical>,
    pointer: Point<f64, Logical>,
    threshold: u32,
) -> bool {
    let threshold = threshold as f64;
    pointer.x >= geometry.loc.x as f64
        && pointer.x < (geometry.loc.x + geometry.size.w) as f64
        && pointer.y >= geometry.loc.y as f64
        && pointer.y < (geometry.loc.y as f64 + threshold)
}

pub struct MoveSurfaceGrab {
    pub start_data: GrabStartData<RixState>,
    pub window: Window,
    pub initial_window_location: Point<i32, Logical>,
    pub snap_output: Option<Output>,
}

impl PointerGrab<RixState> for MoveSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        _focus: Option<(<RixState as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);

        if crate::window_manager::is_fullscreen(&self.window) {
            handle.unset_grab(self, data, event.serial, event.time, true);
            return;
        }

        if let Some((id, _)) = data
            .windows
            .entries
            .iter()
            .find(|(_, window)| window == &self.window)
        {
            data.windows.resize_placement.remove(id);
        }
        let delta = event.location - self.start_data.location;
        let raw_location = self.initial_window_location.to_f64() + delta;

        let (desktop_left, desktop_top, desktop_right, desktop_bottom) =
            data.desktop_bounds().unwrap_or((0.0, 0.0, 1280.0, 720.0));
        let win_geo = data
            .space
            .element_geometry(&self.window)
            .unwrap_or_default();
        let min_visible_x = 100.0;
        let min_visible_y = 50.0;
        let (x_lo, x_hi) = ordered(
            desktop_left - (win_geo.size.w as f64 - min_visible_x),
            desktop_right - min_visible_x,
        );
        let (y_lo, y_hi) = ordered(desktop_top, desktop_bottom - min_visible_y);
        let clamped_x = raw_location.x.clamp(x_lo, x_hi);
        let clamped_y = raw_location.y.clamp(y_lo, y_hi);
        let new_location = smithay::utils::Point::<f64, Logical>::from((clamped_x, clamped_y));
        data.space
            .map_element(self.window.clone(), new_location.to_i32_round(), false);
        self.update_snap_target(data, event.location);

        data.space_dirty = true;
        let _ = data.display_handle.flush_clients();
    }

    fn relative_motion(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        _focus: Option<(<RixState as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, None, event);
    }

    fn button(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            if let Some(output) = self.snap_output.take() {
                let id = data
                    .windows
                    .entries
                    .iter()
                    .find(|(_, window)| window == &self.window)
                    .map(|(id, _)| *id);
                if data.window_snap_settings.maximize_at_top_edge {
                    if let Some(id) = id.filter(|id| {
                        !data.windows.maximized.contains_key(id)
                            && !data.windows.fullscreen.contains_key(id)
                    }) {
                        if let Err(error) = data.maximize_on_output(id, &output) {
                            tracing::debug!(%error, "Could not maximize window after edge snap");
                        }
                    }
                }
            }
            if let Some(preview) = data.snap_preview.as_mut() {
                preview.visible = false;
                preview.transition_started = Instant::now();
            }
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
        let _ = data.display_handle.flush_clients();
    }

    fn axis(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut RixState, handle: &mut PointerInnerHandle<'_, RixState>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &GrabStartData<RixState> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut RixState) {
        self.snap_output = None;
        if let Some(preview) = data.snap_preview.as_mut() {
            preview.visible = false;
            preview.transition_started = Instant::now();
        }
    }
}

impl MoveSurfaceGrab {
    fn update_snap_target(&mut self, data: &mut RixState, pointer: Point<f64, Logical>) {
        let window_id = data
            .windows
            .entries
            .iter()
            .find(|(_, window)| window == &self.window)
            .map(|(id, _)| *id);
        let can_snap = data.window_snap_settings.maximize_at_top_edge
            && window_id.is_some_and(|id| {
                !data.windows.maximized.contains_key(&id)
                    && !data.windows.fullscreen.contains_key(&id)
            });
        let target = can_snap
            .then(|| {
                data.space.outputs().find_map(|output| {
                    let geometry = data.space.output_geometry(output)?;
                    contains_top_edge_zone(
                        geometry,
                        pointer,
                        data.window_snap_settings.trigger_distance,
                    )
                    .then(|| (output.clone(), data.work_area(output)))
                })
            })
            .flatten();

        if let Some((output, geometry)) = target {
            self.snap_output = Some(output.clone());
            let output_name = output.name();
            let unchanged = data.snap_preview.as_ref().is_some_and(|preview| {
                preview.visible
                    && preview.output_name == output_name
                    && preview.geometry == geometry
            });
            if !unchanged {
                data.snap_preview = Some(crate::state::WindowSnapPreview {
                    output_name,
                    geometry,
                    visible: true,
                    transition_started: Instant::now(),
                });
            }
        } else {
            self.snap_output = None;
            if let Some(preview) = data.snap_preview.as_mut() {
                if preview.visible {
                    preview.visible = false;
                    preview.transition_started = Instant::now();
                }
            }
        }
    }
}

pub fn resize_cursor(edge: ResizeEdge) -> smithay::input::pointer::CursorIcon {
    use smithay::input::pointer::CursorIcon;
    match edge {
        ResizeEdge::Top => CursorIcon::NResize,
        ResizeEdge::Bottom => CursorIcon::SResize,
        ResizeEdge::Left => CursorIcon::WResize,
        ResizeEdge::Right => CursorIcon::EResize,
        ResizeEdge::TopLeft => CursorIcon::NwResize,
        ResizeEdge::TopRight => CursorIcon::NeResize,
        ResizeEdge::BottomLeft => CursorIcon::SwResize,
        ResizeEdge::BottomRight => CursorIcon::SeResize,
        _ => CursorIcon::Default,
    }
}

pub struct ResizeSurfaceGrab {
    pub start_data: GrabStartData<RixState>,
    pub window: Window,
    pub edges: ResizeEdge,
    pub initial_window_location: Point<i32, Logical>,
    pub initial_window_size: Size<i32, Logical>,
}

impl PointerGrab<RixState> for ResizeSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        _focus: Option<(<RixState as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        if crate::window_manager::is_fullscreen(&self.window) {
            handle.unset_grab(self, data, event.serial, event.time, true);
            return;
        }
        let Some(toplevel) = self.window.toplevel() else {
            return;
        };
        let delta = event.location - self.start_data.location;
        let left = matches!(
            self.edges,
            ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft
        );
        let right = matches!(
            self.edges,
            ResizeEdge::Right | ResizeEdge::TopRight | ResizeEdge::BottomRight
        );
        let top = matches!(
            self.edges,
            ResizeEdge::Top | ResizeEdge::TopLeft | ResizeEdge::TopRight
        );
        let bottom = matches!(
            self.edges,
            ResizeEdge::Bottom | ResizeEdge::BottomLeft | ResizeEdge::BottomRight
        );
        let dx = delta.x.round() as i32;
        let dy = delta.y.round() as i32;
        let (minimum, maximum) =
            smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
                let mut state = states
                    .cached_state
                    .get::<smithay::wayland::shell::xdg::SurfaceCachedState>();
                let current = state.current();
                (current.min_size, current.max_size)
            });

        let limit = |value: i32, min: i32, max: i32| {
            let min = min.max(1);
            let max = if max > 0 { max.max(min) } else { i32::MAX };
            value.clamp(min, max)
        };
        let width = if left {
            self.initial_window_size.w - dx
        } else if right {
            self.initial_window_size.w + dx
        } else {
            self.initial_window_size.w
        };
        let width = limit(width, minimum.w, maximum.w);
        let height = if top {
            self.initial_window_size.h - dy
        } else if bottom {
            self.initial_window_size.h + dy
        } else {
            self.initial_window_size.h
        };
        let height = limit(height, minimum.h, maximum.h);
        toplevel.with_pending_state(|state| {
            state.states.set(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Resizing);
            state.size = Some((width, height).into());
        });
        let serial = toplevel.send_configure();
        if let Some((id, _)) = data
            .windows
            .entries
            .iter()
            .find(|(_, window)| window == &self.window)
        {
            if data
                .windows
                .resize_placement
                .get(id)
                .is_some_and(|resize| resize.released)
            {
                data.windows.resize_placement.remove(id);
            }
            let resize = data.windows.resize_placement.entry(*id).or_insert_with(|| {
                crate::window_manager::ResizePlacement {
                    initial: smithay::utils::Rectangle::new(
                        self.initial_window_location,
                        self.initial_window_size,
                    ),
                    left,
                    top,
                    first_serial: serial,
                    last_serial: serial,
                    target_size: (width, height).into(),
                    released: false,
                    started: std::time::Instant::now(),
                }
            });
            resize.last_serial = serial;
            resize.target_size = (width, height).into();
            resize.started = std::time::Instant::now();
        }
        let _ = data.display_handle.flush_clients();
    }

    fn relative_motion(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        _focus: Option<(<RixState as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, None, event);
    }

    fn button(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            if let Some(toplevel) = self.window.toplevel() {
                toplevel.with_pending_state(|state| {
                    state.states.unset(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Resizing);
                });
                let serial = toplevel.send_configure();
                if let Some((id, _)) = data
                    .windows
                    .entries
                    .iter()
                    .find(|(_, window)| window == &self.window)
                {
                    if let Some(resize) = data.windows.resize_placement.get_mut(id) {
                        resize.released = true;
                        resize.last_serial = serial;
                        resize.started = std::time::Instant::now();
                    }
                }
            }
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
        let _ = data.display_handle.flush_clients();
    }

    fn axis(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut RixState, handle: &mut PointerInnerHandle<'_, RixState>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut RixState,
        handle: &mut PointerInnerHandle<'_, RixState>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &GrabStartData<RixState> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut RixState) {
        data.resize_cursor = None;
        if let Some(toplevel) = self.window.toplevel() {
            if let Some((id, _)) = data
                .windows
                .entries
                .iter()
                .find(|(_, window)| window == &self.window)
            {
                if let Some(resize) = data
                    .windows
                    .resize_placement
                    .get_mut(id)
                    .filter(|resize| !resize.released)
                {
                    toplevel.with_pending_state(|state| {
                        state.states.unset(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Resizing);
                    });
                    resize.last_serial = toplevel.send_configure();
                    resize.released = true;
                    resize.started = std::time::Instant::now();
                }
            }
        }
    }
}

#[cfg(test)]
mod snap_zone_tests {
    use super::contains_top_edge_zone;
    use smithay::utils::{Logical, Point, Rectangle};

    #[test]
    fn snap_zone_tracks_the_selected_output_and_has_clear_edges() {
        let output = Rectangle::<i32, Logical>::new((1200, -200).into(), (1920, 1080).into());
        assert!(contains_top_edge_zone(
            output,
            Point::<f64, Logical>::from((1200., -200.)),
            28
        ));
        assert!(contains_top_edge_zone(
            output,
            Point::<f64, Logical>::from((3119., -173.)),
            28
        ));
        assert!(!contains_top_edge_zone(
            output,
            Point::<f64, Logical>::from((1200., -171.)),
            28
        ));
        assert!(!contains_top_edge_zone(
            output,
            Point::<f64, Logical>::from((1200., -228.)),
            28
        ));
        assert!(!contains_top_edge_zone(
            output,
            Point::<f64, Logical>::from((3120., -200.)),
            28
        ));
    }
}
