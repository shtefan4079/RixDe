use wayland_protocols::wp::{
    fractional_scale::v1::client::{
        wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        wp_fractional_scale_v1::{self, WpFractionalScaleV1},
    },
    viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
};

use crate::{LayerSurfaceSpec, ShellLayer};
use i_slint_renderer_femtovg::{
    FemtoVGOpenGLRenderer, FemtoVGOpenGLRendererExt, FemtoVGRendererExt,
};
use rix_protocols::client::{
    rix_surface_effects_manager_v1::RixSurfaceEffectsManagerV1,
    rix_surface_effects_v1::RixSurfaceEffectsV1,
};
use slint::{
    platform::{
        self,
        software_renderer::{PremultipliedRgbaColor, RepaintBufferType, SoftwareRenderer},
        Clipboard as PlatformClipboard, EventLoopProxy, Key, Platform, PointerEventButton,
        WindowAdapter, WindowEvent, WindowProperties,
    },
    LogicalPosition, LogicalSize, PhysicalSize, PlatformError, Window, WindowSize,
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_compositor, delegate_data_device, delegate_keyboard, delegate_layer, delegate_output,
    delegate_pointer, delegate_registry, delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers, RepeatInfo},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{
        slot::{Buffer, SlotPool},
        Shm, ShmHandler,
    },
};
use std::{
    cell::{Cell, RefCell},
    io::{Read, Write},
    os::{
        fd::{AsFd, AsRawFd},
        unix::net::UnixStream,
    },
    rc::{Rc, Weak},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
};
use wayland_client::Dispatch;
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_region, wl_seat, wl_shm, wl_surface},
    Connection, EventQueue, Proxy as WaylandProxy, QueueHandle,
};
use wayland_protocols_plasma::blur::client::{
    org_kde_kwin_blur::OrgKdeKwinBlur, org_kde_kwin_blur_manager::OrgKdeKwinBlurManager,
};

use i_slint_core::{items::MouseCursor, window::WindowAdapterInternal};
use smithay_client_toolkit::{delegate_xdg_popup, delegate_xdg_shell, shell::xdg::XdgShell};
#[path = "wayland_clipboard.rs"]
mod clipboard;
#[path = "wayland_popup.rs"]
mod popup;
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1},
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};

fn error(e: impl std::fmt::Display) -> PlatformError {
    PlatformError::Other(e.to_string())
}

#[derive(Clone)]
pub struct SurfaceController(Rc<RefCell<Control>>);
struct Control {
    spec: LayerSurfaceSpec,
    overlay_active: bool,
    output_fullscreen: bool,
    adapter: Weak<LayerWindow>,
    focus_changed: Option<Rc<dyn Fn(bool)>>,
    output_size_changed: Option<Rc<dyn Fn(u32, u32)>>,
    layout_changed: Option<Rc<dyn Fn(String, bool)>>,
    pending_layout: Option<(String, bool)>,
    regions: Option<Rc<dyn Fn() -> Vec<crate::SurfaceRect>>>,
    popup_radius: Option<Rc<dyn Fn() -> f32>>,
}
impl SurfaceController {
    pub fn supports_native_popups(&self) -> bool {
        self.0
            .borrow()
            .adapter
            .upgrade()
            .is_some_and(|w| w.popup_factory.is_some())
    }
    pub fn set_popup_radius_provider(&self, provider: impl Fn() -> f32 + 'static) {
        self.0.borrow_mut().popup_radius = Some(Rc::new(provider));
    }
    pub fn on_layout_changed(&self, callback: impl Fn(String, bool) + 'static) {
        self.0.borrow_mut().layout_changed = Some(Rc::new(callback));
    }

    pub fn set_region_provider(&self, provider: impl Fn() -> Vec<crate::SurfaceRect> + 'static) {
        self.0.borrow_mut().regions = Some(Rc::new(provider));
        if let Some(adapter) = self.0.borrow().adapter.upgrade() {
            adapter.request_redraw();
        }
    }

    pub fn paint_phases(&self) -> [std::time::Duration; 3] {
        self.0
            .borrow()
            .adapter
            .upgrade()
            .map(|w| w.paint_phases.get())
            .unwrap_or_default()
    }

    pub fn frame_stats(&self) -> (u64, std::time::Duration) {
        self.0
            .borrow()
            .adapter
            .upgrade()
            .map(|w| (w.rendered_frames.get(), w.paint_time.get()))
            .unwrap_or_default()
    }
    pub fn on_output_size_changed(&self, callback: impl Fn(u32, u32) + 'static) {
        self.0.borrow_mut().output_size_changed = Some(Rc::new(callback));
    }
    pub fn on_focus_changed(&self, callback: impl Fn(bool) + 'static) {
        self.0.borrow_mut().focus_changed = Some(Rc::new(callback));
    }
    pub fn set_spec(&self, spec: LayerSurfaceSpec) {
        let adapter = {
            let mut c = self.0.borrow_mut();
            c.spec = spec;
            c.adapter.upgrade()
        };
        if let Some(adapter) = adapter {
            adapter.apply_spec();
        }
    }
    pub fn set_keyboard_interactive(&self, interactive: bool) {
        let mut spec = self.0.borrow().spec.clone();
        if spec.keyboard_interactive != interactive {
            spec.keyboard_interactive = interactive;
            self.set_spec(spec);
        }
    }

    pub fn set_exclusive_zone(&self, exclusive_zone: i32) {
        let mut spec = self.0.borrow().spec.clone();
        if spec.exclusive_zone != exclusive_zone {
            spec.exclusive_zone = exclusive_zone;
            self.set_spec(spec);
        }
    }

    pub fn set_output_fullscreen(&self, fullscreen: bool) {
        let adapter = {
            let mut control = self.0.borrow_mut();
            if control.output_fullscreen == fullscreen {
                return;
            }
            control.output_fullscreen = fullscreen;
            control.adapter.upgrade()
        };
        if let Some(adapter) = adapter {
            adapter.apply_spec();
        }
    }
    pub fn set_overlay_active(&self, active: bool) {
        let adapter = {
            let mut control = self.0.borrow_mut();
            if control.overlay_active == active {
                return;
            }
            control.overlay_active = active;
            control.adapter.upgrade()
        };
        if let Some(adapter) = adapter {
            adapter.apply_spec();
        }
    }
}

pub fn install(spec: LayerSurfaceSpec) -> Result<SurfaceController, PlatformError> {
    let conn = Connection::connect_to_env().map_err(error)?;
    let (globals, mut queue) = registry_queue_init(&conn).map_err(error)?;
    let qh = queue.handle();
    let control = SurfaceController(Rc::new(RefCell::new(Control {
        spec,
        overlay_active: false,
        output_fullscreen: false,
        adapter: Weak::new(),
        focus_changed: None,
        output_size_changed: None,
        layout_changed: None,
        pending_layout: None,
        regions: None,
        popup_radius: None,
    })));
    let (wake_read, wake_write) = UnixStream::pair().map_err(error)?;
    wake_read.set_nonblocking(true).map_err(error)?;
    wake_write.set_nonblocking(true).map_err(error)?;
    let (sender, receiver) = mpsc::sync_channel(MESSAGE_QUEUE_CAPACITY);
    let proxy = Proxy {
        sender,
        wake: Arc::new(Mutex::new(wake_write)),
        mailbox: Arc::new(Mailbox::default()),
        loop_thread: std::thread::current().id(),
    };
    let clipboard = clipboard::Clipboard::new(
        smithay_client_toolkit::data_device_manager::DataDeviceManagerState::bind(&globals, &qh)
            .ok(),
        proxy.clone(),
    );
    let mut state = WaylandState {
        registry: RegistryState::new(&globals),
        seats: SeatState::new(&globals, &qh),
        outputs: OutputState::new(&globals, &qh),
        shm: Shm::bind(&globals, &qh).map_err(error)?,
        compositor: Rc::new(CompositorState::bind(&globals, &qh).map_err(error)?),
        blur: globals.bind(&qh, 1..=1, ()).ok(),
        effects: globals.bind(&qh, 2..=2, ()).ok(),
        viewporter: globals.bind(&qh, 1..=1, ()).ok(),
        fractional_scale: globals.bind(&qh, 1..=1, ()).ok(),
        cursor_shape: globals.bind(&qh, 1..=1, ()).ok(),
        cursor_device: None,
        layers: LayerShell::bind(&globals, &qh).map_err(error)?,
        xdg: XdgShell::bind(&globals, &qh).ok().map(Rc::new),
        window: Weak::new(),
        keyboard: None,
        pointer: None,
        focused: false,
        keyboard_surface: None,
        pressed: Default::default(),
        keymap: None,
        modifiers: Modifiers::default(),
        clipboard: clipboard.clone(),
        deferred_keys: RefCell::new(None),
        repeat_info: RepeatInfo::Disable,
        repeating: None,
        repeat_delay: slint::Timer::default(),
        repeat_timer: Rc::new(slint::Timer::default()),
        exit: false,
    };

    let first_seat = state.seats.seats().next();
    if let Some(seat) = first_seat {
        state.new_seat(&conn, &qh, seat);
    }

    queue.roundtrip(&mut state).map_err(error)?;
    platform::set_platform(Box::new(LayerPlatform {
        conn,
        queue: RefCell::new(queue),
        state: RefCell::new(state),
        control: control.clone(),
        wake_read: RefCell::new(wake_read),
        proxy,
        clipboard,
        receiver,
    }))
    .map_err(error)?;
    Ok(control)
}

