use crate::state::RixState;
use rix_protocols::capture_server::{
    rix_capture_policy_manager_v1::{self, RixCapturePolicyManagerV1},
    rix_capture_policy_surface_v1::{self, Policy, RixCapturePolicySurfaceV1},
};
use smithay::{
    backend::renderer::element::{Element, Id},
    desktop::{PopupManager, Window},
    input::pointer::CursorImageStatus,
    wayland::{
        compositor::{
            get_parent, with_states, with_surface_tree_downward, Cacheable, TraversalAction,
        },
        shell::xdg::XdgPopupSurfaceData,
    },
};
use std::{collections::HashSet, sync::Mutex};
use wayland_server::{
    protocol::wl_surface::WlSurface, Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch,
    New, Resource, WEnum,
};

#[derive(Default)]
struct Slot(Mutex<Option<wayland_server::Weak<RixCapturePolicySurfaceV1>>>);

struct Update {
    policy: Policy,
    acknowledgement: Option<(wayland_server::Weak<RixCapturePolicySurfaceV1>, u32)>,
}

#[derive(Default)]
struct CapturePolicy {
    update: Option<Update>,
}

impl Cacheable for CapturePolicy {
    fn commit(&mut self, _: &DisplayHandle) -> Self {
        Self {
            update: self.update.take(),
        }
    }

    fn merge_into(self, into: &mut Self, _: &DisplayHandle) {
        if self.update.is_some() {
            into.update = self.update;
        }
    }
}

pub struct SurfaceData {
    surface: WlSurface,
}

pub fn install(dh: &DisplayHandle) {
    dh.create_global::<RixState, RixCapturePolicyManagerV1, _>(1, ());
}

pub fn surface_commit(state: &mut RixState, surface: &WlSurface) {
    let acknowledgement = with_states(surface, |states| {
        states.data_map.get::<Slot>()?;
        let mut cached = states.cached_state.get::<CapturePolicy>();
        let update = cached.current().update.as_mut()?;
        let (object, serial) = update.acknowledgement.take()?;
        Some((object, serial, update.policy))
    });
    let Some((object, serial, policy)) = acknowledgement else {
        return;
    };
    state.previews.cancel("Capture policy changed");
    state.screenshot_writer.cancel("Capture policy changed");
    state.screencopy.invalidate();
    state.window_capture.invalidate();
    if let Ok(object) = object.upgrade() {
        object.applied(serial, policy);
    }

    for (id, window) in &state.windows.entries {
        if window_excluded(window) {
            state.window_capture.stop_window(*id);
        }
    }
}

fn own_exclusion(surface: &WlSurface) -> bool {
    with_states(surface, state_excludes)
}

fn state_excludes(states: &smithay::wayland::compositor::SurfaceData) -> bool {
    states.data_map.get::<Slot>().is_some()
        && states
            .cached_state
            .get::<CapturePolicy>()
            .current()
            .update
            .as_ref()
            .is_some_and(|update| update.policy == Policy::Exclude)
}

pub fn excluded(surface: &WlSurface) -> bool {
    let mut ancestor = Some(surface.clone());
    let mut visited = HashSet::new();
    while let Some(surface) = ancestor {
        if !surface.is_alive() || !visited.insert(surface.id()) {
            return true;
        }
        if own_exclusion(&surface) {
            return true;
        }
        ancestor = surface_parent(&surface);
    }
    false
}

pub(crate) fn surface_parent(surface: &WlSurface) -> Option<WlSurface> {
    get_parent(surface).or_else(|| {
        with_states(surface, |states| {
            states
                .data_map
                .get::<XdgPopupSurfaceData>()
                .and_then(|data| data.lock().unwrap().parent.clone())
        })
    })
}

pub fn window_excluded(window: &Window) -> bool {
    window
        .toplevel()
        .is_some_and(|toplevel| excluded(toplevel.wl_surface()))
}

#[derive(Default)]
pub struct CaptureFilter {
    excluded: HashSet<Id>,
}

impl CaptureFilter {
    pub fn for_state(state: &RixState) -> Self {
        let mut filter = Self::default();
        for window in state.space.elements() {
            filter.add_window(window);
        }
        for layer in &state.layer_surfaces {
            filter.add_surface(layer.wl_surface());
        }
        for (_, surface) in &state.lock.surfaces {
            filter.add_surface(surface.wl_surface());
        }
        filter
    }

