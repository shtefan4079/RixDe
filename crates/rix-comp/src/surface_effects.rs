use crate::state::RixState;
use rix_protocols::server::{
    rix_surface_effects_manager_v1::{self, RixSurfaceEffectsManagerV1},
    rix_surface_effects_v1::{self, RixSurfaceEffectsV1},
};
use smithay::wayland::compositor::with_states;
use std::sync::Mutex;
use wayland_server::{
    protocol::wl_surface::WlSurface, Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
};
pub const MAX_SHAPES: usize = 16;
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Clip {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shape {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub radius: f64,
    pub opacity: f64,
    pub clip: Option<Clip>,
}
#[derive(Default)]
struct Coverage {
    current: Option<Vec<Shape>>,
    pending: Option<Vec<Shape>>,
}
#[derive(Default)]
struct State(Mutex<Coverage>);
pub struct Object {
    surface: WlSurface,
    pending: Mutex<Vec<Shape>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
pub fn install(dh: &DisplayHandle) {
    dh.create_global::<RixState, RixSurfaceEffectsManagerV1, _>(2, ());
}
pub fn shapes(surface: &WlSurface) -> Option<Vec<Shape>> {
    with_states(surface, |s| {
        s.data_map
            .get::<State>()
            .and_then(|s| lock(&s.0).current.clone())
    })
}
pub fn surface_commit(surface: &WlSurface) {
    with_states(surface, |s| {
        if let Some(state) = s.data_map.get::<State>() {
            let mut state = lock(&state.0);
            if let Some(next) = state.pending.take() {
                state.current = Some(next);
            }
        }
    });
}
impl GlobalDispatch<RixSurfaceEffectsManagerV1, ()> for RixState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<RixSurfaceEffectsManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}
impl Dispatch<RixSurfaceEffectsManagerV1, ()> for RixState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &RixSurfaceEffectsManagerV1,
        request: rix_surface_effects_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let rix_surface_effects_manager_v1::Request::GetSurface { id, surface } = request {
            with_states(&surface, |s| {
                s.data_map.insert_if_missing(State::default);
            });
            init.init(
                id,
                Object {
                    surface,
                    pending: Mutex::default(),
                },
            );
        }
    }
}
impl Dispatch<RixSurfaceEffectsV1, Object> for RixState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &RixSurfaceEffectsV1,
        request: rix_surface_effects_v1::Request,
        object: &Object,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            rix_surface_effects_v1::Request::Clear => lock(&object.pending).clear(),
            rix_surface_effects_v1::Request::AddRectangle {
                x,
                y,
                width,
                height,
                radius,
                opacity,
            } => {
                object.add(Shape {
                    x,
                    y,
                    width,
                    height,
                    radius,
                    opacity,
                    clip: None,
                });
            }
            rix_surface_effects_v1::Request::AddClippedRectangle {
                x,
                y,
                width,
                height,
                radius,
                opacity,
                clip_x,
                clip_y,
                clip_width,
                clip_height,
            } => {
                object.add(Shape {
                    x,
                    y,
                    width,
                    height,
                    radius,
                    opacity,
                    clip: Some(Clip {
                        x: clip_x,
                        y: clip_y,
                        width: clip_width,
                        height: clip_height,
                    }),
                });
            }
            rix_surface_effects_v1::Request::Commit => with_states(&object.surface, |s| {
                if let Some(state) = s.data_map.get::<State>() {
                    lock(&state.0).pending = Some(lock(&object.pending).clone());
                }
            }),
            rix_surface_effects_v1::Request::Destroy => with_states(&object.surface, |s| {
                if let Some(state) = s.data_map.get::<State>() {
                    *lock(&state.0) = Coverage::default();
                }
            }),
            _ => {}
        }
    }
}

impl Object {
    fn add(&self, mut shape: Shape) {
        if ![
            shape.x,
            shape.y,
            shape.width,
            shape.height,
            shape.radius,
            shape.opacity,
        ]
        .iter()
        .all(|n| n.is_finite())
            || shape.width <= 0.
            || shape.height <= 0.
        {
            return;
        }
        if let Some(c) = shape.clip {
            if ![c.x, c.y, c.width, c.height].iter().all(|n| n.is_finite())
                || c.width <= 0.
                || c.height <= 0.
            {
                return;
            }
        }
        shape.radius = shape.radius.clamp(0., shape.width.min(shape.height) / 2.);
        shape.opacity = shape.opacity.clamp(0., 1.);
        let mut pending = lock(&self.pending);
        if pending.len() < MAX_SHAPES {
            pending.push(shape);
        }
    }
}
