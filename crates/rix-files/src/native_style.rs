use crate::FilesWindow;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use rix_config::ThemeSettings;
use slint::{ComponentHandle, Timer, TimerMode};
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
use wayland_protocols_plasma::blur::client::{
    org_kde_kwin_blur::OrgKdeKwinBlur, org_kde_kwin_blur_manager::OrgKdeKwinBlurManager,
};

const PROBE_INTERVAL: Duration = Duration::from_millis(5);
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(1);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

fn rix_wayland_blur_is_available() -> bool {
    std::env::var("XDG_SESSION_TYPE").is_ok_and(|value| value.eq_ignore_ascii_case("wayland"))
        && std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|desktop| {
            desktop
                .split(':')
                .any(|entry| entry.eq_ignore_ascii_case("RIX"))
        })
}

pub struct NativeStyle {
    timer: Timer,
    maintenance: Timer,
    window: slint::Weak<FilesWindow>,
    bridge: Option<Bridge>,
    started: Instant,
    enabled: bool,
    failed: bool,
    supported: bool,
}

pub fn install(window: &FilesWindow, theme: &ThemeSettings) -> Rc<RefCell<NativeStyle>> {

    let supported = theme.blur_radius > 0 && rix_wayland_blur_is_available();
    window.set_desktop_material_supported(supported);
    let style = Rc::new(RefCell::new(NativeStyle {
        timer: Timer::default(),
        maintenance: Timer::default(),
        window: window.as_weak(),
        bridge: None,
        started: Instant::now(),
        enabled: theme.blur_radius > 0,
        failed: false,
        supported,
    }));
    let weak = Rc::downgrade(&style);
    style
        .borrow()
        .timer
        .start(TimerMode::Repeated, PROBE_INTERVAL, move || {
            if let Some(style) = weak.upgrade() {
                style.borrow_mut().tick();
            }
        });
    let weak = Rc::downgrade(&style);
    style
        .borrow()
        .maintenance
        .start(TimerMode::Repeated, MAINTENANCE_INTERVAL, move || {
            if let Some(style) = weak.upgrade() {
                let mut style = style.borrow_mut();
                if style.supported {
                    style.tick();
                }
            }
        });
    style
}

impl NativeStyle {

    pub fn prepare(&mut self) {
        if self.bridge.is_none() && !self.failed {
            self.started = Instant::now();
        }
        self.tick();
    }

    pub fn update(&mut self, theme: &ThemeSettings) {
        self.enabled = theme.blur_radius > 0;
        if let Some(bridge) = &mut self.bridge {
            bridge.apply(self.enabled);
            if let Err(error) = bridge.flush() {
                self.fail(&error);
            }
        }
    }

    fn set_supported(&mut self, supported: bool) {
        if self.supported != supported {
            self.supported = supported;
            tracing::debug!(supported, "Native desktop material capability changed");
            if let Some(window) = self.window.upgrade() {
                window.set_desktop_material_supported(supported);
            }
        }
    }

    fn fail(&mut self, error: &str) {
        self.failed = true;
        self.set_supported(false);
        self.bridge = None;
        self.timer.stop();
        self.maintenance.stop();
        tracing::debug!(%error, "Native desktop material unavailable; using opaque theme surfaces");
    }

    fn tick(&mut self) {
        if self.failed {
            return;
        }
        let Some(window) = self.window.upgrade() else {
            self.bridge = None;
            self.timer.stop();
            self.maintenance.stop();
            return;
        };
        if self.bridge.is_none() {
            if !window.window().is_visible() {

                self.started = Instant::now();
                return;
            }
            match Bridge::from_window(&window) {
                Ok(Some(bridge)) => {
                    self.started = Instant::now();
                    self.bridge = Some(bridge);
                }
                Ok(None) => {
                    if self.started.elapsed() >= PROBE_TIMEOUT {
                        self.fail("Wayland window handles did not become available");
                    }
                    return;
                }
                Err(error) => {
                    self.fail(&error);
                    return;
                }
            }
        }
        let bridge = self.bridge.as_mut().expect("native bridge was initialized");
        match bridge.poll(self.enabled) {
            Ok(true) => {
                self.set_supported(true);

                self.timer.stop();
            }
            Ok(false) if self.started.elapsed() >= PROBE_TIMEOUT => {
                self.fail("Compositor did not acknowledge the desktop material probe in time");
            }
            Ok(false) => {}
            Err(error) => self.fail(&error),
        }
    }
}

#[derive(Clone, Copy)]
enum SyncPoint {
    Registry,
    Bound,
}