    pub fn for_window(window: &Window) -> Self {
        let mut filter = Self::default();
        filter.add_window(window);
        filter
    }

    pub fn for_cursor(status: &CursorImageStatus) -> Self {
        let mut filter = Self::default();
        if let CursorImageStatus::Surface(surface) = status {
            if surface.is_alive() {
                filter.add_surface(surface);
            }
        }
        filter
    }

    fn add_window(&mut self, window: &Window) {
        if let Some(toplevel) = window.toplevel() {
            self.add_surface(toplevel.wl_surface());
        }
    }

    fn add_surface(&mut self, surface: &WlSurface) {
        self.add_tree(surface);
        for (popup, _) in PopupManager::popups_for_surface(surface) {
            self.add_tree(popup.wl_surface());
        }
    }

    fn add_tree(&mut self, surface: &WlSurface) {
        if !surface.is_alive() {
            return;
        }
        let inherited = excluded(surface);
        with_surface_tree_downward(
            surface,
            inherited,
            |surface, states, inherited| {
                let excluded = *inherited || state_excludes(states);
                if excluded {
                    self.excluded.insert(Id::from_wayland_resource(surface));
                }
                TraversalAction::DoChildren(excluded)
            },
            |_, _, _| {},
            |_, _, _| true,
        );
    }

    pub fn allows(&self, element: &impl Element) -> bool {
        !self.excluded.contains(element.id())
    }
}

impl GlobalDispatch<RixCapturePolicyManagerV1, ()> for RixState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<RixCapturePolicyManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<RixCapturePolicyManagerV1, ()> for RixState {
    fn request(
        _: &mut Self,
        _: &Client,
        manager: &RixCapturePolicyManagerV1,
        request: rix_capture_policy_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let rix_capture_policy_manager_v1::Request::GetSurface { id, surface } = request {
            with_states(&surface, |states| {
                states.data_map.insert_if_missing(Slot::default);
                let mut slot = states.data_map.get::<Slot>().unwrap().0.lock().unwrap();
                if slot.as_ref().is_some_and(wayland_server::Weak::is_alive) {
                    init.init(
                        id,
                        SurfaceData {
                            surface: surface.clone(),
                        },
                    );
                    manager.post_error(
                        rix_capture_policy_manager_v1::Error::AlreadyExists,
                        "Surface already has a capture policy object",
                    );
                    return;
                }
                let object = init.init(
                    id,
                    SurfaceData {
                        surface: surface.clone(),
                    },
                );
                *slot = Some(object.downgrade());
            });
        }
    }
}

impl Dispatch<RixCapturePolicySurfaceV1, SurfaceData> for RixState {
    fn request(
        _: &mut Self,
        _: &Client,
        object: &RixCapturePolicySurfaceV1,
        request: rix_capture_policy_surface_v1::Request,
        data: &SurfaceData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let rix_capture_policy_surface_v1::Request::SetPolicy { policy, serial } = request {
            if !data.surface.is_alive() {
                object.post_error(
                    rix_capture_policy_surface_v1::Error::SurfaceDestroyed,
                    "Surface has been destroyed",
                );
                return;
            }
            let WEnum::Value(policy) = policy else {
                object.post_error(
                    rix_capture_policy_surface_v1::Error::InvalidPolicy,
                    "Unknown capture policy",
                );
                return;
            };
            with_states(&data.surface, |states| {
                states.cached_state.get::<CapturePolicy>().pending().update = Some(Update {
                    policy,
                    acknowledgement: Some((object.downgrade(), serial)),
                });
            });
        }
    }

    fn destroyed(
        _: &mut Self,
        _: wayland_server::backend::ClientId,
        object: &RixCapturePolicySurfaceV1,
        data: &SurfaceData,
    ) {
        if data.surface.is_alive() {
            with_states(&data.surface, |states| {
                let mut slot = states.data_map.get::<Slot>().unwrap().0.lock().unwrap();
                if slot.as_ref().is_some_and(|owner| owner == object) {
                    *slot = None;

                    states.cached_state.get::<CapturePolicy>().pending().update = None;
                }
            });
        }
    }
}

#[cfg(test)]
#[path = "capture_policy_tests.rs"]
mod tests;
