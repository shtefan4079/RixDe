use std::collections::HashMap;
use std::os::fd::AsFd;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, RwLock,
};

use drm::control::atomic::AtomicModeReq;
use drm::control::{
    connector, crtc, framebuffer, plane, property, AtomicCommitFlags, Device as ControlDevice, ResourceHandle,
};

use super::DrmDeviceFd;
use crate::backend::drm::error::AccessError;
use crate::{backend::drm::error::Error, utils::DevPath};

use tracing::{debug, error, info_span, trace};

// Blob IDs are not references: reset_state or the previous owner can destroy
// them. Keep their bytes, then create our own blobs for the restore commit.
#[derive(Debug)]
enum SavedValue {
    Raw(property::RawValue),
    Blob(Vec<u8>),
}

type SavedProperties = Vec<(property::Handle, SavedValue)>;
type OldState = (
    Vec<(connector::Handle, SavedProperties)>,
    Vec<(crtc::Handle, SavedProperties)>,
    Vec<(framebuffer::Handle, SavedProperties)>,
    Vec<(plane::Handle, SavedProperties)>,
);

#[derive(Clone, Debug, Default)]
pub struct PropMapping {
    pub connectors: HashMap<connector::Handle, HashMap<String, property::Handle>>,
    pub crtcs: HashMap<crtc::Handle, HashMap<String, property::Handle>>,
    pub planes: HashMap<plane::Handle, HashMap<String, property::Handle>>,
}

impl PropMapping {
    pub(crate) fn conn_prop_handle(
        &self,
        handle: connector::Handle,
        name: &'static str,
    ) -> Result<property::Handle, Error> {
        self.connectors
            .get(&handle)
            .ok_or(Error::UnknownConnector(handle))?
            .get(name)
            .ok_or_else(|| Error::UnknownProperty {
                handle: handle.into(),
                name,
            })
            .copied()
    }

    pub(crate) fn crtc_prop_handle(
        &self,
        handle: crtc::Handle,
        name: &'static str,
    ) -> Result<property::Handle, Error> {
        self.crtcs
            .get(&handle)
            .ok_or(Error::UnknownCrtc(handle))?
            .get(name)
            .ok_or_else(|| Error::UnknownProperty {
                handle: handle.into(),
                name,
            })
            .copied()
    }

    pub(crate) fn plane_prop_handle(
        &self,
        handle: plane::Handle,
        name: &'static str,
    ) -> Result<property::Handle, Error> {
        self.planes
            .get(&handle)
            .ok_or(Error::UnknownPlane(handle))?
            .get(name)
            .ok_or_else(|| Error::UnknownProperty {
                handle: handle.into(),
                name,
            })
            .copied()
    }
}

#[derive(Debug)]
pub struct AtomicDrmDevice {
    pub(crate) fd: DrmDeviceFd,
    pub(crate) active: Arc<AtomicBool>,
    old_state: OldState,
    pub(crate) prop_mapping: Arc<RwLock<PropMapping>>,
    pub(super) span: tracing::Span,
}

impl AtomicDrmDevice {
    pub fn new(fd: DrmDeviceFd, active: Arc<AtomicBool>, disable_connectors: bool) -> Result<Self, Error> {
        let span = info_span!("drm_atomic");
        let mut dev = AtomicDrmDevice {
            fd,
            active,
            old_state: (Vec::new(), Vec::new(), Vec::new(), Vec::new()),
            prop_mapping: Default::default(),
            span,
        };
        let _guard = dev.span.enter();

        // Enumerate (and save) the current device state.
        let res_handles = dev.fd.resource_handles().map_err(|source| {
            Error::Access(AccessError {
                errmsg: "Error loading drm resources",
                dev: dev.fd.dev_path(),
                source,
            })
        })?;

        let planes = dev.fd.plane_handles().map_err(|source| {
            Error::Access(AccessError {
                errmsg: "Error loading planes",
                dev: dev.fd.dev_path(),
                source,
            })
        })?;

        let mut old_state = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut mapping = dev.prop_mapping.write().unwrap();

        // This helper function takes a snapshot of the current device properties.
        // (everything in the atomic api is set via properties.)
        add_props(&dev.fd, res_handles.connectors(), &mut old_state.0)?;
        add_props(&dev.fd, res_handles.crtcs(), &mut old_state.1)?;
        add_props(&dev.fd, res_handles.framebuffers(), &mut old_state.2)?;
        add_props(&dev.fd, &planes, &mut old_state.3)?;

        // And because the mapping is not consistent across devices,
        // we also need to lookup the handle for a property name.
        // And we do this a fair bit, so lets cache that mapping.
        map_props(&dev.fd, res_handles.connectors(), &mut mapping.connectors)?;
        map_props(&dev.fd, res_handles.crtcs(), &mut mapping.crtcs)?;
        map_props(&dev.fd, &planes, &mut mapping.planes)?;

        dev.old_state = old_state;
        trace!("Mapping: {:#?}", mapping);

        drop(mapping);

        // If the user does not explicitly requests us to skip this,
        // we clear out the complete connector<->crtc mapping on device creation.
        //
        // The reason is, that certain operations may be racy otherwise. Surfaces can
        // exist on different threads: as a result, we cannot really enumerate the current state
        // (it might be changed on another thread during the enumeration). And commits can fail,
        // if e.g. a connector is already bound to another surface, which is difficult to analyse at runtime.
        //
        // An easy workaround is to set a known state on device creation, so we can only
        // run into these errors on our own and not because previous compositors left the device
        // in a funny state.
        if disable_connectors {
            debug!("Resetting drm device to known state");
            dev.reset_state()?;
        }

        drop(_guard);
        Ok(dev)
    }

