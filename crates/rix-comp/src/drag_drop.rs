use crate::state::RixState;
use smithay::{
    backend::renderer::{
        element::{
            surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
            Id, Kind,
        },
        gles::GlesRenderer,
    },
    input::pointer::MotionEvent,
    output::Output,
    reexports::wayland_server::protocol::{wl_data_source::WlDataSource, wl_surface::WlSurface},
    utils::{IsAlive, SERIAL_COUNTER},
};
use std::time::Instant;

pub(crate) struct DragFinish {
    pub icon: Id,
    pub ended: Instant,
    pub accepted: bool,
}

#[derive(Default)]
pub(crate) struct DragState {
    pub active: bool,
    pub icon: Option<WlSurface>,
    pub origin: Option<WlSurface>,
    pub source: Option<WlDataSource>,
    pub touch_slot: Option<smithay::backend::input::TouchSlot>,
    pub touch_location: Option<smithay::utils::Point<f64, smithay::utils::Logical>>,
    pub started: Option<Instant>,
    pub finished: Option<DragFinish>,
}

impl RixState {
    pub(crate) fn refresh_drag_action(&mut self) {
        if !self.drag.active {
            return;
        }
        if let Some(pointer) = self.seat.get_pointer() {
            pointer.with_grab(|_, grab| {
                if let Some(drag) =
                    grab.downcast_ref::<smithay::wayland::selection::data_device::DnDGrab<Self>>()
                {
                    drag.refresh_action(self);
                }
            });
        }
    }

    pub(crate) fn cancel_drag(&mut self) {
        if !self.drag.active {
            return;
        }
        if self.drag.touch_slot.is_some() {
            if let Some(touch) = self.seat.get_touch() {
                touch.cancel(self);
            }
        } else if let Some(pointer) = self.seat.get_pointer() {
            let serial = SERIAL_COUNTER.next_serial();
            let time = self.start_time.elapsed().as_millis() as u32;
            pointer.motion(
                self,
                None,
                &MotionEvent {
                    location: pointer.current_location(),
                    serial,
                    time,
                },
            );
            pointer.unset_grab(self, serial, time);
            pointer.frame(self);
        }

        self.drag = Default::default();
    }

    pub(crate) fn prune_drag(&mut self) {
        if self.drag.active
            && (self
                .drag
                .source
                .as_ref()
                .is_some_and(|source| !source.alive())
                || self
                    .drag
                    .origin
                    .as_ref()
                    .is_some_and(|origin| !origin.alive()))
        {
            self.cancel_drag();
        }
    }
}

pub(crate) fn icon_elements(
    renderer: &mut GlesRenderer,
    state: &RixState,
    output: &Output,
    capture: bool,
) -> Vec<WaylandSurfaceRenderElement<GlesRenderer>> {
    icon_elements_with_alpha(renderer, state, output, capture, 1.)
}

pub(crate) fn icon_elements_with_alpha(
    renderer: &mut GlesRenderer,
    state: &RixState,
    output: &Output,
    capture: bool,
    alpha: f32,
) -> Vec<WaylandSurfaceRenderElement<GlesRenderer>> {
    if !state.drag.active || state.lock.active || state.greeter_mode {
        return Vec::new();
    }
    let Some(icon) = state.drag.icon.as_ref().filter(|surface| surface.alive()) else {
        return Vec::new();
    };

    if capture
        && (crate::capture_policy::excluded(icon)
            || state
                .drag
                .origin
                .as_ref()
                .is_some_and(crate::capture_policy::excluded))
    {
        return Vec::new();
    }
    let Some(bounds) = state.space.output_geometry(output) else {
        return Vec::new();
    };
    let Some(location) = state.drag.touch_location.or_else(|| {
        state
            .seat
            .get_pointer()
            .map(|pointer| pointer.current_location())
    }) else {
        return Vec::new();
    };
    let scale = output.current_scale().fractional_scale();
    let elements = render_elements_from_surface_tree(
        renderer,
        icon,
        (location - bounds.loc.to_f64()).to_physical_precise_round(scale),
        scale,
        alpha,
        Kind::Unspecified,
    );
    if capture {
        let filter = crate::capture_policy::CaptureFilter::for_cursor(
            &smithay::input::pointer::CursorImageStatus::Surface(icon.clone()),
        );
        elements
            .into_iter()
            .filter(|element| filter.allows(element))
            .collect()
    } else {
        elements
    }
}
