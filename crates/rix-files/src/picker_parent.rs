use crate::FilesWindow;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use slint::{
    winit_030::{winit::platform::wayland::WindowExtWayland, WinitWindowAccessor},
    ComponentHandle, Timer, TimerMode,
};
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};
use wayland_client::{
    backend::{Backend, ObjectId, WaylandError},
    protocol::{wl_callback, wl_registry, wl_surface},
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols::xdg::{
    dialog::v1::client::{xdg_dialog_v1::XdgDialogV1, xdg_wm_dialog_v1::XdgWmDialogV1},
    foreign::{
        zv1::client::{zxdg_imported_v1, zxdg_importer_v1::ZxdgImporterV1},
        zv2::client::{zxdg_imported_v2, zxdg_importer_v2::ZxdgImporterV2},
    },
    shell::client::xdg_toplevel::XdgToplevel,
};

const TICK: Duration = Duration::from_millis(50);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

pub struct PickerParent {
    timer: Timer,
    window: slint::Weak<FilesWindow>,
    handle: String,
    modal: bool,
    bridge: Option<Bridge>,
    started: Instant,
    failed: bool,
    on_parent_closed: Rc<dyn Fn()>,
}

pub fn install(
    window: &FilesWindow,
    parent: &str,
    modal: bool,
    on_parent_closed: impl Fn() + 'static,
) -> Option<Rc<RefCell<PickerParent>>> {
    let handle = parent
        .strip_prefix("wayland:")
        .filter(|handle| !handle.is_empty())?;
    let parent = Rc::new(RefCell::new(PickerParent {
        timer: Timer::default(),
        window: window.as_weak(),
        handle: handle.into(),
        modal,
        bridge: None,
        started: Instant::now(),
        failed: false,
        on_parent_closed: Rc::new(on_parent_closed),
    }));
    let weak = Rc::downgrade(&parent);
    parent
        .borrow()
        .timer
        .start(TimerMode::Repeated, TICK, move || {
            let Some(parent) = weak.upgrade() else {
                return;
            };
            let callback = {
                let mut parent = parent.borrow_mut();
                parent.tick().then(|| parent.on_parent_closed.clone())
            };
            if let Some(callback) = callback {
                callback();
            }
        });
    Some(parent)
}

impl PickerParent {
    fn tick(&mut self) -> bool {
        if self.failed {
            return false;
        }
        let Some(window) = self.window.upgrade() else {
            self.timer.stop();
            self.bridge = None;
            return false;
        };
        if self.bridge.is_none() {
            if !window.window().is_visible() {
                self.started = Instant::now();
                return false;
            }
            match Bridge::from_window(&window) {
                Ok(Some(bridge)) => {
                    self.bridge = Some(bridge);
                    self.started = Instant::now();
                }
                Ok(None) if self.started.elapsed() < PROBE_TIMEOUT => return false,
                Ok(None) => {
                    self.fail("Native picker window handles are unavailable");
                    return false;
                }
                Err(error) => {
                    self.fail(&error);
                    return false;
                }
            }
        }
        let bridge = self.bridge.as_mut().expect("initialized picker bridge");
        match bridge.poll(&self.handle, self.modal) {
            Ok(ParentState::Closed) => {
                self.timer.stop();
                self.failed = true;
                true
            }
            Ok(ParentState::Ready) => false,
            Ok(ParentState::Pending) if self.started.elapsed() < PROBE_TIMEOUT => false,
            Ok(ParentState::Pending) => {
                self.fail("Compositor did not acknowledge the picker parent");
                false
            }
            Err(error) => {
                self.fail(&error);
                false
            }
        }
    }

    fn fail(&mut self, error: &str) {
        tracing::debug!(
            error,
            "Picker parent is unavailable; using a standalone window"
        );
        self.timer.stop();
        self.bridge = None;
        self.failed = true;
    }
}

#[derive(Clone, Copy)]
enum SyncPoint {
    Registry,
    Imported,
}
#[derive(Default)]
struct ProtocolState {
    importer_v1: Option<u32>,
    importer_v2: Option<u32>,
    dialog_manager: Option<u32>,
    registry_ready: bool,
    imported_ready: bool,
    parent_closed: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if version >= 1 {
                match interface.as_str() {
                    "zxdg_importer_v2" => state.importer_v2 = Some(name),
                    "zxdg_importer_v1" => state.importer_v1 = Some(name),
                    "xdg_wm_dialog_v1" => state.dialog_manager = Some(name),
                    _ => {}
                }
            }
        }
    }
}
impl Dispatch<wl_callback::WlCallback, SyncPoint> for ProtocolState {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        point: &SyncPoint,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            match point {
                SyncPoint::Registry => state.registry_ready = true,
                SyncPoint::Imported => state.imported_ready = true,
            }
        }
    }
}
impl Dispatch<zxdg_imported_v1::ZxdgImportedV1, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        _: &zxdg_imported_v1::ZxdgImportedV1,
        event: zxdg_imported_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_imported_v1::Event::Destroyed = event {
            state.parent_closed = true;
        }
    }
}
impl Dispatch<zxdg_imported_v2::ZxdgImportedV2, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        _: &zxdg_imported_v2::ZxdgImportedV2,
        event: zxdg_imported_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_imported_v2::Event::Destroyed = event {
            state.parent_closed = true;
        }
    }
}
wayland_client::delegate_noop!(ProtocolState: ignore ZxdgImporterV1);
wayland_client::delegate_noop!(ProtocolState: ignore ZxdgImporterV2);
wayland_client::delegate_noop!(ProtocolState: ignore XdgWmDialogV1);
wayland_client::delegate_noop!(ProtocolState: ignore XdgDialogV1);