    pub(super) fn reset_state(&self) -> Result<(), Error> {
        // reset state sets the connectors into a known state (all disabled),
        // for the same reasons we do this on device creation.
        //
        // We might end up with conflicting commit requirements, if we want to restore our state,
        // on top of the state the previous compositor left the device in.
        // This is because we do commits per surface and not per device, so we do a global
        // commit here, to fix any conflicts.
        let res_handles = self.fd.resource_handles().map_err(|source| {
            Error::Access(AccessError {
                errmsg: "Error loading drm resources",
                dev: self.fd.dev_path(),
                source,
            })
        })?;
        let plane_handles = self.fd.plane_handles().map_err(|source| {
            Error::Access(AccessError {
                errmsg: "Error loading drm plane resources",
                dev: self.fd.dev_path(),
                source,
            })
        })?;

        let mut prop_mapping = self.prop_mapping.write().unwrap();

        // Make sure the mapping is up to date
        prop_mapping.connectors.clear();
        map_props(&self.fd, res_handles.connectors(), &mut prop_mapping.connectors)?;

        // Disable all connectors (otherwise we might run into conflicting commits when restarting the rendering loop)
        let mut req = AtomicModeReq::new();
        for conn in res_handles.connectors() {
            let prop = prop_mapping
                .conn_prop_handle(*conn, "CRTC_ID")
                .expect("Unknown property CRTC_ID");
            req.add_property(*conn, prop, property::Value::CRTC(None));
        }
        // Disable all planes
        for plane in plane_handles {
            let prop = prop_mapping
                .plane_prop_handle(plane, "CRTC_ID")
                .expect("Unknown property CRTC_ID");
            req.add_property(plane, prop, property::Value::CRTC(None));

            let prop = prop_mapping
                .plane_prop_handle(plane, "FB_ID")
                .expect("Unknown property FB_ID");
            req.add_property(plane, prop, property::Value::Framebuffer(None));
        }
        // A crtc without a connector has no mode, we also need to reset that.
        // Otherwise the commit will not be accepted.
        for crtc in res_handles.crtcs() {
            let mode_prop = prop_mapping
                .crtc_prop_handle(*crtc, "MODE_ID")
                .expect("Unknown property MODE_ID");
            let active_prop = prop_mapping
                .crtc_prop_handle(*crtc, "ACTIVE")
                .expect("Unknown property ACTIVE");
            req.add_property(*crtc, active_prop, property::Value::Boolean(false));
            req.add_property(*crtc, mode_prop, property::Value::Unknown(0));
        }
        self.fd
            .atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)
            .map_err(|source| {
                Error::Access(AccessError {
                    errmsg: "Failed to disable connectors",
                    dev: self.fd.dev_path(),
                    source,
                })
            })?;

        Ok(())
    }
}

impl Drop for AtomicDrmDevice {
    fn drop(&mut self) {
        if self.active.load(Ordering::SeqCst) {
            let _guard = self.span.enter();

            // Here we restore the card/tty's to it's previous state.
            // In case e.g. getty was running on the tty sets the correct framebuffer again,
            // so that getty will be visible.
            // We do exit correctly if this fails, but the user will be presented with
            // a black screen if no display handler takes control again.

            // create an atomic mode request consisting of all properties we captured on creation.
            // TODO, check current connector status and remove deactivated connectors from this req.

            debug!("Device still active, trying to restore previous state");
            let mut req = AtomicModeReq::new();
            let mut blobs = Vec::new();
            fn add_multiple_props<T: ResourceHandle>(
                fd: &DrmDeviceFd,
                req: &mut AtomicModeReq,
                old_state: &mut [(T, SavedProperties)],
                blobs: &mut Vec<u64>,
            ) -> std::io::Result<()> {
                for (handle, properties) in old_state {
                    for (property, value) in properties {
                        let raw = match value {
                            SavedValue::Raw(raw) => *raw,
                            SavedValue::Blob(data) => {
                                let blob = drm_ffi::mode::create_property_blob(fd.as_fd(), data)?;
                                let id = u64::from(blob.blob_id);
                                blobs.push(id);
                                id
                            }
                        };
                        req.add_raw_property((*handle).into(), *property, raw);
                    }
                }
                Ok(())
            }

            let result = (|| {
                add_multiple_props(&self.fd, &mut req, &mut self.old_state.0, &mut blobs)?;
                add_multiple_props(&self.fd, &mut req, &mut self.old_state.1, &mut blobs)?;
                add_multiple_props(&self.fd, &mut req, &mut self.old_state.2, &mut blobs)?;
                add_multiple_props(&self.fd, &mut req, &mut self.old_state.3, &mut blobs)?;
                trace!("Previous state: {:?}", req);
                self.fd.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)
            })();
            if let Err(err) = result {
                error!("Failed to restore previous state. Error: {}", err);
            }
            // The applied atomic state retains its own references. Release our
            // handles on both success and failure, including a partial build.
            for blob in blobs {
                let _ = self.fd.destroy_property_blob(blob);
            }
        }
    }
}