const MESSAGE_QUEUE_CAPACITY: usize = 64;

const QUEUE_BACKPRESSURE_WAIT: std::time::Duration = std::time::Duration::from_millis(5);

enum Message {
    Invoke(Box<dyn FnOnce() + Send>),
    Quit,
    ClipboardReady(u64, Option<String>),
}

#[derive(Default)]
struct Mailbox {

    closed: AtomicBool,

    quit: AtomicBool,

    clipboard: Mutex<Option<(u64, Option<String>)>>,
}

pub(crate) fn lock_or_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!("Recovering a poisoned event-loop lock");
            poisoned.into_inner()
        }
    }
}

#[derive(Clone)]
struct Proxy {
    sender: mpsc::SyncSender<Message>,
    wake: Arc<Mutex<UnixStream>>,
    mailbox: Arc<Mailbox>,

    loop_thread: std::thread::ThreadId,
}
impl Proxy {
    fn send(&self, message: Message) -> Result<(), slint::EventLoopError> {
        self.queue(message)?;
        self.wake();
        Ok(())
    }
    fn queue(&self, message: Message) -> Result<(), slint::EventLoopError> {
        match self.sender.try_send(message) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Disconnected(_)) => {
                Err(slint::EventLoopError::EventLoopTerminated)
            }
            Err(mpsc::TrySendError::Full(message)) => self.overflow(message),
        }
    }

    fn overflow(&self, message: Message) -> Result<(), slint::EventLoopError> {
        if self.mailbox.closed.load(Ordering::Acquire) {
            return Err(slint::EventLoopError::EventLoopTerminated);
        }
        match message {
            Message::Quit => self.mailbox.quit.store(true, Ordering::Release),
            Message::ClipboardReady(generation, text) => {
                *lock_or_recover(&self.mailbox.clipboard) = Some((generation, text));
            }
            Message::Invoke(event) => {
                if std::thread::current().id() == self.loop_thread {
                    event();
                } else {
                    self.wait_for_space(event)?;
                }
            }
        }
        Ok(())
    }
    fn wait_for_space(&self, event: Box<dyn FnOnce() + Send>) -> Result<(), slint::EventLoopError> {
        let mut message = Message::Invoke(event);
        loop {
            match self.sender.try_send(message) {
                Ok(()) => return Ok(()),
                Err(mpsc::TrySendError::Full(returned)) => {
                    if self.mailbox.closed.load(Ordering::Acquire) {
                        return Err(slint::EventLoopError::EventLoopTerminated);
                    }
                    message = returned;

                    std::thread::sleep(QUEUE_BACKPRESSURE_WAIT);
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    return Err(slint::EventLoopError::EventLoopTerminated);
                }
            }
        }
    }

    fn wake(&self) {
        let _ = lock_or_recover(&self.wake).write(&[1]);
    }

    fn close(&self) {
        self.mailbox.closed.store(true, Ordering::Release);
    }
}
impl EventLoopProxy for Proxy {
    fn quit_event_loop(&self) -> Result<(), slint::EventLoopError> {
        self.send(Message::Quit)
    }
    fn invoke_from_event_loop(
        &self,
        event: Box<dyn FnOnce() + Send>,
    ) -> Result<(), slint::EventLoopError> {
        self.send(Message::Invoke(event))
    }
}
struct LayerPlatform {
    conn: Connection,
    queue: RefCell<EventQueue<WaylandState>>,
    state: RefCell<WaylandState>,
    control: SurfaceController,
    wake_read: RefCell<UnixStream>,
    proxy: Proxy,
    receiver: mpsc::Receiver<Message>,
    clipboard: Rc<RefCell<clipboard::Clipboard>>,
}
impl Platform for LayerPlatform {
    fn set_clipboard_text(&self, text: &str, clipboard: PlatformClipboard) {
        if matches!(clipboard, PlatformClipboard::DefaultClipboard) {
            self.clipboard.borrow_mut().set_text(text);
        }
    }
    fn clipboard_text(&self, clipboard: PlatformClipboard) -> Option<String> {
        if matches!(clipboard, PlatformClipboard::DefaultClipboard) {
            let mut clipboard = self.clipboard.borrow_mut();
            clipboard.reconcile();
            clipboard.text()
        } else {
            None
        }
    }
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let state = self.state.borrow();
        if state.window.upgrade().is_some() {
            return Err(error(
                "This platform owns one desktop surface; use Slint popup components for its UI",
            ));
        }
        let qh = self.queue.borrow().handle();
        let spec = self.control.0.borrow().spec.clone();
        let surface = state.compositor.create_surface(&qh);
        let blur = state
            .blur
            .as_ref()
            .map(|manager| manager.create(&surface, &qh, ()));
        let output = spec
            .output
            .as_ref()
            .map(|name| {
                state
                    .outputs
                    .outputs()
                    .find(|output| {
                        state
                            .outputs
                            .info(output)
                            .is_some_and(|info| info.name.as_ref() == Some(name))
                    })
                    .ok_or_else(|| error(format!("Panel output is not available: {name}")))
            })
            .transpose()?;
        let layer = state.layers.create_layer_surface(
            &qh,
            surface,
            Layer::Top,
            Some(spec.namespace),
            output.as_ref(),
        );
        let pool = SlotPool::new(4096, &state.shm).map_err(error)?;
        let effects = state
            .effects
            .as_ref()
            .map(|manager| manager.get_surface(layer.wl_surface(), &qh, ()));
        let gpu = if std::env::var("RIX_SHELL_RENDERER").as_deref() == Ok("software") {
            tracing::warn!("Layer shell software renderer explicitly requested");
            None
        } else {
            let context = crate::egl::EglWindow::new(&self.conn, layer.wl_surface())?;
            let renderer = FemtoVGOpenGLRenderer::new_suspended();
            renderer.set_opengl_context(context)?;
            Some(renderer)
        };
        let window = Rc::new_cyclic(|weak: &Weak<LayerWindow>| LayerWindow {
            window: Window::new(weak.clone()),
            cursor: Cell::new(MouseCursor::Default),
            cursor_enter: Cell::new(None),
            cursor_device: RefCell::new(state.cursor_device.clone()),
            pointer: RefCell::new(state.pointer.clone()),
            renderer: SoftwareRenderer::new_with_repaint_buffer_type(
                RepaintBufferType::ReusedBuffer,
            ),
            gpu,
            popups: RefCell::new(Vec::new()),
            popup_factory: state.xdg.as_ref().map(|xdg| {
                Rc::new(popup::Factory {
                    xdg: xdg.clone(),
                    compositor: state.compositor.clone(),
                    shm: Shm::from(state.shm.wl_shm().clone()),
                    blur: state.blur.clone(),
                    effects: state.effects.clone(),
                    viewporter: state.viewporter.clone(),
                    fractional_scale: state.fractional_scale.clone(),
                    last_press: RefCell::new(None),
                })
            }),
            region_cache: RefCell::new(RegionCache::default()),
            blur,
            viewport: state
                .viewporter
                .as_ref()
                .map(|v| v.get_viewport(layer.wl_surface(), &qh, ())),
            fractional_scale: state
                .viewporter
                .as_ref()
                .and_then(|_| state.fractional_scale.as_ref())
                .map(|m| m.get_fractional_scale(layer.wl_surface(), &qh, ())),
            effects,
            layer,
            qh,
            control: self.control.clone(),
            pool: RefCell::new(pool),
            buffers: RefCell::new(Vec::new()),
            hit_mask: RefCell::new(Vec::new()),
            hit_region: RefCell::new(HitRegion::default()),
            pixels: RefCell::new(Vec::new()),
            full_redraw: Cell::new(true),
            awaiting_resize: Cell::new(true),
            size: Cell::new(PhysicalSize::new(1, 1)),
            requested: Cell::new(LogicalSize::new(1., 1.)),
            scale: Cell::new(1.),
            dirty: Cell::new(true),
            configured: Cell::new(false),
            visible: Cell::new(false),
            frame_pending: Cell::new(false),
            rendered_frames: Cell::new(0),
            paint_time: Cell::new(std::time::Duration::ZERO),
            paint_phases: Cell::new([std::time::Duration::ZERO; 3]),
            input_region: RefCell::new(None),
            compositor: state.compositor.clone(),
        });
        self.control.0.borrow_mut().adapter = Rc::downgrade(&window);
        drop(state);
        self.state.borrow_mut().window = Rc::downgrade(&window);
        window.apply_spec();
        Ok(window)
    }
    fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
        Some(Box::new(self.proxy.clone()))
    }
    fn run_event_loop(&self) -> Result<(), PlatformError> {

        let result = (|| -> Result<(), PlatformError> {
            loop {

                let mut clipboard_results = Vec::new();
                while let Ok(message) = self.receiver.try_recv() {
                    match message {
                        Message::Invoke(f) => f(),
                        Message::Quit => return Ok(()),
                        Message::ClipboardReady(generation, text) => {
                            clipboard_results.push((generation, text));
                        }
                    }
                }
                if self.proxy.mailbox.quit.swap(false, Ordering::AcqRel) {
                    return Ok(());
                }
                clipboard_results.extend(lock_or_recover(&self.proxy.mailbox.clipboard).take());
                self.queue
                    .borrow_mut()
                    .dispatch_pending(&mut self.state.borrow_mut())
                    .map_err(error)?;
                self.clipboard.borrow_mut().reconcile();
                for (generation, text) in clipboard_results {
                    if self.clipboard.borrow_mut().ready(generation, text) {
                        self.state.borrow().complete_deferred_keys();
                    }
                }
                platform::update_timers_and_animations();
                self.queue
                    .borrow_mut()
                    .dispatch_pending(&mut self.state.borrow_mut())
                    .map_err(error)?;
                self.clipboard.borrow_mut().reconcile();
                self.clipboard
                    .borrow_mut()
                    .publish(&self.queue.borrow().handle());
                if !self.clipboard.borrow().pending {
                    self.state.borrow().complete_deferred_keys();
                }
                if self.state.borrow().exit {
                    return Ok(());
                }
                let layout = {
                    let mut control = self.control.0.borrow_mut();
                    control
                        .layout_changed
                        .clone()
                        .zip(control.pending_layout.take())
                };
                if let Some((callback, (layout, caps_lock))) = layout {
                    callback(layout, caps_lock);
                }
                let window = self.state.borrow().window.upgrade();
                if let Some(window) = &window {
                    window.draw()?;
                    let popups = window.live_popups();
                    for popup in popups {
                        popup.draw()?;
                    }
                }
                self.conn.flush().map_err(error)?;
                let Some(guard) = self.queue.borrow().prepare_read() else {
                    continue;
                };
                let timeout = if window.as_ref().is_some_and(|w| {
                    (w.window.has_active_animations() && !w.frame_pending.get())
                        || w.live_popups()
                            .iter()
                            .any(|p| p.window.has_active_animations() && !p.frame_pending.get())
                }) {
                    0
                } else {
                    platform::duration_until_next_timer_update()
                        .map(|d| d.as_millis().min(i32::MAX as u128) as i32)
                        .unwrap_or(-1)
                };
                let mut fds = [
                    libc::pollfd {
                        fd: self.conn.as_fd().as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: self.wake_read.borrow().as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];

                let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) };
                if result < 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                {
                    return Err(error(std::io::Error::last_os_error()));
                }
                if fds[0].revents & (libc::POLLERR | libc::POLLHUP) != 0
                    && fds[0].revents & libc::POLLIN == 0
                {
                    return Err(error("Wayland connection closed"));
                }
                if fds[0].revents & libc::POLLIN != 0 {
                    guard.read().map_err(error)?;
                } else {
                    drop(guard);
                }
                if fds[1].revents & libc::POLLIN != 0 {
                    let mut bytes = [0; 512];
                    while self.wake_read.borrow_mut().read(&mut bytes).is_ok() {}
                }
            }
        })();
        self.proxy.close();
        result
    }
}