enum Imported {
    V1(zxdg_imported_v1::ZxdgImportedV1),
    V2(zxdg_imported_v2::ZxdgImportedV2),
}
impl Imported {
    fn set_parent_of(&self, child: &wl_surface::WlSurface) {
        match self {
            Self::V1(parent) => parent.set_parent_of(child),
            Self::V2(parent) => parent.set_parent_of(child),
        }
    }
    fn destroy(&self) {
        match self {
            Self::V1(parent) => parent.destroy(),
            Self::V2(parent) => parent.destroy(),
        }
    }
}
enum ParentState {
    Pending,
    Ready,
    Closed,
}
struct Bridge {
    imported: Option<Imported>,
    dialog: Option<XdgDialogV1>,
    importer_v1: Option<ZxdgImporterV1>,
    importer_v2: Option<ZxdgImporterV2>,
    dialog_manager: Option<XdgWmDialogV1>,
    registry: Option<wl_registry::WlRegistry>,
    surface: Option<wl_surface::WlSurface>,
    toplevel: Option<XdgToplevel>,
    queue: Option<EventQueue<ProtocolState>>,
    connection: Option<Connection>,
    state: ProtocolState,
    provider: Option<slint::WindowHandle>,
}

impl Bridge {
    fn from_window(window: &FilesWindow) -> Result<Option<Self>, String> {
        let provider = window.window().window_handle();
        let (Ok(display), Ok(surface)) = (provider.display_handle(), provider.window_handle())
        else {
            return Ok(None);
        };
        let (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(surface)) =
            (display.as_raw(), surface.as_raw())
        else {
            return Err("Picker is not a native Wayland window".into());
        };
        let Some(toplevel) = window
            .window()
            .with_winit_window(|window| window.xdg_toplevel())
            .flatten()
        else {
            return Ok(None);
        };

        let (backend, surface_id, toplevel_id) = unsafe {
            (
                Backend::from_foreign_display(display.display.as_ptr().cast()),
                ObjectId::from_ptr(
                    wl_surface::WlSurface::interface(),
                    surface.surface.as_ptr().cast(),
                )
                .map_err(|error| error.to_string())?,
                ObjectId::from_ptr(XdgToplevel::interface(), toplevel.as_ptr().cast())
                    .map_err(|error| error.to_string())?,
            )
        };
        let connection = Connection::from_backend(backend);
        let surface = wl_surface::WlSurface::from_id(&connection, surface_id)
            .map_err(|error| error.to_string())?;
        let toplevel =
            XdgToplevel::from_id(&connection, toplevel_id).map_err(|error| error.to_string())?;
        let queue = connection.new_event_queue::<ProtocolState>();
        let registry = connection.display().get_registry(&queue.handle(), ());
        connection
            .display()
            .sync(&queue.handle(), SyncPoint::Registry);
        let bridge = Self {
            imported: None,
            dialog: None,
            importer_v1: None,
            importer_v2: None,
            dialog_manager: None,
            registry: Some(registry),
            surface: Some(surface),
            toplevel: Some(toplevel),
            queue: Some(queue),
            connection: Some(connection),
            state: ProtocolState::default(),
            provider: Some(provider),
        };
        bridge.flush()?;
        Ok(Some(bridge))
    }

