use crate::state::RixState;
use rix_protocols::observer_server::{
    rix_toplevel_observer_manager_v1::{self, RixToplevelObserverManagerV1},
    rix_toplevel_observer_v1::{self, RixToplevelObserverV1},
};
use smithay::utils::{Logical, Rectangle};
use smithay::{
    backend::renderer::element::Id,
    desktop::PopupManager,
    wayland::compositor::{with_states, with_surface_tree_downward, TraversalAction},
};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
};
use wayland_server::protocol::wl_surface::WlSurface;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

#[derive(Clone, Debug, PartialEq)]
struct Snapshot {
    output: String,
    geometry: Rectangle<i32, Logical>,
    output_geometry: Rectangle<i32, Logical>,
    active: bool,
}

pub struct ObserverData {
    pid: Option<i32>,
    last: Mutex<Option<Option<Snapshot>>>,
    visible: Arc<AtomicBool>,
}

#[derive(Default)]
struct AttachedOverlay(Mutex<Option<Weak<AtomicBool>>>);

pub fn visible(surface: &WlSurface) -> bool {
    let mut ancestor = Some(surface.clone());
    let mut visited = HashSet::new();
    while let Some(surface) = ancestor {
        if !surface.is_alive() || !visited.insert(surface.id()) {
            return false;
        }
        let visible = with_states(&surface, |states| {
            states.data_map.get::<AttachedOverlay>().is_none_or(|data| {
                data.0.lock().unwrap().as_ref().is_none_or(|observer| {
                    observer
                        .upgrade()
                        .is_some_and(|visible| visible.load(Ordering::Acquire))
                })
            })
        });
        if !visible {
            return false;
        }
        ancestor = crate::capture_policy::surface_parent(&surface);
    }
    true
}

pub fn hidden_elements(state: &RixState) -> HashSet<Id> {
    let mut hidden = HashSet::new();
    for root in state
        .attached_overlays
        .iter()
        .filter(|surface| surface.is_alive() && !visible(surface))
    {
        for surface in std::iter::once(root.clone()).chain(
            PopupManager::popups_for_surface(root).map(|(popup, _)| popup.wl_surface().clone()),
        ) {
            with_surface_tree_downward(
                &surface,
                (),
                |_, _, _| TraversalAction::DoChildren(()),
                |surface, _, _| {
                    hidden.insert(Id::from_wayland_resource(surface));
                },
                |_, _, _| true,
            );
        }
    }
    hidden
}

pub fn install(dh: &DisplayHandle) {
    dh.create_global::<RixState, RixToplevelObserverManagerV1, _>(1, ());
}

fn snapshot(state: &RixState, pid: i32) -> Option<Snapshot> {
    let (id, window, geometry) = state
        .windows
        .entries
        .iter()
        .filter_map(|(id, window)| {
            let surface = window.toplevel()?.wl_surface();
            let credentials = surface
                .client()?
                .get_credentials(&state.display_handle)
                .ok()?;
            if credentials.pid != pid {
                return None;
            }
            let location = state
                .space
                .element_location(window)
                .or_else(|| state.windows.minimized.get(id).copied())?;
            let geometry = Rectangle::new(location, window.geometry().size);
            (geometry.size.w > 0 && geometry.size.h > 0).then_some((*id, window, geometry))
        })
        .max_by_key(|(_, _, geometry)| i64::from(geometry.size.w) * i64::from(geometry.size.h))?;
    let (output, output_geometry, intersection) = state
        .space
        .outputs()
        .filter_map(|output| {
            let bounds = state.space.output_geometry(output)?;
            let intersection = geometry
                .intersection(bounds)
                .map_or(0, |rect| i64::from(rect.size.w) * i64::from(rect.size.h));
            Some((output, bounds, intersection))
        })
        .max_by_key(|(_, _, area)| *area)?;
    Some(Snapshot {
        output: output.name(),
        geometry,
        output_geometry,
        active: !state.lock.active
            && state.windows.active == Some(id)
            && state.space.element_location(window).is_some()
            && intersection > 0,
    })
}