type DamageRect = (usize, usize, usize, usize);
struct ShmBuffer {
    buffer: Buffer,
    pending: Vec<DamageRect>,
}

#[derive(Default)]
struct HitRegion {
    spans: Vec<(i32, i32, i32, i32)>,
    region: Option<Region>,
}

struct LayerWindow {
    cursor: Cell<MouseCursor>,
    cursor_enter: Cell<Option<u32>>,
    cursor_device: RefCell<Option<WpCursorShapeDeviceV1>>,
    pointer: RefCell<Option<wl_pointer::WlPointer>>,
    window: Window,
    renderer: SoftwareRenderer,

    gpu: Option<FemtoVGOpenGLRenderer>,
    popups: RefCell<Vec<Weak<popup::PopupWindow>>>,
    popup_factory: Option<Rc<popup::Factory>>,
    region_cache: RefCell<RegionCache>,
    layer: LayerSurface,
    blur: Option<OrgKdeKwinBlur>,
    effects: Option<RixSurfaceEffectsV1>,
    viewport: Option<WpViewport>,
    fractional_scale: Option<WpFractionalScaleV1>,
    qh: QueueHandle<WaylandState>,
    control: SurfaceController,
    pool: RefCell<SlotPool>,
    buffers: RefCell<Vec<ShmBuffer>>,
    hit_mask: RefCell<Vec<bool>>,
    hit_region: RefCell<HitRegion>,
    pixels: RefCell<Vec<PremultipliedRgbaColor>>,
    full_redraw: Cell<bool>,
    awaiting_resize: Cell<bool>,
    size: Cell<PhysicalSize>,
    requested: Cell<LogicalSize>,
    scale: Cell<f64>,
    dirty: Cell<bool>,
    configured: Cell<bool>,
    visible: Cell<bool>,
    frame_pending: Cell<bool>,
    rendered_frames: Cell<u64>,
    paint_time: Cell<std::time::Duration>,
    paint_phases: Cell<[std::time::Duration; 3]>,

    input_region: RefCell<Option<wl_region::WlRegion>>,
    compositor: Rc<CompositorState>,
}
impl LayerWindow {
    fn live_popups(&self) -> Vec<Rc<popup::PopupWindow>> {
        self.popups.borrow_mut().retain(|p| p.strong_count() > 0);
        self.popups
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .collect()
    }
    fn update_cursor(&self) {
        let Some(serial) = self.cursor_enter.get() else {
            return;
        };
        if self.cursor.get() == MouseCursor::None {
            if let Some(pointer) = self.pointer.borrow().as_ref() {
                pointer.set_cursor(serial, None, 0, 0);
            }
        } else if let Some(device) = self.cursor_device.borrow().as_ref() {
            device.set_shape(serial, cursor_shape(self.cursor.get()));
        }
    }
    fn notify_focus(&self, focused: bool) {
        let callback = self.control.0.borrow().focus_changed.clone();
        if let Some(callback) = callback {
            callback(focused);
        }
    }

