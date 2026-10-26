use crate::{FilesWindow, MenuItem, NativeActionsMenu, Palette, SkinRenderer, Theme};
use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
use rix_config::ThemeSettings;
use slint::{
    platform::{
        software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType},
        Key, Platform, WindowAdapter, WindowEvent,
    },
    winit_030::{winit::platform::wayland::WindowExtWayland, WinitWindowAccessor},
    ComponentHandle, LogicalPosition, LogicalSize, ModelRc, PhysicalSize, Timer, TimerMode,
    VecModel,
};
use smithay_client_toolkit::shell::xdg::{window::Window as SctkWindow, XdgSurface};
use std::{
    cell::RefCell,
    fs::File,
    os::{fd::AsFd, unix::fs::FileExt},
    rc::Rc,
    sync::mpsc::{self, Receiver, Sender, SyncSender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use wayland_client::{
    backend::{Backend, ObjectId, WaylandError},
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_pointer, wl_region, wl_registry,
        wl_seat, wl_shm, wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::xdg::shell::client::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};
use wayland_protocols_plasma::blur::client::{
    org_kde_kwin_blur::OrgKdeKwinBlur, org_kde_kwin_blur_manager::OrgKdeKwinBlurManager,
};
use xkbcommon::xkb;

const TICK: Duration = Duration::from_millis(16);
const IDLE_TICK: Duration = Duration::from_secs(1);
const TIMEOUT: Duration = Duration::from_secs(3);
const MAX_BYTES: usize = 16 * 1024 * 1024;

fn polling_interval(
    worker_ready: bool,
    registry_ready: bool,
    input_ready: bool,
    popup_open: bool,
) -> Duration {
    if worker_ready && registry_ready && input_ready && !popup_open {
        IDLE_TICK
    } else {
        TICK
    }
}

#[derive(Clone, Debug)]
pub struct OwnedMenuItem {
    pub action: String,
    pub label: String,
    pub shortcut: String,
    pub enabled: bool,
    pub separator: bool,
    pub destructive: bool,
    pub checked: bool,
}
impl From<MenuItem> for OwnedMenuItem {
    fn from(item: MenuItem) -> Self {
        Self {
            action: item.action.to_string(),
            label: item.label.to_string(),
            shortcut: item.shortcut.to_string(),
            enabled: item.enabled,
            separator: item.separator,
            destructive: item.destructive,
            checked: item.checked,
        }
    }
}
impl From<OwnedMenuItem> for MenuItem {
    fn from(item: OwnedMenuItem) -> Self {
        Self {
            action: item.action.into(),
            label: item.label.into(),
            shortcut: item.shortcut.into(),
            enabled: item.enabled,
            separator: item.separator,
            destructive: item.destructive,
            checked: item.checked,
        }
    }
}
#[derive(Clone, Debug)]
pub struct MenuRequest {
    pub items: Vec<OwnedMenuItem>,
    pub title: String,

    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub radius: f32,
}
#[derive(Clone, Debug)]
pub enum NativeEvent {
    Shown,
    Dismissed,
    Activated(String),
    Unavailable,
    ReleaseParentPointer(LogicalPosition, slint::platform::PointerEventButton),
}

enum Command {
    Show(u64, MenuRequest, Box<ThemeSettings>),
    Input(WindowEvent),
    Theme(Box<ThemeSettings>),
    Hide,
    Stop,
}
enum WorkerEvent {
    Ready,
    Failed(String),
    Dismissed(u64),
    Activated(u64, String),
}
struct Frame {
    generation: u64,
    width: u32,
    height: u32,
    bytes: Vec<u8>,
}

struct SoftwarePlatform(Rc<MinimalSoftwareWindow>);
impl Platform for SoftwarePlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

fn worker(commands: Receiver<Command>, events: Sender<WorkerEvent>, frames: SyncSender<Frame>) {
    let run = || -> Result<(), String> {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(SoftwarePlatform(adapter.clone())))
            .map_err(|e| e.to_string())?;
        let menu = NativeActionsMenu::new().map_err(|e| e.to_string())?;
        crate::skins::install(menu.global::<SkinRenderer>(), menu.as_weak());
        let generation = Rc::new(std::cell::Cell::new(0u64));
        let selector = Rc::new(std::cell::Cell::new(false));
        let sent = events.clone();
        let current = generation.clone();
        menu.on_activated(move |action| {
            let _ = sent.send(WorkerEvent::Activated(current.get(), action.to_string()));
        });
        let sent = events.clone();
        let current = generation.clone();
        menu.on_dismissed(move || {
            let _ = sent.send(WorkerEvent::Dismissed(current.get()));
        });
        let weak = menu.as_weak();
        let selector_input = selector.clone();
        menu.on_next_index(move |index, direction| {
            let Some(menu) = weak.upgrade() else {
                return -1;
            };
            use slint::Model;
            let model = menu.get_items();
            let rows: Vec<_> = (0..model.row_count())
                .filter_map(|i| model.row_data(i))
                .collect();
            if selector_input.get() {
                next_selector_index(&rows, index, direction)
            } else {
                next_index(&rows, index, direction)
            }
        });
        menu.on_rows_height(crate::controller::menu_rows_height);
        menu.on_items_width(crate::controller::menu_width);
        let _ = events.send(WorkerEvent::Ready);
        let mut shown = false;
        let mut physical = PhysicalSize::new(1, 1);
        let mut pixels = Vec::<PremultipliedRgbaColor>::new();
        let mut pending_frame = None;
        loop {
            let received = if shown {
                commands.recv_timeout(TICK)
            } else {
                commands
                    .recv()
                    .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
            };
            let first = match received {
                Ok(command) => Some(command),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            };
            for command in first.into_iter().chain(commands.try_iter()) {
                match command {
                    Command::Stop => return Ok(()),
                    Command::Hide => {
                        shown = false;
                        pending_frame = None;
                        menu.hide().map_err(|e| e.to_string())?;
                    }
                    Command::Theme(theme) => {
                        crate::theme_bindings::apply_for_app(
                            menu.global::<Theme>(),
                            menu.global::<Palette>(),
                            &theme,
                        );
                        menu.set_material_supported(theme.blur_radius > 0);
                    }
                    Command::Show(id, request, theme) => {
                        generation.set(id);

                        selector.set(request.items.iter().enumerate().all(|(index, item)| {
                            !item.separator && item.action == index.to_string()
                        }));
                        crate::theme_bindings::apply_for_app(
                            menu.global::<Theme>(),
                            menu.global::<Palette>(),
                            &theme,
                        );
                        menu.set_material_supported(theme.blur_radius > 0);
                        let row_height = menu.global::<Theme>().get_menu_row_height();
                        let separator_height = menu.global::<Theme>().get_default_spacing() + 1.;
                        let selected =
                            initial_selection(&request.items, row_height, separator_height);
                        menu.set_initial_index(
                            selected.map(|(index, _)| index as i32).unwrap_or(-1),
                        );
                        menu.set_initial_row_top(selected.map(|(_, top)| top).unwrap_or(-1.));
                        menu.set_initial_row_height(row_height);
                        menu.set_items(ModelRc::new(VecModel::from(
                            request
                                .items
                                .into_iter()
                                .map(MenuItem::from)
                                .collect::<Vec<_>>(),
                        )));
                        menu.set_menu_title(request.title.into());
                        adapter
                            .window()
                            .dispatch_event(WindowEvent::ScaleFactorChanged {
                                scale_factor: request.scale,
                            });
                        adapter.set_size(LogicalSize::new(
                            request.width as f32,
                            request.height as f32,
                        ));
                        physical = PhysicalSize::new(
                            (request.width as f32 * request.scale).ceil() as u32,
                            (request.height as f32 * request.scale).ceil() as u32,
                        );
                        let count = physical.width as usize * physical.height as usize;
                        if count > MAX_BYTES / 4 {
                            return Err("Native menu framebuffer exceeds its size limit".into());
                        }
                        pixels.resize(count, PremultipliedRgbaColor::default());
                        menu.show().map_err(|e| e.to_string())?;
                        menu.invoke_focus_menu();
                        adapter
                            .window()
                            .dispatch_event(WindowEvent::WindowActiveChanged(true));
                        adapter.window().request_redraw();
                        shown = true;
                        pending_frame = None;
                    }
                    Command::Input(event) if shown => adapter.window().dispatch_event(event),
                    Command::Input(_) => {}
                }
            }
            slint::platform::update_timers_and_animations();
            if shown {
                adapter.draw_if_needed(|renderer| {
                    pixels.fill(PremultipliedRgbaColor::default());
                    renderer.render(&mut pixels, physical.width as usize);

                    let scale = adapter.window().scale_factor();
                    clip_rounded_pixels(
                        &mut pixels,
                        physical.width,
                        menu.get_menu_width() * scale,
                        menu.get_menu_height() * scale,
                        menu.get_menu_radius() * scale,
                    );
                    let mut bytes = Vec::with_capacity(pixels.len() * 4);
                    for p in &pixels {
                        let argb = (u32::from(p.alpha) << 24)
                            | (u32::from(p.red) << 16)
                            | (u32::from(p.green) << 8)
                            | u32::from(p.blue);
                        bytes.extend_from_slice(&argb.to_ne_bytes());
                    }
                    pending_frame = Some(Frame {
                        generation: generation.get(),
                        width: physical.width,
                        height: physical.height,
                        bytes,
                    });
                });
                if let Some(frame) = pending_frame.take() {
                    match frames.try_send(frame) {
                        Ok(()) => {}
                        Err(mpsc::TrySendError::Full(frame)) => pending_frame = Some(frame),
                        Err(mpsc::TrySendError::Disconnected(_)) => return Ok(()),
                    }
                }
            }
        }
    };
    if let Err(error) = run() {
        let _ = events.send(WorkerEvent::Failed(error));
    }
}
fn next_index(rows: &[MenuItem], index: i32, direction: i32) -> i32 {
    if rows.is_empty() {
        return -1;
    }
    let step = if direction < 0 { -1 } else { 1 };
    let mut index = if index < 0 {
        if step > 0 {
            -1
        } else {
            rows.len() as i32
        }
    } else {
        index
    };
    for _ in rows {
        index = (index + step).rem_euclid(rows.len() as i32);
        let row = &rows[index as usize];
        if row.enabled && !row.separator {
            return index;
        }
    }
    -1
}

fn next_selector_index(rows: &[MenuItem], index: i32, direction: i32) -> i32 {
    let step = if direction < 0 { -1 } else { 1 };
    let count = rows.len() as i32;
    let mut candidate = if index < 0 {
        if step < 0 {
            count - 1
        } else {
            0
        }
    } else {
        index.saturating_add(step)
    };
    while candidate >= 0 && candidate < count {
        let row = &rows[candidate as usize];
        if row.enabled && !row.separator {
            return candidate;
        }
        candidate += step;
    }
    if index >= 0
        && index < count
        && rows[index as usize].enabled
        && !rows[index as usize].separator
    {
        index
    } else {
        -1
    }
}

fn initial_selection(
    rows: &[OwnedMenuItem],
    row_height: f32,
    separator_height: f32,
) -> Option<(usize, f32)> {
    let mut top = 0.;
    for (index, row) in rows.iter().enumerate() {
        if row.checked && row.enabled && !row.separator {
            return Some((index, top));
        }
        top += if row.separator {
            separator_height
        } else {
            row_height
        };
    }
    None
}

pub struct NativePopup {
    timer: Timer,
    window: slint::Weak<FilesWindow>,
    bridge: Option<Bridge>,
    theme: ThemeSettings,
    commands: Sender<Command>,
    worker_events: Receiver<WorkerEvent>,
    frames: Receiver<Frame>,
    worker: Option<JoinHandle<()>>,
    ready: bool,
    failed: bool,
    started: Instant,
    generation: u64,
    parent_release: Option<(LogicalPosition, slint::platform::PointerEventButton)>,
    on_event: Rc<dyn Fn(NativeEvent)>,
}

pub fn install(
    window: &FilesWindow,
    theme: &ThemeSettings,
    on_event: impl Fn(NativeEvent) + 'static,
) -> Rc<RefCell<NativePopup>> {
    let (commands, receiver) = mpsc::channel();
    let (events, worker_events) = mpsc::channel();
    let (frames_sender, frames) = mpsc::sync_channel(1);
    let spawned = thread::Builder::new()
        .name("rix-files-menu".into())
        .spawn(move || worker(receiver, events, frames_sender));
    let popup = Rc::new(RefCell::new(NativePopup {
        timer: Timer::default(),
        window: window.as_weak(),
        bridge: None,
        theme: theme.clone(),
        commands,
        worker_events,
        frames,
        worker: spawned.ok(),
        ready: false,
        failed: false,
        started: Instant::now(),
        generation: 0,
        parent_release: None,
        on_event: Rc::new(on_event),
    }));
    let weak = Rc::downgrade(&popup);
    popup
        .borrow()
        .timer
        .start(TimerMode::Repeated, TICK, move || {
            let Some(popup) = weak.upgrade() else {
                return;
            };
            let (events, callback) = {
                let mut popup = popup.borrow_mut();
                (popup.tick(), popup.on_event.clone())
            };
            for event in events {
                callback(event);
            }
        });
    popup
}
impl NativePopup {
    pub fn update(&mut self, theme: &ThemeSettings) {
        self.theme = theme.clone();
        let _ = self.commands.send(Command::Theme(Box::new(theme.clone())));
        if let Some(bridge) = &mut self.bridge {
            if let Some(request) = &mut bridge.state.request {
                request.radius = theme.appearance.metric("context_menu_radius");
            }
            bridge.apply_blur(theme.blur_radius > 0);
        }
    }
    pub fn open(&mut self, mut request: MenuRequest) -> bool {
        if self.failed
            || !self.ready
            || !request.scale.is_finite()
            || request.scale <= 0.0
            || request.width == 0
            || request.height == 0
            || request.width > 4096
            || request.height > 4096
            || !request.radius.is_finite()
            || request.width as f64 * request.height as f64 * f64::from(request.scale).powi(2) * 4.0
                > MAX_BYTES as f64
        {
            return false;
        }
        let Some(bridge) = &mut self.bridge else {
            return false;
        };

        if let Err(error) = bridge.poll() {
            tracing::debug!(%error, "Native menu input queue unavailable");
            return false;
        }
        if !bridge.state.registry_ready || bridge.state.blur.is_none() {
            return false;
        }

        request.scale = request.scale.ceil().clamp(1., 4.);
        if request.width as f64 * request.height as f64 * f64::from(request.scale).powi(2) * 4.0
            > MAX_BYTES as f64
        {
            return false;
        }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        match bridge.open(generation, request, self.theme.blur_radius > 0) {
            Ok(()) => {
                self.timer.set_interval(TICK);

                self.parent_release = bridge.state.parent_press.take();
                true
            }
            Err(error) => {
                tracing::debug!(%error, "Native menu unavailable");
                false
            }
        }
    }
    pub fn close(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(bridge) = &mut self.bridge {
            bridge.close();
        }
        let _ = self.commands.send(Command::Hide);
    }
    fn fail(&mut self, error: &str) -> Vec<NativeEvent> {
        tracing::debug!(%error, "Native menu bridge unavailable");
        self.failed = true;
        self.close();
        self.bridge = None;
        self.timer.stop();
        vec![NativeEvent::Unavailable]
    }
    fn tick(&mut self) -> Vec<NativeEvent> {
        if let Some((position, button)) = self.parent_release.take() {
            return vec![NativeEvent::ReleaseParentPointer(position, button)];
        }
        if self.failed {
            return vec![];
        }
        let Some(window) = self.window.upgrade() else {
            self.close();
            self.timer.stop();
            return vec![];
        };
        for event in self.worker_events.try_iter().collect::<Vec<_>>() {
            match event {
                WorkerEvent::Ready => self.ready = true,
                WorkerEvent::Failed(error) => return self.fail(&error),
                WorkerEvent::Dismissed(id) if id == self.generation => {
                    self.close();
                    return vec![NativeEvent::Dismissed];
                }
                WorkerEvent::Activated(id, action) if id == self.generation => {
                    self.close();
                    return vec![NativeEvent::Activated(action)];
                }
                _ => {}
            }
        }
        if self.bridge.is_none() {
            if !window.window().is_visible() {
                self.started = Instant::now();
                return vec![];
            }
            match Bridge::from_window(&window) {
                Ok(Some(bridge)) => {
                    self.bridge = Some(bridge);
                    self.started = Instant::now();
                }
                Ok(None) if self.started.elapsed() < TIMEOUT => return vec![],
                Ok(None) => return self.fail("Parent Wayland surface was not available"),
                Err(error) => return self.fail(&error),
            }
        }
        let bridge = self.bridge.as_mut().unwrap();
        if let Err(error) = bridge.poll() {
            return self.fail(&error);
        }
        if !bridge.state.registry_ready && self.started.elapsed() >= TIMEOUT {
            return self.fail("Compositor registry probe timed out");
        }
        let mut events = Vec::new();
        if bridge.state.dismissed {
            self.generation = self.generation.wrapping_add(1);
            bridge.state.dismissed = false;
            bridge.close();
            let _ = self.commands.send(Command::Hide);
            events.push(NativeEvent::Dismissed);
        }
        if let Some(mut request) = bridge.state.configured.take() {
            request.scale = bridge.scale as f32;
            if self
                .commands
                .send(Command::Show(
                    self.generation,
                    request,
                    Box::new(self.theme.clone()),
                ))
                .is_err()
            {
                return self.fail("Menu renderer stopped");
            }
        }
        for input in bridge.state.input.drain(..) {
            let _ = self.commands.send(Command::Input(input));
        }
        for frame in self.frames.try_iter() {
            if frame.generation == self.generation {
                if let Err(error) = bridge.present(frame) {
                    return self.fail(&error);
                }
            }
        }
        if let Err(error) = bridge.present_pending() {
            return self.fail(&error);
        }
        if bridge
            .popup
            .as_ref()
            .is_some_and(|popup| !popup.mapped && popup.created.elapsed() > TIMEOUT)
        {
            return self.fail("Compositor did not map the native menu in time");
        }
        if bridge.shown {
            bridge.shown = false;
            events.push(NativeEvent::Shown);
        }
        if let Err(error) = bridge.flush() {
            return self.fail(&error);
        }

        self.timer.set_interval(polling_interval(
            self.ready,
            bridge.state.registry_ready,
            bridge.state.input_ready,
            bridge.popup.is_some(),
        ));
        events
    }
}
impl Drop for NativePopup {
    fn drop(&mut self) {
        self.timer.stop();
        self.close();
        self.bridge = None;
        let _ = self.commands.send(Command::Stop);

        self.worker.take();
    }
}

#[derive(Default)]
struct ProtocolState {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    shell: Option<xdg_wm_base::XdgWmBase>,
    blur: Option<OrgKdeKwinBlurManager>,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    registry_ready: bool,
    input_ready: bool,
    input_binding_generation: u64,
    parent: Option<wl_surface::WlSurface>,
    popup: Option<wl_surface::WlSurface>,
    popup_role: Option<xdg_popup::XdgPopup>,
    popup_xdg: Option<xdg_surface::XdgSurface>,
    focused_pointer: Option<wl_surface::WlSurface>,
    focused_keyboard: Option<wl_surface::WlSurface>,
    pointer_position: LogicalPosition,
    pressed: Option<slint::platform::PointerEventButton>,
    serial: Option<(u32, Instant)>,
    parent_press: Option<(LogicalPosition, slint::platform::PointerEventButton)>,
    request: Option<MenuRequest>,
    configured: Option<MenuRequest>,
    dismissed: bool,
    input: Vec<WindowEvent>,
    released: Vec<u64>,
    repeat_rate: i32,
    repeat_delay: i32,
    repeat: Option<(u32, Instant)>,
    keymap: Option<xkb::State>,
    alt: bool,
}
impl Dispatch<wl_registry::WlRegistry, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" if state.compositor.is_none() && version >= 4 => {
                    state.compositor = Some(registry.bind(name, 4, qh, ()))
                }
                "wl_shm" if state.shm.is_none() => state.shm = Some(registry.bind(name, 1, qh, ())),
                "xdg_wm_base" if state.shell.is_none() => {
                    state.shell = Some(registry.bind(name, version.min(3), qh, ()))
                }
                "org_kde_kwin_blur_manager" if state.blur.is_none() => {
                    state.blur = Some(registry.bind(name, 1, qh, ()))
                }
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(7), qh, ()))
                }
                _ => {}
            }
        }
    }
}
impl Dispatch<wl_callback::WlCallback, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.registry_ready = true;
        }
    }
}

