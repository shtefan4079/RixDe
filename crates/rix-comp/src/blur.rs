use crate::state::RixState;
use smithay::wayland::compositor::{get_region_attributes, with_states, RegionAttributes};
use std::sync::Mutex;
use wayland_protocols_plasma::blur::server::{
    org_kde_kwin_blur::{self, OrgKdeKwinBlur},
    org_kde_kwin_blur_manager::{self, OrgKdeKwinBlurManager},
};
use wayland_server::{
    backend::ObjectId, protocol::wl_surface::WlSurface, Client, DataInit, Dispatch, DisplayHandle,
    GlobalDispatch, New, Resource,
};

#[derive(Default)]
struct AppliedBlur {
    owner: Option<ObjectId>,
    region: Option<Option<RegionAttributes>>,
}
#[derive(Default)]
pub struct BlurRegion(Mutex<AppliedBlur>);
pub struct BlurObject {
    surface: WlSurface,
    pending: Mutex<Option<RegionAttributes>>,
}

pub fn install(dh: &DisplayHandle) {
    dh.create_global::<RixState, OrgKdeKwinBlurManager, _>(1, ());
}
pub fn region(surface: &WlSurface) -> Option<Option<RegionAttributes>> {
    with_states(surface, |data| {
        data.data_map
            .get::<BlurRegion>()
            .and_then(|blur| blur.0.lock().unwrap().region.clone())
    })
}
fn set(surface: &WlSurface, owner: Option<ObjectId>, region: Option<Option<RegionAttributes>>) {
    with_states(surface, |data| {
        data.data_map.insert_if_missing(BlurRegion::default);
        *data.data_map.get::<BlurRegion>().unwrap().0.lock().unwrap() =
            AppliedBlur { owner, region };
    });
}
fn release(surface: &WlSurface, owner: ObjectId) {
    with_states(surface, |data| {
        if let Some(blur) = data.data_map.get::<BlurRegion>() {
            let mut applied = blur.0.lock().unwrap();

            if applied.owner.as_ref() == Some(&owner) {
                *applied = AppliedBlur::default();
            }
        }
    });
}
impl GlobalDispatch<OrgKdeKwinBlurManager, ()> for RixState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<OrgKdeKwinBlurManager>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}
impl Dispatch<OrgKdeKwinBlurManager, ()> for RixState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &OrgKdeKwinBlurManager,
        request: org_kde_kwin_blur_manager::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            org_kde_kwin_blur_manager::Request::Create { id, surface } => {
                init.init(
                    id,
                    BlurObject {
                        surface,
                        pending: Mutex::new(None),
                    },
                );
            }
            org_kde_kwin_blur_manager::Request::Unset { surface } => set(&surface, None, None),
            _ => {}
        }
    }
}
impl Dispatch<OrgKdeKwinBlur, BlurObject> for RixState {
    fn request(
        _: &mut Self,
        _: &Client,
        resource: &OrgKdeKwinBlur,
        request: org_kde_kwin_blur::Request,
        data: &BlurObject,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            org_kde_kwin_blur::Request::SetRegion { region } => {
                *data.pending.lock().unwrap() = region.as_ref().map(get_region_attributes);
            }
            org_kde_kwin_blur::Request::Commit => set(
                &data.surface,
                Some(resource.id()),
                Some(data.pending.lock().unwrap().clone()),
            ),
            org_kde_kwin_blur::Request::Release => release(&data.surface, resource.id()),
            _ => {}
        }
    }
}