    fn apply_spec(&self) {
        let spec = &self.control.0.borrow().spec;
        let mut anchor = Anchor::empty();
        for (enabled, flag) in [
            (spec.anchors.top, Anchor::TOP),
            (spec.anchors.bottom, Anchor::BOTTOM),
            (spec.anchors.left, Anchor::LEFT),
            (spec.anchors.right, Anchor::RIGHT),
        ] {
            if enabled {
                anchor |= flag;
            }
        }
        self.layer.set_anchor(anchor);
        let size = self.requested.get();
        self.layer.set_size(
            if spec.anchors.left && spec.anchors.right {
                0
            } else {
                size.width.max(1.) as u32
            },
            if spec.anchors.top && spec.anchors.bottom {
                0
            } else {
                size.height.max(1.) as u32
            },
        );
        let control = self.control.0.borrow();
        let layer = effective_layer(
            spec.layer,
            control.output_fullscreen,
            control.overlay_active,
        );
        drop(control);
        self.layer.set_layer(match layer {
            ShellLayer::Background => Layer::Background,
            ShellLayer::Bottom => Layer::Bottom,
            ShellLayer::Top => Layer::Top,
            ShellLayer::Overlay => Layer::Overlay,
        });
        self.layer.set_exclusive_zone(spec.exclusive_zone);
        self.layer.set_margin(
            spec.margins.top,
            spec.margins.right,
            spec.margins.bottom,
            spec.margins.left,
        );
        self.layer
            .set_keyboard_interactivity(if spec.keyboard_interactive {
                KeyboardInteractivity::Exclusive
            } else {
                KeyboardInteractivity::None
            });
        self.layer.commit();
    }
    fn request_size(&self, size: LogicalSize) {
        let size = LogicalSize::new(size.width.max(1.), size.height.max(1.));
        let previous = self.requested.replace(size);
        let anchors = self.control.0.borrow().spec.anchors;
        let requested = layer_requested_size(size, anchors);
        if requested == layer_requested_size(previous, anchors) {

            return;
        }
        let current = self.size.get();
        let scale = self.scale.get();
        self.awaiting_resize.set(
            (requested.0 != 0 && requested.0 != (current.width as f64 / scale).round() as u32)
                || (requested.1 != 0
                    && requested.1 != (current.height as f64 / scale).round() as u32),
        );
        self.layer.set_size(requested.0, requested.1);
        self.layer.commit();
    }
    fn resize(&self, width: u32, height: u32) {
        let scale = self.scale.get();
        let size = PhysicalSize::new(
            (width as f64 * scale).ceil() as u32,
            (height as f64 * scale).ceil() as u32,
        );
        if let Some(viewport) = &self.viewport {
            self.layer.wl_surface().set_buffer_scale(1);
            viewport.set_destination(width.max(1) as i32, height.max(1) as i32);
        }
        if self.size.replace(size) != size {
            self.full_redraw.set(true);
            self.buffers.borrow_mut().clear();
            self.hit_mask.borrow_mut().clear();

            *self.hit_region.borrow_mut() = HitRegion::default();
            if self.gpu.is_none() {
                self.pixels.borrow_mut().resize(
                    (size.width * size.height) as usize,
                    PremultipliedRgbaColor::default(),
                );
            }
            self.window.dispatch_event(WindowEvent::Resized {
                size: LogicalSize::new(width as f32, height as f32),
            });
            self.dirty.set(true);
        }
    }
    fn draw(&self) -> Result<(), PlatformError> {
        if !self.configured.get()
            || self.awaiting_resize.get()
            || !self.visible.get()
            || self.frame_pending.get()
            || !(self.dirty.replace(false) || self.window.has_active_animations())
        {
            return Ok(());
        }
        let paint_start = std::time::Instant::now();
        let provider = self.control.0.borrow().regions.clone();
        let regions = provider.map(|f| f());
        if let Some(renderer) = &self.gpu {
            let (region, region_changed) = self
                .region_cache
                .borrow_mut()
                .region(&self.compositor, regions.as_deref())?;
            if region_changed || self.full_redraw.replace(false) {
                if let Some(blur) = &self.blur {

                    let empty = if region.is_none() {
                        Some(Region::new(&*self.compositor).map_err(error)?)
                    } else {
                        None
                    };
                    blur.set_region(region.as_ref().or(empty.as_ref().map(Region::wl_region)));
                    blur.commit();
                }
                if let Some(effects) = &self.effects {
                    effects.clear();
                    for rect in regions.iter().flatten() {
                        if rect.width > 0. && rect.height > 0. && rect.opacity > 0. {
                            if let Some(clip) = rect.clip {
                                effects.add_clipped_rectangle(
                                    rect.x as f64,
                                    rect.y as f64,
                                    rect.width as f64,
                                    rect.height as f64,
                                    rect.radius as f64,
                                    rect.opacity as f64,
                                    clip.x as f64,
                                    clip.y as f64,
                                    clip.width as f64,
                                    clip.height as f64,
                                );
                            } else {
                                effects.add_rectangle(
                                    rect.x as f64,
                                    rect.y as f64,
                                    rect.width as f64,
                                    rect.height as f64,
                                    rect.radius as f64,
                                    rect.opacity as f64,
                                );
                            }
                        }
                    }
                    effects.commit();
                }
                *self.input_region.borrow_mut() = region;
            }

            if self.live_popups().iter().any(|p| p.visible.get()) {
                self.layer.wl_surface().set_input_region(None);
            } else {
                let input_region = self.input_region.borrow();
                self.layer
                    .wl_surface()
                    .set_input_region(input_region.as_ref());
            }
            let regions_done = std::time::Instant::now();

            self.layer
                .wl_surface()
                .frame(&self.qh, self.layer.wl_surface().clone());
            self.frame_pending.set(true);
            renderer.render()?;
            let mut phases = self.paint_phases.get();
            phases[0] += regions_done.elapsed();
            phases[2] += regions_done - paint_start;
            self.paint_phases.set(phases);
            self.rendered_frames.set(self.rendered_frames.get() + 1);
            self.paint_time
                .set(self.paint_time.get() + paint_start.elapsed());
            return Ok(());
        }
        let size = self.size.get();
        let mut pixels = self.pixels.borrow_mut();

        self.renderer
            .set_repaint_buffer_type(if self.full_redraw.replace(false) {
                RepaintBufferType::NewBuffer
            } else {
                RepaintBufferType::ReusedBuffer
            });
        let damage = self.renderer.render(&mut pixels[..], size.width as usize);
        let rendered = std::time::Instant::now();
        let damage: Vec<DamageRect> = damage
            .iter()
            .map(|(pos, size)| {
                (
                    pos.x as usize,
                    pos.y as usize,
                    size.width as usize,
                    size.height as usize,
                )
            })
            .collect();
        let regions_changed = {
            let (region, region_changed) = self
                .region_cache
                .borrow_mut()
                .region(&self.compositor, regions.as_deref())?;
            if region_changed {
                *self.input_region.borrow_mut() = region;
            }
            region_changed
        };
        if self.live_popups().iter().any(|p| p.visible.get()) {
            self.layer.wl_surface().set_input_region(None);
        } else {
            let input_region = self.input_region.borrow();
            self.layer
                .wl_surface()
                .set_input_region(input_region.as_ref());
        }
        if damage.is_empty() {
            if regions_changed {

                self.layer.commit();
            }
            return Ok(());
        }
        let full = (0, 0, size.width as usize, size.height as usize);
        let mut pool = self.pool.borrow_mut();
        let mut buffers = self.buffers.borrow_mut();
        for buffer in buffers.iter_mut() {
            buffer.pending.extend_from_slice(&damage);
            if buffer.pending.len() > 64 {
                buffer.pending = vec![full];
            }
        }
        let free = buffers
            .iter()
            .position(|slot| slot.buffer.canvas(&mut pool).is_some());
        let index = match free {
            Some(index) => index,
            None => {
                let (buffer, _) = pool
                    .create_buffer(
                        size.width as i32,
                        size.height as i32,
                        size.width as i32 * 4,
                        wl_shm::Format::Argb8888,
                    )
                    .map_err(error)?;
                buffers.push(ShmBuffer {
                    buffer,
                    pending: vec![full],
                });
                buffers.len() - 1
            }
        };
        let slot = &mut buffers[index];
        let canvas = slot
            .buffer
            .canvas(&mut pool)
            .ok_or_else(|| error("SHM buffer still in use"))?;
        for &(x, y, width, height) in &slot.pending {
            for row in y..y + height {
                let start = row * size.width as usize + x;
                for (pixel, bytes) in pixels[start..start + width].iter().zip(
                    canvas[start * 4..(start + width) * 4]
                        .as_chunks_mut::<4>()
                        .0,
                ) {
                    bytes.copy_from_slice(&[pixel.blue, pixel.green, pixel.red, pixel.alpha]);
                }
            }
        }
        slot.pending.clear();
        let copied = std::time::Instant::now();

        let mut mask = self.hit_mask.borrow_mut();
        let mut mask_changed = mask.len() != pixels.len();
        mask.resize(pixels.len(), false);
        for &(x, y, width, height) in &damage {
            for row in y..y + height {
                let start = row * size.width as usize + x;
                for index in start..start + width {
                    let visible = pixels[index].alpha > 16;
                    mask_changed |= mask[index] != visible;
                    mask[index] = visible;
                }
            }
        }
        drop(mask);
        if mask_changed {

            let mut hit = self.hit_region.borrow_mut();
            let spans = input_rectangles(&pixels, size.width as usize, self.scale.get());
            if hit.spans != spans {
                let region = Region::new(&*self.compositor).map_err(error)?;
                for (x, y, width, height) in &spans {
                    region.add(*x, *y, *width, *height);
                }
                hit.spans = spans;

                hit.region = Some(region);
                if let Some(blur) = &self.blur {
                    blur.set_region(hit.region.as_ref().map(Region::wl_region));
                    blur.commit();
                }
            }
        }
        for &(x, y, width, height) in &damage {
            self.layer
                .wl_surface()
                .damage_buffer(x as i32, y as i32, width as i32, height as i32);
        }
        let regions = std::time::Instant::now();
        let mut phases = self.paint_phases.get();
        for (total, delta) in
            phases
                .iter_mut()
                .zip([rendered - paint_start, copied - rendered, regions - copied])
        {
            *total += delta;
        }
        self.paint_phases.set(phases);
        self.layer
            .wl_surface()
            .frame(&self.qh, self.layer.wl_surface().clone());
        self.frame_pending.set(true);
        slot.buffer
            .attach_to(self.layer.wl_surface())
            .map_err(error)?;
        self.layer.commit();
        self.rendered_frames.set(self.rendered_frames.get() + 1);
        self.paint_time
            .set(self.paint_time.get() + paint_start.elapsed());
        Ok(())
    }
}