#[derive(Clone, Copy)]
struct InputReady(u64);

impl ProtocolState {
    fn complete_input_binding(&mut self, generation: u64) {
        if generation == self.input_binding_generation {
            self.input_ready = true;
        }
    }
}

impl Dispatch<wl_callback::WlCallback, InputReady> for ProtocolState {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        ready: &InputReady,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.complete_input_binding(ready.0);
        }
    }
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for ProtocolState {
    fn event(
        _: &mut Self,
        proxy: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            proxy.pong(serial);
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        proxy: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if state.popup_xdg.as_ref() != Some(proxy) {
            return;
        }
        if let xdg_surface::Event::Configure { serial } = event {
            proxy.ack_configure(serial);
            state.configured = state.request.clone();
        }
    }
}
impl Dispatch<xdg_popup::XdgPopup, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        proxy: &xdg_popup::XdgPopup,
        event: xdg_popup::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if state.popup_role.as_ref() != Some(proxy) {
            return;
        }
        match event {
            xdg_popup::Event::Configure {
                x,
                y,
                width,
                height,
            } => {
                if let Some(request) = &mut state.request {
                    request.x = x;
                    request.y = y;
                    if width > 0 && height > 0 {
                        request.width = width as u32;
                        request.height = height as u32;
                    }
                }
            }
            xdg_popup::Event::PopupDone => state.dismissed = true,
            _ => {}
        }
    }
}
impl Dispatch<wl_seat::WlSeat, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        proxy: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        connection: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            state.input_ready = false;
            state.input_binding_generation = state.input_binding_generation.wrapping_add(1);
            if capabilities.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(proxy.get_pointer(qh, ()));
            }
            if capabilities.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(proxy.get_keyboard(qh, ()));
            }
            if !capabilities.contains(wl_seat::Capability::Pointer) {
                if let Some(pointer) = state.pointer.take() {
                    if pointer.version() >= 3 {
                        pointer.release();
                    }
                }
            }
            if !capabilities.contains(wl_seat::Capability::Keyboard) {
                if let Some(keyboard) = state.keyboard.take() {
                    if keyboard.version() >= 3 {
                        keyboard.release();
                    }
                }
            }

            connection
                .display()
                .sync(qh, InputReady(state.input_binding_generation));
        }
    }
}
fn pointer_button(button: u32) -> Option<slint::platform::PointerEventButton> {
    use slint::platform::PointerEventButton;
    match button {
        272 => Some(PointerEventButton::Left),
        273 => Some(PointerEventButton::Right),
        274 => Some(PointerEventButton::Middle),
        _ => None,
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                state.focused_pointer = Some(surface);
                state.pointer_position = LogicalPosition::new(surface_x as f32, surface_y as f32);
                let selector = state.request.as_ref().is_some_and(|request| {
                    request
                        .items
                        .iter()
                        .enumerate()
                        .all(|(index, item)| item.action == index.to_string())
                });
                if state.popup.is_some() && state.focused_pointer == state.popup && !selector {
                    state.input.push(WindowEvent::PointerMoved {
                        position: state.pointer_position,
                    });
                }
            }
            wl_pointer::Event::Leave { surface, .. } => {
                if state.popup.as_ref() == Some(&surface) {
                    state.input.push(WindowEvent::PointerExited);
                }
                if state.focused_pointer.as_ref() == Some(&surface) {
                    state.focused_pointer = None;
                }
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_position = LogicalPosition::new(surface_x as f32, surface_y as f32);
                if state.popup.is_some() && state.focused_pointer == state.popup {
                    state.input.push(WindowEvent::PointerMoved {
                        position: state.pointer_position,
                    });
                }
            }
            wl_pointer::Event::Button {
                serial,
                button,
                state: WEnum::Value(button_state),
                ..
            } => {
                if button_state == wl_pointer::ButtonState::Pressed
                    && state.focused_pointer == state.parent
                {
                    state.serial = Some((serial, Instant::now()));
                    state.parent_press = (button == 273).then_some((
                        state.pointer_position,
                        slint::platform::PointerEventButton::Right,
                    ));
                } else if button_state == wl_pointer::ButtonState::Released
                    && state.focused_pointer == state.parent
                {
                    state.parent_press = None;
                }
                if state.popup.is_none() {
                    return;
                }
                if state.focused_pointer != state.popup {
                    if button_state == wl_pointer::ButtonState::Pressed {
                        state.dismissed = true;
                    }
                    return;
                }
                let Some(button) = pointer_button(button) else {
                    return;
                };
                if button_state == wl_pointer::ButtonState::Pressed {
                    state.pressed = Some(button);
                    state.input.push(WindowEvent::PointerPressed {
                        position: state.pointer_position,
                        button,
                    });
                } else if state.pressed.take() == Some(button) {

                    state.input.push(WindowEvent::PointerReleased {
                        position: state.pointer_position,
                        button,
                    });
                }
            }
            wl_pointer::Event::Axis {
                axis: WEnum::Value(axis),
                value,
                ..
            } if state.popup.is_some() && state.focused_pointer == state.popup => {
                let (delta_x, delta_y) = match axis {
                    wl_pointer::Axis::HorizontalScroll => (-value as f32, 0.),
                    _ => (0., -value as f32),
                };
                state.input.push(WindowEvent::PointerScrolled {
                    position: state.pointer_position,
                    delta_x,
                    delta_y,
                });
            }
            _ => {}
        }
    }
}
fn key_text(key: u32, alt: bool) -> Option<(String, usize)> {
    let text: slint::SharedString = match key {
        1 | 62 => Key::Escape.into(),
        103 if alt => Key::Escape.into(),
        28 | 96 => Key::Return.into(),
        102 => Key::Home.into(),
        103 | 104 => Key::UpArrow.into(),
        107 => Key::End.into(),
        108 | 109 => Key::DownArrow.into(),
        57 => " ".into(),
        105 => Key::LeftArrow.into(),
        106 => Key::RightArrow.into(),
        15 => Key::Tab.into(),
        _ => return None,
    };
    Some((
        text.to_string(),
        if key == 104 || key == 109 { 8 } else { 1 },
    ))
}