#[derive(Default)]
struct ProtocolState {
    blur_global: Option<u32>,
    registry_ready: bool,
    bound_ready: bool,
    removed: bool,
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
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == OrgKdeKwinBlurManager::interface().name && version >= 1 => {
                state.blur_global = Some(name);
            }
            wl_registry::Event::GlobalRemove { name } if state.blur_global == Some(name) => {
                state.blur_global = None;
                state.removed = true;
            }
            _ => {}
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
                SyncPoint::Bound => state.bound_ready = true,
            }
        }
    }
}
wayland_client::delegate_noop!(ProtocolState: ignore OrgKdeKwinBlurManager);
wayland_client::delegate_noop!(ProtocolState: ignore OrgKdeKwinBlur);

struct Bridge {

    blur: Option<OrgKdeKwinBlur>,
    manager: Option<OrgKdeKwinBlurManager>,
    registry: Option<wl_registry::WlRegistry>,
    surface: Option<wl_surface::WlSurface>,
    queue: Option<EventQueue<ProtocolState>>,
    connection: Option<Connection>,
    state: ProtocolState,
    applied: Option<bool>,
    provider: Option<slint::WindowHandle>,
}

impl Bridge {
    fn from_window(window: &FilesWindow) -> Result<Option<Self>, String> {
        let provider = window.window().window_handle();
        let Ok(display) = provider.display_handle() else {
            return Ok(None);
        };
        let Ok(surface) = provider.window_handle() else {
            return Ok(None);
        };
        let (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(surface)) =
            (display.as_raw(), surface.as_raw())
        else {
            return Err("Window is not a native Wayland surface".into());
        };

        let (backend, surface_id) = unsafe {
            let backend = Backend::from_foreign_display(display.display.as_ptr().cast());
            let surface_id = ObjectId::from_ptr(
                wl_surface::WlSurface::interface(),
                surface.surface.as_ptr().cast(),
            )
            .map_err(|error| error.to_string())?;
            (backend, surface_id)
        };
        let connection = Connection::from_backend(backend);
        let surface = wl_surface::WlSurface::from_id(&connection, surface_id)
            .map_err(|error| error.to_string())?;
        let queue = connection.new_event_queue::<ProtocolState>();
        let registry = connection.display().get_registry(&queue.handle(), ());
        connection
            .display()
            .sync(&queue.handle(), SyncPoint::Registry);
        let bridge = Self {
            blur: None,
            manager: None,
            registry: Some(registry),
            surface: Some(surface),
            queue: Some(queue),
            connection: Some(connection),
            state: ProtocolState::default(),
            applied: None,
            provider: Some(provider),
        };
        bridge.flush()?;
        Ok(Some(bridge))
    }

    fn poll(&mut self, enabled: bool) -> Result<bool, String> {

        self.queue
            .as_mut()
            .expect("bridge queue")
            .dispatch_pending(&mut self.state)
            .map_err(|error| error.to_string())?;
        if self.state.removed {
            return Err("Compositor removed the desktop blur protocol".into());
        }
        if self.state.registry_ready && self.manager.is_none() {
            let name = self
                .state
                .blur_global
                .ok_or("Compositor does not support desktop blur")?;
            let qh = self.queue.as_ref().expect("bridge queue").handle();
            let manager = self
                .registry
                .as_ref()
                .expect("bridge registry")
                .bind::<OrgKdeKwinBlurManager, _, _>(name, 1, &qh, ());
            self.blur =
                Some(manager.create(self.surface.as_ref().expect("bridge surface"), &qh, ()));
            self.manager = Some(manager);
            self.apply(enabled);
            self.connection
                .as_ref()
                .expect("bridge connection")
                .display()
                .sync(&qh, SyncPoint::Bound);
        }
        self.flush()?;
        Ok(self.state.bound_ready)
    }

    fn apply(&mut self, enabled: bool) {
        let (Some(blur), Some(manager), Some(surface)) = (&self.blur, &self.manager, &self.surface)
        else {
            return;
        };
        if self.applied != Some(enabled) {
            if enabled {
                blur.set_region(None);
                blur.commit();
            } else {
                manager.unset(surface);
            }
            self.applied = Some(enabled);
        }
    }

    fn flush(&self) -> Result<(), String> {
        match self.connection.as_ref().expect("bridge connection").flush() {
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
        if let Some(blur) = self.blur.take() {
            blur.release();
        }
        if let (Some(manager), Some(surface)) = (&self.manager, &self.surface) {
            manager.unset(surface);
        }
        if let Some(connection) = &self.connection {
            let _ = connection.flush();
        }
        self.manager = None;
        self.registry = None;
        self.surface = None;
        self.queue = None;
        self.connection = None;

        self.provider = None;
    }
}