fn layer_requested_size(size: LogicalSize, anchors: crate::ShellAnchors) -> (u32, u32) {
    (
        if anchors.left && anchors.right {
            0
        } else {
            size.width.max(1.) as u32
        },
        if anchors.top && anchors.bottom {
            0
        } else {
            size.height.max(1.) as u32
        },
    )
}
impl WindowAdapterInternal for LayerWindow {
    fn create_child_window_adapter(
        &self,
        kind: i_slint_core::window::WindowKind,
    ) -> Option<Rc<dyn WindowAdapter>> {
        if matches!(kind, i_slint_core::window::WindowKind::ToolTip) {
            return None;
        }
        let parent = self.control.0.borrow().adapter.upgrade()?;
        match popup::PopupWindow::new(parent) {
            Ok(popup) => {
                self.popups.borrow_mut().push(Rc::downgrade(&popup));
                Some(popup)
            }
            Err(err) => {
                tracing::error!(%err, "Could not create native shell popup");
                None
            }
        }
    }
    fn set_mouse_cursor(&self, cursor: MouseCursor) {
        if self.cursor.replace(cursor) != cursor {
            self.update_cursor();
        }
    }
}
fn cursor_shape(cursor: MouseCursor) -> Shape {
    match cursor {
        MouseCursor::Help => Shape::Help,
        MouseCursor::Pointer => Shape::Pointer,
        MouseCursor::Progress => Shape::Progress,
        MouseCursor::Wait => Shape::Wait,
        MouseCursor::Crosshair => Shape::Crosshair,
        MouseCursor::Text => Shape::Text,
        MouseCursor::Alias => Shape::Alias,
        MouseCursor::Copy => Shape::Copy,
        MouseCursor::Move => Shape::Move,
        MouseCursor::NoDrop => Shape::NoDrop,
        MouseCursor::NotAllowed => Shape::NotAllowed,
        MouseCursor::Grab => Shape::Grab,
        MouseCursor::Grabbing => Shape::Grabbing,
        MouseCursor::ColResize => Shape::ColResize,
        MouseCursor::RowResize => Shape::RowResize,
        MouseCursor::NResize => Shape::NResize,
        MouseCursor::EResize => Shape::EResize,
        MouseCursor::SResize => Shape::SResize,
        MouseCursor::WResize => Shape::WResize,
        MouseCursor::NeResize => Shape::NeResize,
        MouseCursor::NwResize => Shape::NwResize,
        MouseCursor::SeResize => Shape::SeResize,
        MouseCursor::SwResize => Shape::SwResize,
        MouseCursor::EwResize => Shape::EwResize,
        MouseCursor::NsResize => Shape::NsResize,
        MouseCursor::NeswResize => Shape::NeswResize,
        MouseCursor::NwseResize => Shape::NwseResize,
        _ => Shape::Default,
    }
}
impl WindowAdapter for LayerWindow {
    fn internal(&self, _: i_slint_core::InternalToken) -> Option<&dyn WindowAdapterInternal> {
        Some(self)
    }

    fn window(&self) -> &Window {
        &self.window
    }
    fn renderer(&self) -> &dyn platform::Renderer {
        match &self.gpu {
            Some(renderer) => renderer,
            None => &self.renderer,
        }
    }
    fn size(&self) -> PhysicalSize {
        self.size.get()
    }
    fn set_size(&self, size: WindowSize) {
        self.request_size(size.to_logical(self.scale.get() as f32));
    }
    fn request_redraw(&self) {
        self.dirty.set(true);
    }
    fn set_visible(&self, visible: bool) -> Result<(), PlatformError> {
        self.visible.set(visible);
        if visible {
            self.dirty.set(true);
            self.layer.commit();
        } else {
            self.layer.wl_surface().attach(None, 0, 0);
            self.layer.commit();
        }
        Ok(())
    }
    fn update_window_properties(&self, properties: WindowProperties<'_>) {
        self.request_size(properties.layout_constraints().preferred);
    }
}

fn rounded_rectangles(rect: crate::SurfaceRect) -> Vec<(i32, i32, i32, i32)> {
    if ![rect.x, rect.y, rect.width, rect.height, rect.radius]
        .iter()
        .all(|v| v.is_finite())
        || rect.opacity <= 0.
        || rect.width <= 0.
        || rect.height <= 0.
    {
        return Vec::new();
    }
    let x = rect.x.floor() as i32;
    let y = rect.y.floor() as i32;
    let width = ((rect.x + rect.width).ceil() as i32 - x).max(0);
    let height = ((rect.y + rect.height).ceil() as i32 - y).max(0);
    let radius = rect
        .radius
        .max(0.)
        .min(rect.width.min(rect.height) / 2.)
        .ceil() as i32;
    let mut spans: Vec<(i32, i32, i32, i32)> = Vec::new();
    for row in 0..height {
        let edge = row.min(height - 1 - row);
        let inset = if edge < radius {
            let distance = (radius - edge) as f32 - 0.5;
            (radius as f32
                - ((radius * radius) as f32 - distance * distance)
                    .max(0.)
                    .sqrt())
            .floor() as i32
        } else {
            0
        };
        let span_width = (width - inset * 2).max(0);
        if let Some(last) = spans
            .last_mut()
            .filter(|last| last.0 == x + inset && last.2 == span_width)
        {
            last.3 += 1;
        } else {
            spans.push((x + inset, y + row, span_width, 1));
        }
    }
    if let Some(clip) = rect.clip {
        let left = clip.x.floor() as i32;
        let top = clip.y.floor() as i32;
        let right = (clip.x + clip.width).ceil() as i32;
        let bottom = (clip.y + clip.height).ceil() as i32;
        spans = spans
            .into_iter()
            .filter_map(|(x, y, w, h)| {
                let x1 = x.max(left);
                let y1 = y.max(top);
                let x2 = (x + w).min(right);
                let y2 = (y + h).min(bottom);
                (x2 > x1 && y2 > y1).then_some((x1, y1, x2 - x1, y2 - y1))
            })
            .collect();
    }
    spans
}

#[test]
fn clipped_input_region_keeps_the_original_curve_and_excludes_hidden_pixels() {
    let rect = crate::SurfaceRect {
        x: 8.,
        y: 24.25,
        width: 40.,
        height: 40.,
        radius: 10.,
        opacity: 1.,
        clip: Some(crate::SurfaceClip {
            x: 0.,
            y: 0.,
            width: 64.,
            height: 48.,
        }),
    };
    let spans = rounded_rectangles(rect);
    let contains = |px, py| {
        spans
            .iter()
            .any(|&(x, y, w, h)| px >= x && px < x + w && py >= y && py < y + h)
    };
    assert!(
        contains(8, 47),
        "Clipping a tall shape must not create a new bottom corner"
    );
    assert!(
        !contains(25, 49),
        "The hidden part must not intercept input"
    );
    assert!(!contains(8, 24), "Original top corner still applies");
    assert!(rounded_rectangles(crate::SurfaceRect {
        opacity: 0.,
        ..rect
    })
    .is_empty());
}

fn effective_layer(layer: ShellLayer, fullscreen: bool, overlay: bool) -> ShellLayer {
    match layer {
        ShellLayer::Top if fullscreen && !overlay => ShellLayer::Bottom,
        ShellLayer::Bottom if overlay => ShellLayer::Top,
        _ => layer,
    }
}

#[derive(Default)]
struct RegionCache {

    built: bool,
    source: Option<Vec<crate::SurfaceRect>>,
    region: Option<Region>,
}
impl RegionCache {

    fn region(
        &mut self,
        compositor: &CompositorState,
        rectangles: Option<&[crate::SurfaceRect]>,
    ) -> Result<(Option<wl_region::WlRegion>, bool), PlatformError> {
        if self.built && self.source.as_deref() == rectangles {
            return Ok((self.handle(), false));
        }
        self.built = true;
        self.source = rectangles.map(|rectangles| rectangles.to_vec());
        self.region = build_input_region(compositor, rectangles)?;
        Ok((self.handle(), true))
    }
    fn handle(&self) -> Option<wl_region::WlRegion> {
        self.region
            .as_ref()
            .map(|region| region.wl_region().clone())
    }
}

#[cfg(test)]
fn set_surface_input_region(
    surface: &wl_surface::WlSurface,
    compositor: &CompositorState,
    rectangles: Option<&[crate::SurfaceRect]>,
) -> Result<Option<Region>, PlatformError> {
    let region = build_input_region(compositor, rectangles)?;
    surface.set_input_region(region.as_ref().map(Region::wl_region));
    Ok(region)
}

fn build_input_region(
    compositor: &CompositorState,
    rectangles: Option<&[crate::SurfaceRect]>,
) -> Result<Option<Region>, PlatformError> {
    let Some(rectangles) = rectangles else {
        return Ok(None);
    };
    let region = Region::new(compositor).map_err(error)?;
    for rect in rectangles {
        for (x, y, width, height) in rounded_rectangles(*rect) {
            region.add(x, y, width, height);
        }
    }
    Ok(Some(region))
}