#[derive(Clone, Copy)]
enum KeyPhase {
    Pressed,
    Released,
    Repeated,
}

fn queue_key(input: &mut Vec<WindowEvent>, key: u32, alt: bool, phase: KeyPhase) -> bool {
    let Some((text, count)) = key_text(key, alt) else {
        return false;
    };
    for _ in 0..count {
        let text = text.clone().into();
        input.push(match phase {
            KeyPhase::Pressed => WindowEvent::KeyPressed { text },
            KeyPhase::Released => WindowEvent::KeyReleased { text },
            KeyPhase::Repeated => WindowEvent::KeyPressRepeated { text },
        });
    }
    true
}
impl Dispatch<wl_keyboard::WlKeyboard, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } if size > 0 && size <= 1024 * 1024 => {
                let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);

                if let Ok(Some(keymap)) = unsafe {
                    xkb::Keymap::new_from_fd(
                        &context,
                        fd,
                        size as usize,
                        xkb::KEYMAP_FORMAT_TEXT_V1,
                        xkb::COMPILE_NO_FLAGS,
                    )
                } {
                    state.keymap = Some(xkb::State::new(&keymap));
                    state.alt = false;
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(keymap) = &mut state.keymap {
                    keymap.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                    let alt =
                        keymap.mod_name_is_active(xkb::MOD_NAME_ALT, xkb::STATE_MODS_EFFECTIVE);
                    if alt != state.alt {
                        state.repeat = None;
                    }
                    state.alt = alt;
                }
            }
            wl_keyboard::Event::Enter { surface, .. } => state.focused_keyboard = Some(surface),
            wl_keyboard::Event::Leave { surface, .. } => {
                if state.focused_keyboard.as_ref() == Some(&surface) {
                    state.focused_keyboard = None;
                    state.repeat = None;
                }
            }
            wl_keyboard::Event::Key {
                serial,
                key,
                state: WEnum::Value(key_state),
                ..
            } => {
                if key_state == wl_keyboard::KeyState::Pressed
                    && state.focused_keyboard == state.parent
                {
                    state.serial = Some((serial, Instant::now()));
                }
                if state.popup.is_none() || state.focused_keyboard != state.popup {
                    return;
                }
                let phase = if key_state == wl_keyboard::KeyState::Pressed {
                    KeyPhase::Pressed
                } else {
                    KeyPhase::Released
                };
                if !queue_key(&mut state.input, key, state.alt, phase) {
                    return;
                }
                if key_state == wl_keyboard::KeyState::Pressed {
                    if state.repeat_rate > 0 && key != 1 && key != 62 && !(key == 103 && state.alt)
                    {
                        state.repeat = Some((
                            key,
                            Instant::now()
                                + Duration::from_millis(state.repeat_delay.max(0) as u64),
                        ));
                    }
                } else {
                    if state
                        .repeat
                        .as_ref()
                        .is_some_and(|(repeating, _)| *repeating == key)
                    {
                        state.repeat = None;
                    }
                }
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                state.repeat_rate = rate;
                state.repeat_delay = delay;
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_buffer::WlBuffer, u64> for ProtocolState {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            state.released.push(*id);
        }
    }
}
wayland_client::delegate_noop!(ProtocolState: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(ProtocolState: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(ProtocolState: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(ProtocolState: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(ProtocolState: ignore wl_region::WlRegion);
wayland_client::delegate_noop!(ProtocolState: ignore xdg_positioner::XdgPositioner);
wayland_client::delegate_noop!(ProtocolState: ignore OrgKdeKwinBlurManager);
wayland_client::delegate_noop!(ProtocolState: ignore OrgKdeKwinBlur);

struct Buffer {
    id: u64,
    proxy: wl_buffer::WlBuffer,
    pool: wl_shm_pool::WlShmPool,
    file: File,
    busy: bool,
    width: u32,
    height: u32,
}

fn clip_rounded_pixels(
    pixels: &mut [PremultipliedRgbaColor],
    stride: u32,
    width: f32,
    height: f32,
    radius: f32,
) {
    let radius = radius.max(0.).min(width.min(height) / 2.);
    for (index, pixel) in pixels.iter_mut().enumerate() {
        let x = (index % stride as usize) as f32 + 0.5;
        let y = (index / stride as usize) as f32 + 0.5;
        let qx = (x - width / 2.).abs() - width / 2. + radius;
        let qy = (y - height / 2.).abs() - height / 2. + radius;
        let distance = qx.max(0.).hypot(qy.max(0.)) + qx.max(qy).min(0.) - radius;
        let coverage = (0.5 - distance).clamp(0., 1.);
        if coverage < 1. {
            let channel = |value: u8| (f32::from(value) * coverage).round() as u8;
            pixel.red = channel(pixel.red);
            pixel.green = channel(pixel.green);
            pixel.blue = channel(pixel.blue);
            pixel.alpha = channel(pixel.alpha);
        }
    }
}

fn rounded_region(width: u32, height: u32, radius: f32) -> Vec<(i32, i32, i32, i32)> {
    let radius = f64::from(radius.max(0.)).min(f64::from(width.min(height)) / 2.);
    let mut spans: Vec<(i32, i32, i32, i32)> = Vec::new();
    for y in 0..height {
        let edge = f64::from(y.min(height - 1 - y)) + 0.5;
        let inset = if edge < radius {
            (radius - (radius * radius - (radius - edge).powi(2)).max(0.).sqrt()).ceil() as i32
        } else {
            0
        };
        let extent = width as i32 - 2 * inset;
        if extent <= 0 {
            continue;
        }
        if let Some(last) = spans.last_mut() {
            if last.0 == inset && last.2 == extent && last.1 + last.3 == y as i32 {
                last.3 += 1;
                continue;
            }
        }
        spans.push((inset, y as i32, extent, 1));
    }
    spans
}
struct Popup {
    surface: wl_surface::WlSurface,
    xdg: xdg_surface::XdgSurface,
    popup: xdg_popup::XdgPopup,
    blur: Option<OrgKdeKwinBlur>,
    mapped: bool,
    created: Instant,
}
struct Bridge {
    popup: Option<Popup>,
    buffers: Vec<Buffer>,
    pending: Option<Frame>,
    next_buffer: u64,
    registry: Option<wl_registry::WlRegistry>,
    parent_xdg: Option<xdg_surface::XdgSurface>,
    queue: Option<EventQueue<ProtocolState>>,
    connection: Option<Connection>,
    state: ProtocolState,
    provider: Option<slint::WindowHandle>,
    scale: i32,
    shown: bool,
}

impl Bridge {
    fn from_window(window: &FilesWindow) -> Result<Option<Self>, String> {
        let provider = window.window().window_handle();
        let Ok(display) = provider.display_handle() else {
            return Ok(None);
        };
        let RawDisplayHandle::Wayland(display) = display.as_raw() else {
            return Err("Menus require a Wayland window".into());
        };
        let parent_ptr = window
            .window()
            .with_winit_window(|window| window.xdg_toplevel())
            .flatten();
        let Some(parent_ptr) = parent_ptr else {
            return Ok(None);
        };

        let (backend, id) = unsafe {
            (
                Backend::from_foreign_display(display.display.as_ptr().cast()),
                ObjectId::from_ptr(
                    xdg_toplevel::XdgToplevel::interface(),
                    parent_ptr.as_ptr().cast(),
                )
                .map_err(|e| e.to_string())?,
            )
        };
        let connection = Connection::from_backend(backend);
        let toplevel =
            xdg_toplevel::XdgToplevel::from_id(&connection, id).map_err(|e| e.to_string())?;
        let parent = SctkWindow::from_xdg_toplevel(&toplevel)
            .ok_or("Winit did not expose its managed XDG parent")?;
        use smithay_client_toolkit::shell::WaylandSurface;
        let parent_surface = parent.wl_surface().clone();
        let parent_xdg = parent.xdg_surface().clone();
        let queue = connection.new_event_queue::<ProtocolState>();
        let qh = queue.handle();
        let registry = connection.display().get_registry(&qh, ());
        connection.display().sync(&qh, ());
        let mut bridge = Self {
            popup: None,
            buffers: vec![],
            pending: None,
            next_buffer: 0,
            registry: Some(registry),
            parent_xdg: Some(parent_xdg),
            queue: Some(queue),
            connection: Some(connection),
            state: ProtocolState {
                parent: Some(parent_surface),
                ..Default::default()
            },
            provider: Some(provider),
            scale: 1,
            shown: false,
        };
        bridge.flush()?;
        Ok(Some(bridge))
    }
    fn poll(&mut self) -> Result<(), String> {
        self.queue
            .as_mut()
            .unwrap()
            .dispatch_pending(&mut self.state)
            .map_err(|e| e.to_string())?;
        for id in self.state.released.drain(..) {
            if let Some(buffer) = self.buffers.iter_mut().find(|b| b.id == id) {
                buffer.busy = false;
            }
        }
        if let Some((key, deadline)) = &mut self.state.repeat {
            if Instant::now() >= *deadline && self.state.repeat_rate > 0 {
                queue_key(
                    &mut self.state.input,
                    *key,
                    self.state.alt,
                    KeyPhase::Repeated,
                );
                *deadline = Instant::now()
                    + Duration::from_millis((1000 / self.state.repeat_rate.max(1)) as u64);
            }
        }
        Ok(())
    }
    fn open(&mut self, _: u64, request: MenuRequest, blur: bool) -> Result<(), String> {
        let (serial, timestamp) = self
            .state
            .serial
            .ok_or("No user input serial for popup grab")?;
        if timestamp.elapsed() > Duration::from_secs(5) {
            return Err("Popup input serial expired".into());
        }
        self.close();
        let qh = self.queue.as_ref().unwrap().handle();
        let shell = self.state.shell.as_ref().ok_or("xdg_wm_base unavailable")?;
        let compositor = self
            .state
            .compositor
            .as_ref()
            .ok_or("wl_compositor unavailable")?;
        let seat = self.state.seat.as_ref().ok_or("wl_seat unavailable")?;
        let positioner = shell.create_positioner(&qh, ());
        positioner.set_size(request.width as i32, request.height as i32);
        positioner.set_anchor_rect(request.x, request.y, 1, 1);
        positioner.set_anchor(xdg_positioner::Anchor::TopLeft);
        positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
        positioner.set_constraint_adjustment(
            xdg_positioner::ConstraintAdjustment::SlideX
                | xdg_positioner::ConstraintAdjustment::SlideY
                | xdg_positioner::ConstraintAdjustment::FlipY,
        );
        let surface = compositor.create_surface(&qh, ());
        self.scale = request.scale as i32;
        surface.set_buffer_scale(self.scale);
        let xdg = shell.get_xdg_surface(&surface, &qh, ());
        let popup = xdg.get_popup(self.parent_xdg.as_ref(), &positioner, &qh, ());
        positioner.destroy();
        popup.grab(seat, serial);
        self.state.popup = Some(surface.clone());
        self.state.popup_role = Some(popup.clone());
        self.state.popup_xdg = Some(xdg.clone());
        self.state.request = Some(request);
        self.state.configured = None;
        self.state.dismissed = false;
        self.state.pressed = None;
        self.state.repeat = None;
        self.popup = Some(Popup {
            surface,
            xdg,
            popup,
            blur: None,
            mapped: false,
            created: Instant::now(),
        });
        self.apply_blur(blur);
        self.popup.as_ref().unwrap().surface.commit();
        self.flush()
    }
    fn apply_blur(&mut self, enabled: bool) {
        let Some(popup) = &mut self.popup else {
            return;
        };
        let Some(manager) = &self.state.blur else {
            return;
        };
        if enabled {
            let blur = popup.blur.get_or_insert_with(|| {
                manager.create(&popup.surface, &self.queue.as_ref().unwrap().handle(), ())
            });
            let region = self
                .state
                .compositor
                .as_ref()
                .unwrap()
                .create_region(&self.queue.as_ref().unwrap().handle(), ());
            if let Some(request) = &self.state.request {
                for (x, y, width, height) in
                    rounded_region(request.width, request.height, request.radius)
                {
                    region.add(x, y, width, height);
                }
            }
            blur.set_region(Some(&region));
            blur.commit();
            region.destroy();
        } else if !enabled {
            if let Some(blur) = popup.blur.take() {
                blur.release();
            }
            manager.unset(&popup.surface);
        }
    }
    fn close(&mut self) {
        if let Some(popup) = self.popup.take() {
            if let Some(blur) = popup.blur {
                blur.release();
            }
            popup.popup.destroy();
            popup.xdg.destroy();
            popup.surface.destroy();
        }
        for buffer in self.buffers.drain(..) {
            buffer.proxy.destroy();
            buffer.pool.destroy();
        }
        self.pending = None;
        self.shown = false;
        self.state.popup = None;
        self.state.popup_role = None;
        self.state.popup_xdg = None;
        self.state.request = None;
        self.state.configured = None;
        self.state.dismissed = false;
        self.state.repeat = None;
        self.state.input.clear();
        self.state.pressed = None;
        let _ = self.flush();
    }
    fn present(&mut self, frame: Frame) -> Result<(), String> {
        self.pending = Some(frame);
        self.present_pending()
    }
    fn present_pending(&mut self) -> Result<(), String> {
        let Some(frame) = self.pending.take() else {
            return Ok(());
        };
        let Some(popup) = &mut self.popup else {
            return Ok(());
        };
        self.buffers.retain(|buffer| {
            if !buffer.busy && (buffer.width != frame.width || buffer.height != frame.height) {
                buffer.proxy.destroy();
                buffer.pool.destroy();
                false
            } else {
                true
            }
        });
        let index = self
            .buffers
            .iter()
            .position(|b| !b.busy && b.width == frame.width && b.height == frame.height);
        let index = match index {
            Some(index) => index,
            None if self.buffers.len() < 3 => {
                let shm = self.state.shm.as_ref().ok_or("wl_shm unavailable")?;
                let file = tempfile::tempfile().map_err(|e| e.to_string())?;
                file.set_len(frame.bytes.len() as u64)
                    .map_err(|e| e.to_string())?;
                let qh = self.queue.as_ref().unwrap().handle();
                let pool = shm.create_pool(file.as_fd(), frame.bytes.len() as i32, &qh, ());
                self.next_buffer += 1;
                let proxy = pool.create_buffer(
                    0,
                    frame.width as i32,
                    frame.height as i32,
                    frame.width as i32 * 4,
                    wl_shm::Format::Argb8888,
                    &qh,
                    self.next_buffer,
                );
                self.buffers.push(Buffer {
                    id: self.next_buffer,
                    proxy,
                    pool,
                    file,
                    busy: false,
                    width: frame.width,
                    height: frame.height,
                });
                self.buffers.len() - 1
            }
            None => {
                self.pending = Some(frame);
                return Ok(());
            }
        };
        let buffer = &mut self.buffers[index];
        buffer
            .file
            .write_all_at(&frame.bytes, 0)
            .map_err(|e| e.to_string())?;
        popup.surface.attach(Some(&buffer.proxy), 0, 0);
        popup
            .surface
            .damage_buffer(0, 0, frame.width as i32, frame.height as i32);
        popup.surface.commit();
        buffer.busy = true;
        if !popup.mapped {
            popup.mapped = true;
            self.shown = true;
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<(), String> {
        match self.connection.as_ref().unwrap().flush() {
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
        self.close();

        self.state = ProtocolState::default();
        self.registry = None;
        self.parent_xdg = None;
        self.queue = None;
        self.connection = None;
        self.provider = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_readiness_does_not_idle_before_seat_input_is_bound() {
        assert_eq!(polling_interval(true, true, false, false), TICK,
            "A registry callback alone must not delay binding the input which owns the first press serial");
        assert_eq!(polling_interval(true, true, true, false), IDLE_TICK);
        assert_eq!(polling_interval(true, true, true, true), TICK);
        assert_eq!(polling_interval(false, true, true, false), TICK);
        assert_eq!(polling_interval(true, false, true, false), TICK);
        let mut state = ProtocolState {
            input_binding_generation: 2,
            ..Default::default()
        };
        state.complete_input_binding(1);
        assert!(
            !state.input_ready,
            "A stale seat-capabilities fence cannot complete the new binding"
        );
        state.complete_input_binding(2);
        assert!(state.input_ready);
    }

    #[test]
    fn native_selector_page_navigation_is_bounded_and_keeps_disabled_rows_unselected() {
        let rows = (0..12)
            .map(|index| MenuItem {
                action: index.to_string().into(),
                enabled: index != 5,
                ..Default::default()
            })
            .collect::<Vec<_>>();
        let mut cursor = 2;
        let mut events = Vec::new();
        assert!(queue_key(&mut events, 109, false, KeyPhase::Pressed));
        assert_eq!(events.len(), 8);
        for event in events {
            assert!(
                matches!(event, WindowEvent::KeyPressed { text } if text == slint::SharedString::from(Key::DownArrow))
            );
            cursor = next_selector_index(&rows, cursor, 1);
        }
        assert_eq!(
            cursor, 11,
            "Eight usable rows include skipping the disabled row"
        );
        for _ in 0..8 {
            cursor = next_selector_index(&rows, cursor, 1);
        }
        assert_eq!(cursor, 11, "PageDown cannot wrap to the first option");
        for _ in 0..30 {
            cursor = next_selector_index(&rows, cursor, -1);
        }
        assert_eq!(cursor, 0, "PageUp cannot wrap to the last option");
        assert_eq!(next_selector_index(&rows, -1, -1), 11);
        assert_eq!(next_selector_index(&rows, -1, 1), 0);
        assert_eq!(next_selector_index(&rows, i32::MAX, 1), -1);
    }

    #[test]
    fn native_keyboard_translates_page_keys_and_selector_close_shortcuts() {
        let up = slint::SharedString::from(Key::UpArrow).to_string();
        let down = slint::SharedString::from(Key::DownArrow).to_string();
        let escape = slint::SharedString::from(Key::Escape).to_string();
        assert_eq!(key_text(104, false), Some((up.clone(), 8)));
        assert_eq!(key_text(109, false), Some((down, 8)));
        assert_eq!(key_text(62, false), Some((escape.clone(), 1)));
        assert_eq!(key_text(103, true), Some((escape, 1)));
        assert_eq!(key_text(103, false), Some((up, 1)));
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap =
            xkb::Keymap::new_from_names(&context, "", "", "us", "", None, xkb::COMPILE_NO_FLAGS)
                .unwrap();
        let alt_mask = 1 << keymap.mod_get_index(xkb::MOD_NAME_ALT);
        let mut state = xkb::State::new(&keymap);
        state.update_mask(alt_mask, 0, 0, 0, 0, 0);
        assert!(state.mod_name_is_active(xkb::MOD_NAME_ALT, xkb::STATE_MODS_EFFECTIVE));
        state.update_mask(0, 0, 0, 0, 0, 0);
        assert!(!state.mod_name_is_active(xkb::MOD_NAME_ALT, xkb::STATE_MODS_EFFECTIVE));
        let mut events = Vec::new();
        assert!(queue_key(&mut events, 104, false, KeyPhase::Released));
        assert_eq!(events.len(), 8);
        assert!(events
            .iter()
            .all(|event| matches!(event, WindowEvent::KeyReleased { .. })));
        events.clear();
        assert!(queue_key(&mut events, 62, false, KeyPhase::Pressed));
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], WindowEvent::KeyPressed { text } if text == &slint::SharedString::from(Key::Escape))
        );
        assert!(!queue_key(&mut events, 999, false, KeyPhase::Pressed));
    }

    #[test]
    fn initial_choice_skips_disabled_checks_and_measures_separator_rows() {
        let mut rows = vec![
            OwnedMenuItem {
                action: "0".into(),
                label: "Unavailable".into(),
                shortcut: String::new(),
                enabled: false,
                separator: false,
                destructive: false,
                checked: true,
            },
            OwnedMenuItem {
                action: String::new(),
                label: String::new(),
                shortcut: String::new(),
                enabled: true,
                separator: true,
                destructive: false,
                checked: true,
            },
            OwnedMenuItem {
                action: "2".into(),
                label: "Current".into(),
                shortcut: String::new(),
                enabled: true,
                separator: false,
                destructive: false,
                checked: true,
            },
        ];
        assert_eq!(initial_selection(&rows, 34., 9.), Some((2, 43.)));
        rows[2].checked = false;
        assert_eq!(initial_selection(&rows, 34., 9.), None);
    }

    #[test]
    fn checked_last_row_is_visible_on_first_frame_and_wide_selectors_use_their_full_width() {
        let (commands, receiver) = mpsc::channel();
        let (events_sender, events) = mpsc::channel();
        let (frames_sender, frames) = mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || worker(receiver, events_sender, frames_sender));
        assert!(matches!(
            events.recv_timeout(TIMEOUT).unwrap(),
            WorkerEvent::Ready
        ));
        let mut theme = ThemeSettings::aero_glass();
        theme.appearance.menu_frame.clear();
        theme
            .appearance
            .metrics
            .insert("context_menu_padding".into(), 4.);
        theme
            .appearance
            .metrics
            .insert("menu_row_height".into(), 34.);
        theme
            .appearance
            .palette
            .insert("surface_hover".into(), "#f04060".into());
        let rows = |selected| {
            (0..30)
                .map(|index| OwnedMenuItem {
                    action: index.to_string(),
                    label: format!("Option {index}"),
                    shortcut: String::new(),
                    enabled: true,
                    separator: false,
                    destructive: false,
                    checked: index == selected,
                })
                .collect()
        };
        let request = |selected| MenuRequest {
            items: rows(selected),
            title: "Select option".into(),
            x: 0,
            y: 0,
            width: 420,
            height: 140,
            scale: 1.,
            radius: 4.,
        };
        commands
            .send(Command::Show(21, request(29), Box::new(theme.clone())))
            .unwrap();
        let frame = frames.recv_timeout(TIMEOUT).unwrap();
        assert_eq!(
            (frame.generation, frame.width, frame.height),
            (21, 420, 140)
        );

        let offset = (119 * frame.width as usize + 390) * 4;
        assert_eq!(u32::from_ne_bytes(frame.bytes[offset..offset + 4].try_into().unwrap()), 0xfff04060,
            "The checked last row must be revealed and fill the requested selector width before its first frame");
        commands
            .send(Command::Input(WindowEvent::KeyPressed {
                text: Key::Return.into(),
            }))
            .unwrap();
        assert!(
            matches!(events.recv_timeout(TIMEOUT).unwrap(), WorkerEvent::Activated(21, action) if action == "29")
        );

        commands.send(Command::Hide).unwrap();
        commands
            .send(Command::Show(22, request(5), Box::new(theme)))
            .unwrap();
        while frames.recv_timeout(TIMEOUT).unwrap().generation != 22 {}
        commands
            .send(Command::Input(WindowEvent::KeyPressed {
                text: Key::Tab.into(),
            }))
            .unwrap();
        assert!(matches!(
            events.recv_timeout(TIMEOUT).unwrap(),
            WorkerEvent::Dismissed(22)
        ));
        commands.send(Command::Stop).unwrap();
        thread.join().unwrap();
    }

    #[test]
    fn rounded_blur_mask_excludes_transparent_corners_and_preserves_center() {
        let spans = rounded_region(100, 80, 20.);
        let covered = |x: i32, y: i32| {
            spans
                .iter()
                .any(|&(sx, sy, w, h)| x >= sx && x < sx + w && y >= sy && y < sy + h)
        };
        for (x, y) in [(0, 0), (99, 0), (0, 79), (99, 79)] {
            assert!(!covered(x, y));
        }
        for (x, y) in [(50, 0), (50, 79), (0, 40), (99, 40), (50, 40)] {
            assert!(covered(x, y));
        }
        assert_eq!(rounded_region(100, 80, 0.), vec![(0, 0, 100, 80)]);
        assert!(spans.len() < 80);
    }

    #[test]
    fn software_menu_uses_alpha_and_keyboard_skips_disabled_rows() {
        let (commands, receiver) = mpsc::channel();
        let (events_sender, events) = mpsc::channel();
        let (frames_sender, frames) = mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || worker(receiver, events_sender, frames_sender));
        match events.recv_timeout(TIMEOUT).unwrap() {
            WorkerEvent::Ready => {}
            WorkerEvent::Failed(error) => panic!("{error}"),
            _ => panic!("Unexpected menu renderer response"),
        }
        let mut theme = ThemeSettings::aero_glass();
        theme.blur_radius = 12;
        theme.appearance.menu_frame.clear();
        let row = |action: &str, enabled, separator| OwnedMenuItem {
            action: action.into(),
            label: action.into(),
            shortcut: String::new(),
            enabled,
            separator,
            destructive: false,
            checked: false,
        };
        commands
            .send(Command::Show(
                7,
                MenuRequest {
                    items: vec![
                        row("disabled", false, false),
                        row("separator", true, true),
                        row("chosen", true, false),
                    ],
                    title: "File actions".into(),
                    x: 0,
                    y: 0,
                    width: 300,
                    height: 160,
                    scale: 1.,
                    radius: 4.,
                },
                Box::new(theme),
            ))
            .unwrap();
        let frame = frames.recv_timeout(TIMEOUT).unwrap();
        assert_eq!((frame.generation, frame.width, frame.height), (7, 300, 160));
        assert_eq!(frame.bytes.len(), 300 * 160 * 4);
        assert!(
            frame.bytes.as_chunks::<4>().0.iter().any(|bytes| {
                let alpha = u32::from_ne_bytes(*bytes) >> 24;
                alpha > 0 && alpha < 255
            }),
            "The native menu keeps translucent material in its own buffer"
        );
        commands
            .send(Command::Input(WindowEvent::KeyPressed {
                text: Key::DownArrow.into(),
            }))
            .unwrap();
        commands
            .send(Command::Input(WindowEvent::KeyPressed {
                text: Key::Return.into(),
            }))
            .unwrap();
        match events.recv_timeout(TIMEOUT).unwrap() {
            WorkerEvent::Activated(7, action) => assert_eq!(action, "chosen"),
            WorkerEvent::Failed(error) => panic!("{error}"),
            _ => panic!("Keyboard activated a disabled row or separator"),
        }
        commands
            .send(Command::Input(WindowEvent::KeyPressed {
                text: Key::Escape.into(),
            }))
            .unwrap();
        assert!(matches!(
            events.recv_timeout(TIMEOUT).unwrap(),
            WorkerEvent::Dismissed(7)
        ));
        commands.send(Command::Stop).unwrap();
        thread.join().unwrap();
    }

    #[test]
    fn native_row_highlight_is_clipped_to_theme_corners_at_every_scale() {
        let (commands, receiver) = mpsc::channel();
        let (events_sender, events) = mpsc::channel();
        let (frames_sender, frames) = mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || worker(receiver, events_sender, frames_sender));
        assert!(matches!(
            events.recv_timeout(TIMEOUT).unwrap(),
            WorkerEvent::Ready
        ));
        let mut theme = ThemeSettings::aero_glass();
        theme.appearance.menu_frame.clear();
        theme
            .appearance
            .metrics
            .insert("context_menu_padding".into(), 0.);
        theme
            .appearance
            .metrics
            .insert("context_menu_radius".into(), 12.);
        theme
            .appearance
            .palette
            .insert("surface_hover".into(), "#f04060".into());
        let pixel = |frame: &Frame, x: u32, y: u32| {
            let offset = ((y * frame.width + x) * 4) as usize;
            u32::from_ne_bytes(frame.bytes[offset..offset + 4].try_into().unwrap())
        };
        for (index, scale) in [1., 1.5, 2.].into_iter().enumerate() {
            let generation = index as u64 + 100;
            commands
                .send(Command::Show(
                    generation,
                    MenuRequest {
                        items: vec![OwnedMenuItem {
                            action: "create".into(),
                            label: "Create".into(),
                            shortcut: String::new(),
                            enabled: true,
                            separator: false,
                            destructive: false,
                            checked: false,
                        }],
                        title: "Folder actions".into(),
                        x: 0,
                        y: 0,
                        width: 300,
                        height: 34,
                        scale,
                        radius: 12.,
                    },
                    Box::new(theme.clone()),
                ))
                .unwrap();
            loop {
                if frames.recv_timeout(TIMEOUT).unwrap().generation == generation {
                    break;
                }
            }
            commands
                .send(Command::Input(WindowEvent::KeyPressed {
                    text: Key::DownArrow.into(),
                }))
                .unwrap();
            let deadline = Instant::now() + TIMEOUT;
            let selected = loop {
                let frame = frames
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap();
                if frame.generation == generation
                    && pixel(&frame, frame.width / 2, (3. * scale) as u32) >> 24 == 255
                {
                    break frame;
                }
            };
            for (x, y) in [
                (0, 0),
                (selected.width - 1, 0),
                (0, selected.height - 1),
                (selected.width - 1, selected.height - 1),
            ] {
                assert_eq!(
                    pixel(&selected, x, y),
                    0,
                    "Highlighted rows cannot leak outside the rounded menu at scale {scale}"
                );
            }
            assert!(
                selected.bytes.as_chunks::<4>().0.iter().any(|bytes| {
                    let alpha = u32::from_ne_bytes(*bytes) >> 24;
                    alpha > 0 && alpha < 255
                }),
                "The final corner mask must retain antialiasing"
            );
        }
        commands.send(Command::Stop).unwrap();
        thread.join().unwrap();
    }
}