// These names are part of the kernel DRM ABI, not monitor-specific policy.
// ACTIVE + CRTC_ID restore power/routing atomically; DPMS is legacy-only.
fn restorable_property(name: &[u8], mutable: bool) -> bool {
    mutable
        && !matches!(
            name,
            b"DPMS"
                | b"IN_FENCE_FD"
                | b"OUT_FENCE_PTR"
                | b"WRITEBACK_OUT_FENCE_PTR"
                | b"WRITEBACK_FB_ID"
                | b"FB_DAMAGE_CLIPS"
        )
}

#[cfg(test)]
mod restore_tests {
    use super::restorable_property;
    #[test]
    fn atomic_restore_excludes_metadata_and_per_commit_handles() {
        for name in [b"type".as_slice(), b"EDID", b"IN_FORMATS", b"non-desktop"] {
            assert!(!restorable_property(name, false));
        }
        for name in [
            b"DPMS".as_slice(),
            b"IN_FENCE_FD",
            b"OUT_FENCE_PTR",
            b"WRITEBACK_OUT_FENCE_PTR",
            b"WRITEBACK_FB_ID",
            b"FB_DAMAGE_CLIPS",
        ] {
            assert!(!restorable_property(name, true));
        }
        for name in [
            b"ACTIVE".as_slice(),
            b"MODE_ID",
            b"CRTC_ID",
            b"FB_ID",
            b"SRC_W",
            b"CRTC_X",
            b"rotation",
            b"VRR_ENABLED",
        ] {
            assert!(restorable_property(name, true));
        }
    }
}

// Add all properties of given handles to a given drm resource type to state.
// You may use this to snapshot the current state of the drm device (fully or partially).
fn add_props<D, T>(fd: &D, handles: &[T], state: &mut Vec<(T, SavedProperties)>) -> Result<(), Error>
where
    D: DevPath + ControlDevice,
    T: ResourceHandle,
{
    let snapshot = || -> std::io::Result<Vec<(T, SavedProperties)>> {
        let mut saved = Vec::with_capacity(handles.len());
        for handle in handles {
            let mut properties = Vec::new();
            for (property, raw) in fd.get_properties(*handle)? {
                let info = fd.get_property(property)?;
                // GETPROPERTIES also returns immutable metadata, legacy DPMS
                // and per-commit handles. None belongs in an atomic restore.
                if !restorable_property(info.name().to_bytes(), info.mutable()) {
                    continue;
                }
                let value = if raw != 0 && matches!(info.value_type(), property::ValueType::Blob) {
                    SavedValue::Blob(fd.get_property_blob(raw)?)
                } else {
                    SavedValue::Raw(raw)
                };
                properties.push((property, value));
            }
            saved.push((*handle, properties));
        }
        Ok(saved)
    };
    *state = snapshot().map_err(|source| {
        Error::Access(AccessError {
            errmsg: "Error saving DRM properties and blobs",
            dev: fd.dev_path(),
            source,
        })
    })?;
    Ok(())
}

/// Create a mapping of property names and handles for given handles of a given drm resource type.
/// You may use this to easily lookup properties by name instead of going through this procedure manually.
pub(in crate::backend::drm) fn map_props<D, T>(
    fd: &D,
    handles: &[T],
    mapping: &mut HashMap<T, HashMap<String, property::Handle>>,
) -> Result<(), Error>
where
    D: DevPath + ControlDevice,
    T: ResourceHandle + Eq + std::hash::Hash,
{
    handles
        .iter()
        .map(|x| (x, fd.get_properties(*x)))
        .try_for_each(|(handle, props)| {
            let mut map = HashMap::new();
            match props {
                Ok(props) => {
                    let (prop_handles, _) = props.as_props_and_values();
                    for prop in prop_handles {
                        if let Ok(info) = fd.get_property(*prop) {
                            let name = info.name().to_string_lossy().into_owned();
                            map.insert(name, *prop);
                        }
                    }
                    mapping.insert(*handle, map);
                    Ok(())
                }
                Err(err) => Err(err),
            }
        })
        .map_err(|source| {
            Error::Access(AccessError {
                errmsg: "Error reading properties",
                dev: fd.dev_path(),
                source,
            })
        })
}