fn input_rectangles(
    pixels: &[PremultipliedRgbaColor],
    width: usize,
    scale: f64,
) -> Vec<(i32, i32, i32, i32)> {
    if width == 0 || !scale.is_finite() || scale <= 0. {
        return Vec::new();
    }
    let mut result: Vec<(i32, i32, i32, i32)> = Vec::new();
    let height = pixels.len() / width;
    let mut previous = std::collections::HashMap::new();
    let logical_width = (width as f64 / scale).round() as usize;
    let logical_height = (height as f64 / scale).round() as usize;
    for y in 0..logical_height {
        let visible = |x: usize| {
            let y_start = (y as f64 * scale).floor() as usize;
            let y_end = (((y + 1) as f64 * scale).ceil() as usize).min(height);
            let x_start = (x as f64 * scale).floor() as usize;
            let x_end = (((x + 1) as f64 * scale).ceil() as usize).min(width);
            (y_start..y_end).any(|py| (x_start..x_end).any(|px| pixels[py * width + px].alpha > 16))
        };
        let mut current = std::collections::HashMap::new();
        let mut x = 0;
        while x < logical_width {
            if !visible(x) {
                x += 1;
                continue;
            }
            let start = x;
            x += 1;
            while x < logical_width && visible(x) {
                x += 1;
            }
            let span = (start as i32, (x - start) as i32);
            let index = if let Some(&index) = previous.get(&span) {
                let rect: &mut (i32, i32, i32, i32) = &mut result[index];
                rect.3 += 1;
                index
            } else {
                result.push((span.0, y as i32, span.1, 1));
                result.len() - 1
            };
            current.insert(span, index);
        }
        previous = current;
    }
    result
}

struct WaylandState {
    registry: RegistryState,
    seats: SeatState,
    outputs: OutputState,
    shm: Shm,
    compositor: Rc<CompositorState>,
    layers: LayerShell,
    xdg: Option<Rc<XdgShell>>,
    blur: Option<OrgKdeKwinBlurManager>,
    effects: Option<RixSurfaceEffectsManagerV1>,
    viewporter: Option<WpViewporter>,
    fractional_scale: Option<WpFractionalScaleManagerV1>,
    cursor_shape: Option<WpCursorShapeManagerV1>,
    cursor_device: Option<WpCursorShapeDeviceV1>,
    window: Weak<LayerWindow>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    focused: bool,
    keyboard_surface: Option<wl_surface::WlSurface>,
    pressed: std::collections::HashMap<u32, slint::SharedString>,
    keymap: Option<xkbcommon::xkb::Keymap>,
    modifiers: Modifiers,
    clipboard: Rc<RefCell<clipboard::Clipboard>>,
    deferred_keys: RefCell<Option<Vec<WindowEvent>>>,
    repeat_info: RepeatInfo,
    repeating: Option<u32>,
    repeat_delay: slint::Timer,
    repeat_timer: Rc<slint::Timer>,
    exit: bool,
}
impl WaylandState {
    fn popup_for_surface(&self, surface: &wl_surface::WlSurface) -> Option<Rc<popup::PopupWindow>> {
        self.window
            .upgrade()?
            .live_popups()
            .into_iter()
            .find(|p| &p.surface == surface)
    }
    fn dispatch_key(&self, event: WindowEvent) {
        if let Some(keys) = self.deferred_keys.borrow_mut().as_mut() {

            if keys.len() < 64 || matches!(event, WindowEvent::KeyReleased { .. }) {
                keys.push(event);
            }
            return;
        }
        self.dispatch_key_now(event);
    }
    fn complete_deferred_keys(&self) {
        let keys = self.deferred_keys.borrow_mut().take();
        if let Some(keys) = keys {
            for event in keys {
                self.dispatch_key_now(event);
            }
        }
    }
    fn dispatch_key_now(&self, event: WindowEvent) {
        if let Some(root) = self.window.upgrade() {

            if let Some(popup) = root
                .live_popups()
                .into_iter()
                .rev()
                .find(|p| p.visible.get())
            {
                popup.window.dispatch_event(event);
            } else {
                root.window.dispatch_event(event);
            }
        }
    }
    fn notify_output_size(&self, output: &wl_output::WlOutput) {
        let Some(info) = self.outputs.info(output) else {
            return;
        };
        let Some(window) = self.window.upgrade() else {
            return;
        };

        if let Some(data) = window
            .layer
            .wl_surface()
            .data::<smithay_client_toolkit::compositor::SurfaceData>()
        {
            let entered: Vec<_> = data.outputs().collect();
            if !entered.is_empty() && !entered.contains(output) {
                return;
            }
        }
        let size = info.logical_size.or_else(|| {
            info.modes.iter().find(|m| m.current).map(|m| {
                (
                    m.dimensions.0 / info.scale_factor.max(1),
                    m.dimensions.1 / info.scale_factor.max(1),
                )
            })
        });
        let callback = window.control.0.borrow().output_size_changed.clone();
        if let (Some((width, height)), Some(callback)) = (size, callback) {
            callback(width.max(1) as u32, height.max(1) as u32);
        }
    }