fn send(state: &RixState, observer: &RixToplevelObserverV1) {
    let Some(data) = observer.data::<ObserverData>() else {
        return;
    };
    let next = data.pid.and_then(|pid| snapshot(state, pid));
    data.visible.store(
        next.as_ref().is_some_and(|snapshot| snapshot.active),
        Ordering::Release,
    );
    let mut last = data.last.lock().unwrap();
    if last.as_ref() == Some(&next) {
        return;
    }
    if let Some(next) = &next {
        observer.state(
            next.output.clone(),
            next.geometry.loc.x,
            next.geometry.loc.y,
            next.geometry.size.w,
            next.geometry.size.h,
            next.output_geometry.loc.x,
            next.output_geometry.loc.y,
            u32::from(next.active),
        );
    } else {
        observer.unavailable();
    }
    *last = Some(next);
}

pub fn refresh(state: &mut RixState) {
    state.refresh_foreground_windows();
    state.toplevel_observers.retain(Resource::is_alive);
    state.attached_overlays.retain(Resource::is_alive);
    for observer in &state.toplevel_observers {
        send(state, observer);
    }
}

impl GlobalDispatch<RixToplevelObserverManagerV1, ()> for RixState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<RixToplevelObserverManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<RixToplevelObserverManagerV1, ()> for RixState {
    fn request(
        state: &mut Self,
        client: &Client,
        _: &RixToplevelObserverManagerV1,
        request: rix_toplevel_observer_manager_v1::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let rix_toplevel_observer_manager_v1::Request::Observe { id } = request {
            let pid = client
                .get_credentials(dh)
                .ok()
                .map(|credentials| credentials.pid);
            let observer = init.init(
                id,
                ObserverData {
                    pid,
                    last: Mutex::default(),
                    visible: Arc::new(AtomicBool::new(false)),
                },
            );
            send(state, &observer);
            state.toplevel_observers.push(observer);
        }
    }
}

impl Dispatch<RixToplevelObserverV1, ObserverData> for RixState {
    fn request(
        state: &mut Self,
        _: &Client,
        observer: &RixToplevelObserverV1,
        request: rix_toplevel_observer_v1::Request,
        data: &ObserverData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            rix_toplevel_observer_v1::Request::AttachOverlay { surface } => {
                if smithay::wayland::compositor::get_role(&surface)
                    != Some(smithay::wayland::shell::wlr_layer::LAYER_SURFACE_ROLE)
                {
                    observer.post_error(
                        rix_toplevel_observer_v1::Error::InvalidRole,
                        "An overlay must be a layer-shell surface",
                    );
                    return;
                }
                with_states(&surface, |states| {
                    states.data_map.insert_if_missing(AttachedOverlay::default);
                    *states
                        .data_map
                        .get::<AttachedOverlay>()
                        .unwrap()
                        .0
                        .lock()
                        .unwrap() = Some(Arc::downgrade(&data.visible));
                });
                if !state.attached_overlays.contains(&surface) {
                    state.attached_overlays.push(surface);
                }
            }
            rix_toplevel_observer_v1::Request::DetachOverlay { surface } => {
                with_states(&surface, |states| {
                    if let Some(attachment) = states.data_map.get::<AttachedOverlay>() {
                        let mut attachment = attachment.0.lock().unwrap();
                        if attachment
                            .as_ref()
                            .is_some_and(|old| old.ptr_eq(&Arc::downgrade(&data.visible)))
                        {
                            *attachment = None;
                        }
                    }
                });
            }
            _ => {}
        }
    }

    fn destroyed(
        _: &mut Self,
        _: wayland_server::backend::ClientId,
        _: &RixToplevelObserverV1,
        data: &ObserverData,
    ) {
        data.visible.store(false, Ordering::Release);
    }
}