    fn poll(&mut self, handle: &str, modal: bool) -> Result<ParentState, String> {
        self.queue
            .as_mut()
            .expect("picker queue")
            .dispatch_pending(&mut self.state)
            .map_err(|error| error.to_string())?;
        if self.state.parent_closed {
            return Ok(ParentState::Closed);
        }
        if self.state.registry_ready && self.imported.is_none() {
            let qh = self.queue.as_ref().expect("picker queue").handle();
            let registry = self.registry.as_ref().expect("picker registry");
            let imported = if let Some(name) = self.state.importer_v2 {
                let importer = registry.bind::<ZxdgImporterV2, _, _>(name, 1, &qh, ());
                let parent = importer.import_toplevel(handle.into(), &qh, ());
                self.importer_v2 = Some(importer);
                Imported::V2(parent)
            } else if let Some(name) = self.state.importer_v1 {
                let importer = registry.bind::<ZxdgImporterV1, _, _>(name, 1, &qh, ());
                let parent = importer.import(handle.into(), &qh, ());
                self.importer_v1 = Some(importer);
                Imported::V1(parent)
            } else {
                return Err("Compositor does not support xdg-foreign".into());
            };
            imported.set_parent_of(self.surface.as_ref().expect("picker surface"));
            self.imported = Some(imported);
            if let Some(name) = self.state.dialog_manager {
                let manager = registry.bind::<XdgWmDialogV1, _, _>(name, 1, &qh, ());
                let dialog = manager.get_xdg_dialog(
                    self.toplevel.as_ref().expect("picker toplevel"),
                    &qh,
                    (),
                );
                if modal {
                    dialog.set_modal();
                } else {
                    dialog.unset_modal();
                }
                self.dialog_manager = Some(manager);
                self.dialog = Some(dialog);
            }
            self.connection
                .as_ref()
                .expect("picker connection")
                .display()
                .sync(&qh, SyncPoint::Imported);
        }
        self.flush()?;
        Ok(if self.state.imported_ready {
            ParentState::Ready
        } else {
            ParentState::Pending
        })
    }

    fn flush(&self) -> Result<(), String> {
        match self.connection.as_ref().expect("picker connection").flush() {
            Ok(()) => Ok(()),
            Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Ok(())
            }
            Err(error) => Err(error.to_string()),
        }
    }
}
impl Drop for Bridge {
    fn drop(&mut self) {
        if let Some(dialog) = self.dialog.take() {
            dialog.destroy();
        }
        if let Some(parent) = self.imported.take() {
            parent.destroy();
        }
        if let Some(importer) = self.importer_v1.take() {
            importer.destroy();
        }
        if let Some(importer) = self.importer_v2.take() {
            importer.destroy();
        }
        if let Some(manager) = self.dialog_manager.take() {
            manager.destroy();
        }
        if let Some(connection) = &self.connection {
            let _ = connection.flush();
        }
        self.registry = None;
        self.surface = None;
        self.toplevel = None;
        self.queue = None;
        self.connection = None;
        self.provider = None;
    }
}