    fn stop_repeat(&mut self) {
        self.repeating = None;
        self.repeat_delay.stop();
        self.repeat_timer.stop();
    }
}
impl CompositorHandler for WaylandState {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        scale: i32,
    ) {
        if let Some(popup) = self.popup_for_surface(surface) {
            if popup.fractional_scale.is_none() {
                let old = popup.scale.replace(scale.max(1) as f64);
                let size = popup.size.get();
                popup.resize(
                    (size.width as f64 / old).round() as u32,
                    (size.height as f64 / old).round() as u32,
                );
            }
            return;
        }
        if let Some(w) = self.window.upgrade() {
            if w.fractional_scale.is_some() {
                return;
            }
            let old = w.scale.replace(scale.max(1) as f64);
            w.layer.wl_surface().set_buffer_scale(scale.max(1));
            w.window.dispatch_event(WindowEvent::ScaleFactorChanged {
                scale_factor: scale.max(1) as f32,
            });
            w.resize(
                (w.size.get().width as f64 / old).round() as u32,
                (w.size.get().height as f64 / old).round() as u32,
            );
        }
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if let Some(popup) = self.popup_for_surface(surface) {
            popup.frame_pending.set(false);
            return;
        }
        if let Some(w) = self.window.upgrade() {
            w.frame_pending.set(false);
        }
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        self.notify_output_size(output);
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}
impl OutputHandler for WaylandState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.notify_output_size(&output);
    }
    fn update_output(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.notify_output_size(&output);
    }
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}
impl LayerShellHandler for WaylandState {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if let Some(w) = self.window.upgrade() {
            w.configured.set(true);
            w.awaiting_resize.set(false);
            let requested = w.requested.get();
            w.resize(
                if configure.new_size.0 == 0 {
                    requested.width as u32
                } else {
                    configure.new_size.0
                },
                if configure.new_size.1 == 0 {
                    requested.height as u32
                } else {
                    configure.new_size.1
                },
            );
        }
    }
}
impl SeatHandler for WaylandState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }
    fn new_seat(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        let mut clipboard = self.clipboard.borrow_mut();
        if clipboard.device.is_none() {
            clipboard.device = clipboard
                .manager
                .as_ref()
                .map(|manager| (seat.clone(), manager.get_data_device(qh, &seat)));
        }
    }
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seats.get_keyboard(qh, &seat, None).ok();
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seats.get_pointer(qh, &seat).ok();
            self.cursor_device = self
                .cursor_shape
                .as_ref()
                .zip(self.pointer.as_ref())
                .map(|(manager, pointer)| manager.get_pointer(pointer, qh, ()));
            if let Some(w) = self.window.upgrade() {
                *w.cursor_device.borrow_mut() = self.cursor_device.clone();
                *w.pointer.borrow_mut() = self.pointer.clone();
            }
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard {
            self.stop_repeat();
            self.pressed.clear();
            if let Some(k) = self.keyboard.take() {
                k.release();
            }
        }
        if capability == Capability::Pointer {
            if let Some(device) = self.cursor_device.take() {
                device.destroy();
            }
            if let Some(w) = self.window.upgrade() {
                w.cursor_device.borrow_mut().take();
                w.pointer.borrow_mut().take();
                w.cursor_enter.set(None);
            }
            if let Some(p) = self.pointer.take() {
                p.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        let mut clipboard = self.clipboard.borrow_mut();
        if clipboard
            .device
            .as_ref()
            .is_some_and(|(active, _)| *active == seat)
        {
            clipboard.leave();
            clipboard.device.take();
        }
    }
}
pub(super) fn key_text(event: &KeyEvent, modifiers: Modifiers) -> slint::SharedString {
    if let Some(letter) = crate::keyboard::shortcut_letter(event, modifiers) {
        return letter.to_string().into();
    }
    let key = match event.keysym {
        Keysym::Escape => Key::Escape,
        Keysym::Return | Keysym::KP_Enter => Key::Return,
        Keysym::BackSpace => Key::Backspace,
        Keysym::Delete => Key::Delete,
        Keysym::Tab | Keysym::ISO_Left_Tab => Key::Tab,
        Keysym::Left => Key::LeftArrow,
        Keysym::Right => Key::RightArrow,
        Keysym::Up => Key::UpArrow,
        Keysym::Down => Key::DownArrow,
        Keysym::Home => Key::Home,
        Keysym::End => Key::End,
        Keysym::Page_Up => Key::PageUp,
        Keysym::Page_Down => Key::PageDown,
        Keysym::Shift_L | Keysym::Shift_R => Key::Shift,
        Keysym::Control_L | Keysym::Control_R => Key::Control,
        Keysym::Alt_L | Keysym::Alt_R => Key::Alt,
        _ => return event.utf8.clone().unwrap_or_default().into(),
    };
    key.into()
}
impl KeyboardHandler for WaylandState {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        serial: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        self.focused = true;
        self.clipboard.borrow_mut().serial = Some(serial);
        self.keyboard_surface = Some(surface.clone());
        if let Some(w) = self.window.upgrade() {
            w.window
                .dispatch_event(WindowEvent::WindowActiveChanged(true));
            w.notify_focus(true);
        }
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        self.focused = false;
        self.modifiers = Modifiers::default();
        self.clipboard.borrow_mut().leave();
        self.deferred_keys.borrow_mut().take();
        self.keyboard_surface = None;
        self.stop_repeat();
        if let Some(w) = self.window.upgrade() {
            for (_, text) in self.pressed.drain() {
                w.window.dispatch_event(WindowEvent::KeyReleased { text });
            }
            w.window
                .dispatch_event(WindowEvent::WindowActiveChanged(false));
            if surface == w.layer.wl_surface() && w.live_popups().iter().all(|p| !p.visible.get()) {
                w.notify_focus(false);
            }
        }
    }
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        if !self.focused {
            return;
        }
        self.stop_repeat();
        self.clipboard.borrow_mut().serial = Some(serial);
        let text = key_text(&event, self.modifiers);
        if text.is_empty() {
            return;
        }
        self.pressed.insert(event.raw_code, text.clone());
        if self.modifiers.ctrl
            && !self.modifiers.shift
            && text.as_str() == "v"
            && self.clipboard.borrow().pending
        {
            self.deferred_keys.borrow_mut().get_or_insert_with(Vec::new);
        }
        self.dispatch_key(WindowEvent::KeyPressed { text: text.clone() });
        if self.deferred_keys.borrow().is_some() {
            return;
        }
        if self
            .keymap
            .as_ref()
            .is_some_and(|map| map.key_repeats(xkbcommon::xkb::Keycode::new(event.raw_code + 8)))
        {
            if let RepeatInfo::Repeat { delay, rate } = self.repeat_info {
                self.repeating = Some(event.raw_code);
                let target: Option<Rc<dyn WindowAdapter>> = self.window.upgrade().map(|root| {
                    root.live_popups()
                        .into_iter()
                        .rev()
                        .find(|p| p.visible.get())
                        .map(|p| p as Rc<dyn WindowAdapter>)
                        .unwrap_or(root)
                });
                let window = target.as_ref().map(Rc::downgrade);
                let timer = self.repeat_timer.clone();
                self.repeat_delay.start(
                    slint::TimerMode::SingleShot,
                    std::time::Duration::from_millis(delay as u64),
                    move || {
                        if let Some(w) = window.as_ref().and_then(Weak::upgrade) {
                            w.window().dispatch_event(WindowEvent::KeyPressRepeated {
                                text: text.clone(),
                            });
                        }
                        let window = window.clone();
                        let text = text.clone();
                        timer.start(
                            slint::TimerMode::Repeated,
                            std::time::Duration::from_secs_f64(1. / rate.get() as f64),
                            move || {
                                if let Some(w) = window.as_ref().and_then(Weak::upgrade) {
                                    w.window().dispatch_event(WindowEvent::KeyPressRepeated {
                                        text: text.clone(),
                                    });
                                }
                            },
                        );
                    },
                );
            }
        }
    }
    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.dispatch_key(WindowEvent::KeyPressRepeated {
            text: key_text(&event, self.modifiers),
        });
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if self.repeating == Some(event.raw_code) {
            self.stop_repeat();
        }
        if let Some(text) = self.pressed.remove(&event.raw_code) {
            self.dispatch_key(WindowEvent::KeyReleased { text });
        }
    }
    fn update_repeat_info(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        info: RepeatInfo,
    ) {
        self.stop_repeat();
        self.repeat_info = info;
    }
    fn update_keymap(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        keymap: smithay_client_toolkit::seat::keyboard::Keymap<'_>,
    ) {
        self.stop_repeat();
        self.keymap = xkbcommon::xkb::Keymap::new_from_string(
            &xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS),
            keymap.as_string(),
            xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        );
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: RawModifiers,
        group: u32,
    ) {
        self.modifiers = modifiers;
        let layout = self
            .keymap
            .as_ref()
            .filter(|keymap| group < keymap.num_layouts())
            .map(|keymap| keymap.layout_get_name(group).to_owned())
            .unwrap_or_default();
        if let Some(window) = self.window.upgrade() {
            window.control.0.borrow_mut().pending_layout = Some((layout, modifiers.caps_lock));
        }
    }
}
pub(super) fn pointer_button(button: u32) -> PointerEventButton {
    match button {
        0x110 => PointerEventButton::Left,
        0x111 => PointerEventButton::Right,
        0x112 => PointerEventButton::Middle,
        _ => PointerEventButton::Other,
    }
}
impl PointerHandler for WaylandState {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        for e in events {
            let popup = self.popup_for_surface(&e.surface);
            if popup.is_none() && &e.surface != w.layer.wl_surface() {
                continue;
            }
            let position = LogicalPosition::new(e.position.0 as f32, e.position.1 as f32);
            let event = match e.kind {
                PointerEventKind::Enter { serial, .. } => {
                    w.cursor_enter.set(Some(serial));
                    if let Some(popup) = &popup {
                        popup.set_cursor(serial);
                    } else {
                        w.update_cursor();
                    }
                    WindowEvent::PointerMoved { position }
                }
                PointerEventKind::Motion { .. } => WindowEvent::PointerMoved { position },
                PointerEventKind::Leave { .. } => {
                    w.cursor_enter.set(None);
                    WindowEvent::PointerExited
                }
                PointerEventKind::Press { button, serial, .. } => {

                    self.complete_deferred_keys();
                    self.clipboard.borrow_mut().serial = Some(serial);
                    if let Some(factory) = &w.popup_factory {
                        if let Some(pointer) = self.pointer.as_ref() {
                            if let Some(data) =
                                pointer.data::<smithay_client_toolkit::seat::pointer::PointerData>()
                            {
                                *factory.last_press.borrow_mut() =
                                    Some((data.seat().clone(), serial, std::time::Instant::now()));
                            }
                        }
                    }
                    WindowEvent::PointerPressed {
                        position,
                        button: pointer_button(button),
                    }
                }
                PointerEventKind::Release { button, .. } => WindowEvent::PointerReleased {
                    position,
                    button: pointer_button(button),
                },
                PointerEventKind::Axis {
                    horizontal,
                    vertical,
                    ..
                } => WindowEvent::PointerScrolled {
                    position,
                    delta_x: -horizontal.absolute as f32,
                    delta_y: -vertical.absolute as f32,
                },
            };
            if let Some(popup) = popup {
                popup.window.dispatch_event(event);
            } else {
                w.window.dispatch_event(event);
            }
        }
    }
}
impl ShmHandler for WaylandState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}
impl ProvidesRegistryState for WaylandState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}
delegate_compositor!(WaylandState);
delegate_output!(WaylandState);
delegate_shm!(WaylandState);
delegate_seat!(WaylandState);
delegate_data_device!(WaylandState);
delegate_keyboard!(WaylandState);
delegate_pointer!(WaylandState);
delegate_layer!(WaylandState);
delegate_registry!(WaylandState);
delegate_xdg_shell!(WaylandState);
delegate_xdg_popup!(WaylandState);
wayland_client::delegate_noop!(WaylandState: ignore wayland_protocols::xdg::shell::client::xdg_positioner::XdgPositioner);

#[cfg(test)]
#[path = "wayland_input_tests.rs"]
mod input_tests;

#[cfg(test)]
#[path = "wayland_paste_tests.rs"]
mod paste_tests;

#[cfg(test)]
mod tests {
    use super::*;
    fn test_proxy(capacity: usize) -> (Proxy, mpsc::Receiver<Message>, UnixStream) {
        let (read, write) = UnixStream::pair().unwrap();
        write.set_nonblocking(true).unwrap();
        let (sender, receiver) = mpsc::sync_channel(capacity);
        (
            Proxy {
                sender,
                wake: Arc::new(Mutex::new(write)),
                mailbox: Arc::new(Mailbox::default()),
                loop_thread: std::thread::current().id(),
            },
            receiver,

            read,
        )
    }
    #[test]
    fn saturated_queue_coalesces_quit_without_blocking() {
        let (proxy, receiver, _wake_guard) = test_proxy(1);
        assert!(proxy.send(Message::Quit).is_ok());
        assert!(proxy.send(Message::Quit).is_ok());
        assert!(proxy.send(Message::Quit).is_ok());
        assert!(proxy.mailbox.quit.load(Ordering::Acquire));
        assert!(matches!(receiver.try_recv(), Ok(Message::Quit)));
        assert!(receiver.try_recv().is_err());
    }
    #[test]
    fn invoke_from_the_loop_thread_runs_inline_when_the_queue_is_full() {
        let (proxy, _receiver, _wake_guard) = test_proxy(1);
        assert!(proxy.send(Message::Quit).is_ok());
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        assert!(proxy
            .send(Message::Invoke(Box::new(
                move || flag.store(true, Ordering::Release)
            )))
            .is_ok());
        assert!(ran.load(Ordering::Acquire));
    }
    #[test]
    fn invoke_from_another_thread_waits_for_space_and_is_never_lost() {
        let (proxy, receiver, _wake_guard) = test_proxy(1);
        assert!(proxy.send(Message::Quit).is_ok());
        let background = proxy.clone();
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        let producer = std::thread::spawn(move || {
            assert!(background
                .send(Message::Invoke(Box::new(
                    move || flag.store(true, Ordering::Release)
                )))
                .is_ok());
        });

        assert!(matches!(receiver.try_recv(), Ok(Message::Quit)));
        producer.join().unwrap();
        let received = receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("Deferred invoke must arrive after space is freed");
        match received {
            Message::Invoke(f) => f(),
            _ => panic!("Expected the deferred invoke"),
        }
        assert!(ran.load(Ordering::Acquire));
    }
    #[test]
    fn a_closed_loop_releases_a_parked_producer() {
        let (proxy, _receiver, _wake_guard) = test_proxy(1);
        assert!(proxy.send(Message::Quit).is_ok());
        let background = proxy.clone();
        let producer =
            std::thread::spawn(move || background.send(Message::Invoke(Box::new(|| {}))).is_err());
        proxy.close();
        assert!(
            producer.join().unwrap(),
            "A parked producer must fail once the loop stopped draining"
        );
    }
    slint::slint! {
        export component ResizeProbe inherits Window {
            width: 640px;
            height: 46px;
            background: #203040;
        }
    }
    #[test]
    #[ignore = "requires a disposable Wayland compositor"]
    fn native_anchored_resize_does_not_stall_frames() {
        use slint::ComponentHandle;
        let control = install(Default::default()).unwrap();
        let window = ResizeProbe::new().unwrap();
        let mut tick = 0;
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(100),
            move || {
                tick += 1;
                let adapter = control.0.borrow().adapter.upgrade().unwrap();
                if tick == 5 {
                    assert!(adapter.configured.get());
                    assert!(!adapter.awaiting_resize.get());
                    let current = adapter.size.get();
                    adapter.request_size(LogicalSize::new(current.width as f32 + 100., 46.));
                    assert!(
                        !adapter.awaiting_resize.get(),
                        "Compositor-owned width must not stall frames waiting for configure"
                    );
                    adapter.request_redraw();
                }
                if tick == 8 {
                    assert!(
                        !adapter.frame_pending.get(),
                        "The frame callback must have completed without input"
                    );
                    adapter.request_size(LogicalSize::new(700., 80.));
                    assert!(
                        adapter.awaiting_resize.get(),
                        "Actual height changes still wait for configure"
                    );
                }
                if tick == 13 {
                    assert!(!adapter.awaiting_resize.get());
                    assert_eq!(
                        (adapter.size.get().height as f64 / adapter.scale.get()).round() as u32,
                        80
                    );
                    slint::quit_event_loop().unwrap();
                }
            },
        );
        window.run().unwrap();
    }
    #[test]
    fn anchored_preferred_width_does_not_request_another_configure() {
        let anchors = crate::ShellAnchors::from_position(rix_core::Position::Bottom);
        assert_eq!(
            layer_requested_size(LogicalSize::new(1280., 46.), anchors),
            layer_requested_size(LogicalSize::new(1920., 46.), anchors)
        );
        assert_ne!(
            layer_requested_size(LogicalSize::new(1920., 46.), anchors),
            layer_requested_size(LogicalSize::new(1920., 546.), anchors)
        );
        assert_eq!(
            layer_requested_size(LogicalSize::new(1920.1, 46.1), anchors),
            layer_requested_size(LogicalSize::new(1920.9, 46.9), anchors)
        );
    }
    #[test]
    fn transparent_gap_does_not_steal_input() {
        let mut pixels = vec![PremultipliedRgbaColor::default(); 80];
        for y in 0..3 {
            for x in 2..6 {
                pixels[y * 10 + x].alpha = 200;
            }
        }
        for pixel in &mut pixels[60..] {
            pixel.alpha = 255;
        }
        assert_eq!(
            input_rectangles(&pixels, 10, 1.),
            [(2, 0, 4, 3), (0, 6, 10, 2)]
        );
    }

    #[test]
    fn panels_stack_above_regular_windows_and_below_fullscreen_until_popup_closes() {
        assert_eq!(
            effective_layer(ShellLayer::Top, false, false),
            ShellLayer::Top
        );
        assert_eq!(
            effective_layer(ShellLayer::Top, true, false),
            ShellLayer::Bottom
        );
        assert_eq!(
            effective_layer(ShellLayer::Top, true, true),
            ShellLayer::Top
        );
        assert_eq!(
            effective_layer(ShellLayer::Background, true, true),
            ShellLayer::Background
        );
    }

    #[test]
    fn software_hit_regions_follow_fractional_scale() {
        let pixels = vec![
            PremultipliedRgbaColor {
                red: 0,
                green: 0,
                blue: 0,
                alpha: 255
            };
            25
        ];
        assert_eq!(input_rectangles(&pixels, 5, 1.25), [(0, 0, 4, 4)]);
        assert_eq!(input_rectangles(&pixels, 5, 0.5), [(0, 0, 10, 10)]);
        assert!(input_rectangles(&pixels, 0, 1.).is_empty());
        assert!(input_rectangles(&pixels, 5, f64::NAN).is_empty());
    }
}

wayland_client::delegate_noop!(WaylandState: ignore OrgKdeKwinBlurManager);
wayland_client::delegate_noop!(WaylandState: ignore OrgKdeKwinBlur);

wayland_client::delegate_noop!(WaylandState: ignore WpCursorShapeManagerV1);
wayland_client::delegate_noop!(WaylandState: ignore WpCursorShapeDeviceV1);

wayland_client::delegate_noop!(WaylandState: ignore RixSurfaceEffectsManagerV1);
wayland_client::delegate_noop!(WaylandState: ignore RixSurfaceEffectsV1);

impl Dispatch<WpFractionalScaleV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        fractional: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            if scale == 0 {
                return;
            }
            if let Some(popup) = state.window.upgrade().and_then(|w| {
                w.live_popups()
                    .into_iter()
                    .find(|p| p.fractional_scale.as_ref() == Some(fractional))
            }) {
                let scale = f64::from(scale) / 120.;
                let old = popup.scale.replace(scale);
                if old != scale {
                    let size = popup.size.get();
                    popup.resize(
                        (size.width as f64 / old).round() as u32,
                        (size.height as f64 / old).round() as u32,
                    );
                }
                return;
            }
            let Some(window) = state.window.upgrade() else {
                return;
            };
            if scale == 0 {
                return;
            }
            let scale = f64::from(scale) / 120.;
            let old = window.scale.replace(scale);
            if old == scale {
                return;
            }
            window
                .window
                .dispatch_event(WindowEvent::ScaleFactorChanged {
                    scale_factor: scale as f32,
                });
            let size = window.size.get();
            window.resize(
                (size.width as f64 / old).round() as u32,
                (size.height as f64 / old).round() as u32,
            );
            window.dirty.set(true);
        }
    }
}
wayland_client::delegate_noop!(WaylandState: ignore WpViewporter);
wayland_client::delegate_noop!(WaylandState: ignore WpViewport);
wayland_client::delegate_noop!(WaylandState: ignore WpFractionalScaleManagerV1);
