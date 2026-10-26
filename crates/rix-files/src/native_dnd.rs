use crate::{FilesWindow, Theme};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use slint::{ComponentHandle, LogicalPosition, Timer, TimerMode};
use std::{
    cell::RefCell,
    collections::HashMap,
    fs::File,
    io::{self, Write},
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    path::PathBuf,
    rc::Rc,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use wayland_client::{
    backend::{Backend, ObjectId, WaylandError},
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_data_device, wl_data_device_manager,
        wl_data_offer, wl_data_source, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
};
use xkbcommon::xkb;

const TICK: Duration = Duration::from_millis(16);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_RETRY: Duration = Duration::from_secs(10);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(5);

const SOURCE_RECLAIM: Duration = Duration::from_secs(15);
const URI_LIST: &str = "text/uri-list";
const GNOME_FILES: &str = "x-special/gnome-copied-files";
const INVALID_TARGET: i32 = -2147483647;
type Actions = wl_data_device_manager::DndAction;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Copy,
    Move,
}

#[derive(Debug)]
pub enum Event {
    Hover(Option<LogicalPosition>),
    SourceStarted(LogicalPosition),
    SourceEnded,

    Offered {
        id: u64,
        position: LogicalPosition,
    },
    Dropped {
        id: u64,
        paths: Vec<PathBuf>,
        destination: PathBuf,
        action: Action,
    },
    Error(String),
}

struct TransferResult {
    id: u64,
    destination: PathBuf,
    action: Action,
    result: Result<Vec<PathBuf>, String>,
}

pub struct NativeDnd {
    timer: Timer,
    window: slint::Weak<FilesWindow>,
    bridge: Option<Bridge>,
    started: Instant,

    failed_until: Option<Instant>,
    on_event: Rc<dyn Fn(Event)>,
    results: mpsc::Receiver<TransferResult>,
    sender: mpsc::Sender<TransferResult>,
}

pub fn install(window: &FilesWindow, on_event: impl Fn(Event) + 'static) -> Rc<RefCell<NativeDnd>> {
    let (sender, results) = mpsc::channel();
    let dnd = Rc::new(RefCell::new(NativeDnd {
        timer: Timer::default(),
        window: window.as_weak(),
        bridge: None,
        started: Instant::now(),
        failed_until: None,
        on_event: Rc::new(on_event),
        results,
        sender,
    }));
    let weak = Rc::downgrade(&dnd);
    dnd.borrow()
        .timer
        .start(TimerMode::Repeated, TICK, move || {
            let Some(dnd) = weak.upgrade() else {
                return;
            };
            let (events, callback) = {
                let mut dnd = dnd.borrow_mut();
                (dnd.tick(), dnd.on_event.clone())
            };
            for event in events {
                callback(event);
            }
        });
    dnd
}

impl NativeDnd {

    pub fn start(&mut self, paths: &[PathBuf], icon: &slint::Image) -> Result<(), String> {
        let window = self.window.upgrade().ok_or("File window was closed")?;
        let bridge = self
            .bridge
            .as_mut()
            .ok_or("Native drag-and-drop is not ready")?;
        bridge.poll()?;
        if let Some(source) = &bridge.state.source {
            if source.performed.is_none() {
                return Err("A file drag is already active".into());
            }

            let proxy = &source.proxy;
            proxy.destroy();
            bridge.state.source = None;
        }
        let (serial, _) = bridge
            .state
            .left_press
            .ok_or("Drag must start from a left-button press")?;
        let position = bridge.state.position;
        let manager = bridge
            .state
            .manager
            .as_ref()
            .ok_or("The compositor does not support file drag-and-drop")?;
        let device = bridge
            .state
            .device
            .as_ref()
            .ok_or("No Wayland data device is available")?;
        let (_, uris) = rix_files::clipboard::encode(paths, false)?;
        let qh = bridge.queue.as_ref().unwrap().handle();
        let source = manager.create_data_source(&qh, ());
        source.offer(URI_LIST.into());
        source.offer(GNOME_FILES.into());
        if source.version() >= 3 {
            source.set_actions(Actions::Copy | Actions::Move);
        }
        bridge.icon = create_icon(&bridge.state, &qh, icon, paths, &window).ok();
        device.start_drag(
            Some(&source),
            bridge.state.parent.as_ref().unwrap(),
            bridge.icon.as_ref().map(|icon| &icon.surface),
            serial,
        );
        bridge.state.source = Some(Source {
            proxy: source,
            paths: paths.to_vec(),
            uris,
            action: Actions::None,
            writers: 0,
            performed: None,
        });
        bridge.state.events.push(Event::SourceStarted(position));
        bridge.flush()?;
        Ok(())
    }

    pub fn accept(&mut self, id: u64, destination: Option<PathBuf>) {
        let Some(bridge) = &mut self.bridge else {
            return;
        };
        let Some(pending) = bridge.state.pending.get_mut(&id) else {
            return;
        };
        let Some(destination) = destination else {
            self.complete(id, false);
            return;
        };
        if pending.reading {
            return;
        }
        pending.reading = true;
        let (reader, writer) = match pipe() {
            Ok(pair) => pair,
            Err(error) => {
                bridge.state.events.push(Event::Error(error.to_string()));
                self.complete(id, false);
                return;
            }
        };
        pending
            .offer
            .proxy
            .receive(pending.mime.clone(), writer.as_fd());
        drop(writer);
        let action = pending.action;
        let gnome = pending.mime == GNOME_FILES;
        let sender = self.sender.clone();
        if let Err(error) = bridge.flush() {
            bridge.state.events.push(Event::Error(error));
            self.complete(id, false);
            return;
        }
        if let Err(error) = thread::Builder::new()
            .name("rix-file-drop".into())
            .spawn(move || {
                let result =
                    rix_files::clipboard::read_bounded(File::from(reader), TRANSFER_TIMEOUT)
                        .and_then(|bytes| {
                            rix_files::clipboard::decode(&bytes, gnome).map(|(paths, _)| paths)
                        });
                let _ = sender.send(TransferResult {
                    id,
                    destination,
                    action,
                    result,
                });
            })
        {
            bridge.state.events.push(Event::Error(error.to_string()));
            self.complete(id, false);
        }
    }

    pub fn complete(&mut self, id: u64, success: bool) {
        if let Some(bridge) = &mut self.bridge {
            if let Some(drop) = bridge.state.pending.remove(&id) {
                if success && drop.offer.proxy.version() >= 3 {
                    drop.offer.proxy.finish();
                }
                drop.offer.proxy.destroy();
            }
            let _ = bridge.flush();
        }
    }

    fn tick(&mut self) -> Vec<Event> {
        if let Some(deadline) = self.failed_until {
            if Instant::now() < deadline {
                return vec![];
            }
            self.failed_until = None;
        }
        let Some(window) = self.window.upgrade() else {
            self.timer.stop();
            self.bridge = None;
            return vec![];
        };
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
                Ok(None) if self.started.elapsed() < PROBE_TIMEOUT => return vec![],
                result => {

                    self.failed_until = Some(Instant::now() + PROBE_RETRY);
                    self.bridge = None;
                    tracing::debug!(error = ?result.err(), "Native file drag-and-drop is unavailable, retrying");
                    return vec![];
                }
            }
        }
        let bridge = self.bridge.as_mut().unwrap();
        if let Err(error) = bridge.poll() {
            self.failed_until = Some(Instant::now() + PROBE_RETRY);
            self.bridge = None;
            tracing::debug!(%error, "Native file drag-and-drop connection failed, retrying");
            return vec![Event::SourceEnded, Event::Hover(None)];
        }
        if bridge.state.source.is_none() {
            bridge.icon = None;
        }
        if let Some(source) = &bridge.state.source {
            if source_reclaim_due(source.performed, Instant::now()) {
                bridge.state.end_source();
            }
        }
        for result in self.results.try_iter() {

            if !bridge.state.pending.contains_key(&result.id) {
                continue;
            }
            match result.result {
                Ok(paths) => bridge.state.events.push(Event::Dropped {
                    id: result.id,
                    paths,
                    destination: result.destination,
                    action: result.action,
                }),
                Err(error) => {
                    if let Some(drop) = bridge.state.pending.remove(&result.id) {
                        drop.offer.proxy.destroy();
                    }
                    bridge.state.events.push(Event::Error(error));
                }
            }
        }

        let expired: Vec<_> = bridge
            .state
            .pending
            .iter()
            .filter(|(_, drop)| !drop.reading && drop.created.elapsed() > TRANSFER_TIMEOUT)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(drop) = bridge.state.pending.remove(&id) {
                drop.offer.proxy.destroy();
            }
        }
        std::mem::take(&mut bridge.state.events)
    }
}

impl Drop for NativeDnd {
    fn drop(&mut self) {
        self.timer.stop();
        self.bridge = None;
    }
}

fn source_reclaim_due(performed: Option<Instant>, now: Instant) -> bool {
    performed.is_some_and(|dropped| now.duration_since(dropped) >= SOURCE_RECLAIM)
}

struct Offer {
    proxy: wl_data_offer::WlDataOffer,
    types: Vec<String>,
    actions: Actions,
    action: Actions,
}
struct Incoming {
    offer: Offer,
    serial: u32,
    position: LogicalPosition,
    accepted: bool,
}
struct Pending {
    offer: Offer,
    mime: String,
    action: Action,
    created: Instant,
    reading: bool,
}
struct Source {
    proxy: wl_data_source::WlDataSource,
    paths: Vec<PathBuf>,
    uris: Vec<u8>,
    action: Actions,
    writers: usize,

    performed: Option<Instant>,
}
struct ProtocolState {
    parent: Option<wl_surface::WlSurface>,
    registry_ready: bool,
    manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    device: Option<wl_data_device::WlDataDevice>,
    focused: bool,
    position: LogicalPosition,
    left_press: Option<(u32, LogicalPosition)>,
    keymap: Option<xkb::State>,
    ctrl: bool,
    shift: bool,
    offers: HashMap<ObjectId, Offer>,
    incoming: Option<Incoming>,
    pending: HashMap<u64, Pending>,
    source: Option<Source>,
    next_drop: u64,
    events: Vec<Event>,
    hit_test: Rc<dyn Fn(LogicalPosition) -> bool>,
}

fn selected_action(action: Actions, version: u32) -> Option<Action> {
    if version < 3 || action == Actions::Copy {
        Some(Action::Copy)
    } else if action == Actions::Move {
        Some(Action::Move)
    } else {
        None
    }
}
fn preferred_action(ctrl: bool, shift: bool, supported: Actions) -> Actions {
    if ctrl {
        if supported.contains(Actions::Copy) {
            Actions::Copy
        } else {
            Actions::None
        }
    } else if shift {
        if supported.contains(Actions::Move) {
            Actions::Move
        } else {
            Actions::None
        }
    } else if supported.contains(Actions::Copy) {
        Actions::Copy
    } else if supported.contains(Actions::Move) {
        Actions::Move
    } else {
        Actions::None
    }
}

impl ProtocolState {
    fn initialize_device(&mut self, qh: &QueueHandle<Self>) {
        if self.device.is_none() {
            if let (Some(manager), Some(seat)) = (&self.manager, &self.seat) {
                self.device = Some(manager.get_data_device(seat, qh, ()));
            }
        }
    }
    fn negotiate(&mut self) {
        let Some(incoming) = &mut self.incoming else {
            return;
        };
        let mime = if incoming.offer.types.iter().any(|t| t == URI_LIST) {
            Some(URI_LIST)
        } else if incoming.offer.types.iter().any(|t| t == GNOME_FILES) {
            Some(GNOME_FILES)
        } else {
            None
        };
        let preferred = preferred_action(self.ctrl, self.shift, incoming.offer.actions);
        incoming.accepted = mime.is_some()
            && (self.hit_test)(incoming.position)
            && (incoming.offer.proxy.version() < 3 || preferred != Actions::None);
        incoming.offer.proxy.accept(
            incoming.serial,
            incoming.accepted.then(|| mime.unwrap().into()),
        );
        if incoming.offer.proxy.version() >= 3 {
            let supported = if self.ctrl {
                Actions::Copy
            } else if self.shift {
                Actions::Move
            } else {
                Actions::Copy | Actions::Move
            };
            incoming.offer.proxy.set_actions(
                if incoming.accepted {
                    supported
                } else {
                    Actions::None
                },
                if incoming.accepted {
                    preferred
                } else {
                    Actions::None
                },
            );
        }
    }
    fn end_source(&mut self) {
        if let Some(source) = self.source.take() {
            source.proxy.destroy();
            self.events.push(Event::SourceEnded);
        }
        self.left_press = None;
    }
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
                "wl_data_device_manager" if state.manager.is_none() => {
                    state.manager = Some(registry.bind(name, version.min(3), qh, ()))
                }
                "wl_compositor" if state.compositor.is_none() => {
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wl_shm" if state.shm.is_none() => state.shm = Some(registry.bind(name, 1, qh, ())),
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(7), qh, ()))
                }
                _ => {}
            }
            state.initialize_device(qh);
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
impl Dispatch<wl_seat::WlSeat, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            if capabilities.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
            if capabilities.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
            if !capabilities.contains(wl_seat::Capability::Pointer) {
                if let Some(pointer) = state.pointer.take() {
                    if pointer.version() >= 3 {
                        pointer.release();
                    }
                }
                state.left_press = None;
            }
            if !capabilities.contains(wl_seat::Capability::Keyboard) {
                if let Some(keyboard) = state.keyboard.take() {
                    if keyboard.version() >= 3 {
                        keyboard.release();
                    }
                }
                state.keymap = None;
                state.ctrl = false;
                state.shift = false;
            }
        }
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
                state.focused = state.parent.as_ref() == Some(&surface);
                state.position = LogicalPosition::new(surface_x as f32, surface_y as f32);
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => state.position = LogicalPosition::new(surface_x as f32, surface_y as f32),
            wl_pointer::Event::Leave { surface, .. } if state.parent.as_ref() == Some(&surface) => {
                state.focused = false
            }
            wl_pointer::Event::Button {
                serial,
                button: 272,
                state: WEnum::Value(button),
                ..
            } => {
                if button == wl_pointer::ButtonState::Pressed && state.focused {
                    state.left_press = Some((serial, state.position));
                } else if button == wl_pointer::ButtonState::Released {
                    state.left_press = None;
                }
            }
            _ => {}
        }
    }
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
                    state.ctrl =
                        keymap.mod_name_is_active(xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE);
                    state.shift =
                        keymap.mod_name_is_active(xkb::MOD_NAME_SHIFT, xkb::STATE_MODS_EFFECTIVE);
                    state.negotiate();
                }
            }
            wl_keyboard::Event::Key {
                key: 1,
                state: WEnum::Value(wl_keyboard::KeyState::Pressed),
                ..
            } => state.end_source(),
            _ => {}
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for ProtocolState {
    wayland_client::event_created_child!(ProtocolState, wl_data_device::WlDataDevice, [0 => (wl_data_offer::WlDataOffer, ())]);
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                if state.offers.len() >= 32 {
                    id.destroy();
                    return;
                }
                state.offers.insert(
                    id.id(),
                    Offer {
                        proxy: id,
                        types: vec![],
                        actions: Actions::None,
                        action: Actions::None,
                    },
                );
            }
            wl_data_device::Event::Selection { id: Some(id) } => {

                if let Some(offer) = state.offers.remove(&id.id()) {
                    offer.proxy.destroy();
                }
            }
            wl_data_device::Event::Enter {
                serial,
                surface,
                x,
                y,
                id,
            } => {
                if let Some(old) = state.incoming.take() {
                    old.offer.proxy.destroy();
                }
                if state.parent.as_ref() != Some(&surface) {
                    if let Some(id) = id {
                        if let Some(offer) = state.offers.remove(&id.id()) {
                            offer.proxy.destroy();
                        }
                    }
                    return;
                }
                let Some(offer) = id.and_then(|id| state.offers.remove(&id.id())) else {
                    return;
                };
                let position = LogicalPosition::new(x as f32, y as f32);
                state.incoming = Some(Incoming {
                    offer,
                    serial,
                    position,
                    accepted: false,
                });
                state.negotiate();
                state.events.push(Event::Hover(Some(position)));
            }
            wl_data_device::Event::Motion { x, y, .. } => {
                if let Some(incoming) = &mut state.incoming {
                    let position = LogicalPosition::new(x as f32, y as f32);
                    incoming.position = position;
                    state.events.push(Event::Hover(Some(position)));
                }
                state.negotiate();
            }
            wl_data_device::Event::Leave => {
                if let Some(incoming) = state.incoming.take() {
                    incoming.offer.proxy.destroy();
                }
                state.events.push(Event::Hover(None));
            }
            wl_data_device::Event::Drop => {
                let Some(incoming) = state.incoming.take() else {
                    return;
                };
                state.events.push(Event::Hover(None));
                let action = selected_action(incoming.offer.action, incoming.offer.proxy.version());
                if !incoming.accepted || action.is_none() || state.pending.len() >= 4 {
                    incoming.offer.proxy.destroy();
                    return;
                }
                let mime = if incoming.offer.types.iter().any(|t| t == URI_LIST) {
                    URI_LIST
                } else {
                    GNOME_FILES
                };
                state.next_drop = state.next_drop.wrapping_add(1);
                let id = state.next_drop;
                let action = action.unwrap();
                state.pending.insert(
                    id,
                    Pending {
                        offer: incoming.offer,
                        mime: mime.into(),
                        action,
                        created: Instant::now(),
                        reading: false,
                    },
                );
                state.events.push(Event::Offered {
                    id,
                    position: incoming.position,
                });
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_data_offer::WlDataOffer, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        proxy: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let offer = if let Some(incoming) = &mut state.incoming {
            if incoming.offer.proxy == *proxy {
                Some(&mut incoming.offer)
            } else {
                state.offers.get_mut(&proxy.id())
            }
        } else {
            state.offers.get_mut(&proxy.id())
        };
        let Some(offer) = offer else {
            return;
        };
        match event {
            wl_data_offer::Event::Offer { mime_type }
                if mime_type.len() <= 1024 && offer.types.len() < 128 =>
            {
                offer.types.push(mime_type)
            }
            wl_data_offer::Event::SourceActions {
                source_actions: WEnum::Value(actions),
            } => offer.actions = actions,
            wl_data_offer::Event::Action {
                dnd_action: WEnum::Value(action),
            } => offer.action = action,
            _ => {}
        }
        state.negotiate();
    }
}
impl Dispatch<wl_data_source::WlDataSource, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        proxy: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(source) = &mut state.source else {
            return;
        };
        if source.proxy != *proxy {
            return;
        }
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {

                if source.writers >= 16 {
                    return;
                }
                let bytes = if mime_type == URI_LIST {
                    source.uris.clone()
                } else if mime_type == GNOME_FILES {
                    match rix_files::clipboard::encode(
                        &source.paths,
                        source.action == Actions::Move,
                    ) {
                        Ok((bytes, _)) => bytes,
                        Err(_) => return,
                    }
                } else {
                    return;
                };
                source.writers += 1;
                let _ = thread::Builder::new()
                    .name("rix-file-drag".into())
                    .spawn(move || {
                        let _ = write_bounded(File::from(fd), &bytes, TRANSFER_TIMEOUT);
                    });
            }
            wl_data_source::Event::Action {
                dnd_action: WEnum::Value(action),
            } => source.action = action,
            wl_data_source::Event::DndDropPerformed => {
                source.performed = Some(Instant::now());
                state.events.push(Event::SourceEnded);
            }
            wl_data_source::Event::DndFinished | wl_data_source::Event::Cancelled => {
                state.end_source()
            }
            _ => {}
        }
    }
}
wayland_client::delegate_noop!(ProtocolState: ignore wl_data_device_manager::WlDataDeviceManager);
wayland_client::delegate_noop!(ProtocolState: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(ProtocolState: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(ProtocolState: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(ProtocolState: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(ProtocolState: ignore wl_buffer::WlBuffer);

struct Icon {
    surface: wl_surface::WlSurface,
    buffer: wl_buffer::WlBuffer,
    _file: File,
}
impl Drop for Icon {
    fn drop(&mut self) {
        self.surface.destroy();
        self.buffer.destroy();
    }
}

fn create_icon(
    state: &ProtocolState,
    qh: &QueueHandle<ProtocolState>,
    image: &slint::Image,
    paths: &[PathBuf],
    window: &FilesWindow,
) -> Result<Icon, String> {
    let compositor = state.compositor.as_ref().ok_or("No compositor")?;
    let shm = state.shm.as_ref().ok_or("No SHM")?;
    let width = 272usize;
    let height = 76usize;
    let theme = window.global::<Theme>();
    let color = |value: slint::Color| {
        format!(
            "#{:02x}{:02x}{:02x}",
            value.red(),
            value.green(),
            value.blue()
        )
    };
    let escape = |value: &str| {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('\"', "&quot;")
    };
    let mut label = paths
        .first()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if label.chars().count() > 28 {
        label = label.chars().take(27).collect::<String>() + "…";
    }
    let subtitle = if paths.len() > 1 {
        crate::translated(
            "{count} selected files",
            &[("count", paths.len().to_string())],
        )
    } else {
        crate::i18n::tr("File").to_string()
    };
    let radius = theme.get_radius_sm().clamp(3., 12.);

    let surface = theme.get_control_surface();
    let backdrop = theme.get_window_background();
    let mix = |foreground: u8, background: u8| {
        let alpha = u32::from(surface.alpha());
        ((u32::from(foreground) * alpha + u32::from(background) * (255 - alpha) + 127) / 255) as u8
    };
    let card_background = slint::Color::from_rgb_u8(
        mix(surface.red(), backdrop.red()),
        mix(surface.green(), backdrop.green()),
        mix(surface.blue(), backdrop.blue()),
    );
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="272" height="76">
        <rect x="7" y="9" width="258" height="62" rx="{radius}" fill="#000" opacity="0.20"/>
        <rect x="5" y="4" width="258" height="62" rx="{radius}" fill="{}" stroke="{}" stroke-opacity="0.65"/>
        <text x="64" y="29" font-family="{}" font-size="13" font-weight="600" fill="{}">{}</text>
        <text x="64" y="49" font-family="{}" font-size="11" fill="{}">{}</text>
        </svg>"##,
        color(card_background),
        color(theme.get_accent()),
        escape(theme.get_font_family().as_str()),
        color(theme.get_text_primary()),
        escape(&label),
        escape(theme.get_font_family().as_str()),
        color(theme.get_text_secondary()),
        escape(&subtitle)
    );
    let frame =
        slint::Image::load_from_svg_data(svg.as_bytes()).map_err(|error| error.to_string())?;
    let mut pixels = frame
        .to_rgba8_premultiplied()
        .ok_or("Could not render drag preview")?;
    if let Some(icon) = image.to_rgba8_premultiplied() {
        let iw = icon.width() as usize;
        let ih = icon.height() as usize;
        if iw > 0 && ih > 0 {
            let side = 40usize;
            let ow = if iw >= ih {
                side
            } else {
                (side * iw / ih).max(1)
            };
            let oh = if ih >= iw {
                side
            } else {
                (side * ih / iw).max(1)
            };
            for y in 0..oh {
                for x in 0..ow {
                    let source = icon.as_slice()[(y * ih / oh) * iw + x * iw / ow];
                    let target = &mut pixels.make_mut_slice()
                        [(y + 15 + (side - oh) / 2) * width + x + 15 + (side - ow) / 2];
                    let mix = |s: u8, b: u8| {
                        (u16::from(s) + u16::from(b) * (255 - u16::from(source.a)) / 255).min(255)
                            as u8
                    };
                    *target = slint::Rgba8Pixel {
                        r: mix(source.r, target.r),
                        g: mix(source.g, target.g),
                        b: mix(source.b, target.b),
                        a: mix(source.a, target.a),
                    };
                }
            }
        }
    }
    let bytes: Vec<u8> = pixels
        .as_slice()
        .iter()
        .flat_map(|pixel| {
            ((u32::from(pixel.a) << 24)
                | (u32::from(pixel.r) << 16)
                | (u32::from(pixel.g) << 8)
                | u32::from(pixel.b))
            .to_ne_bytes()
        })
        .collect();
    let mut file = tempfile::tempfile().map_err(|e| e.to_string())?;
    file.write_all(&bytes).map_err(|e| e.to_string())?;
    let pool = shm.create_pool(file.as_fd(), bytes.len() as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        width as i32 * 4,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );
    pool.destroy();
    let surface = compositor.create_surface(qh, ());
    surface.attach(Some(&buffer), 10, 10);
    surface.damage(0, 0, width as i32, height as i32);
    surface.commit();
    Ok(Icon {
        surface,
        buffer,
        _file: file,
    })
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [-1; 2];

    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    })
}

fn write_bounded(mut file: File, bytes: &[u8], timeout: Duration) -> io::Result<()> {
    let fd = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let deadline = Instant::now() + timeout;
    let mut remaining = bytes;
    while !remaining.is_empty() {
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let result = unsafe {
            libc::poll(
                &mut descriptor,
                1,
                timeout.as_millis().clamp(1, i32::MAX as u128) as i32,
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if result == 0 {
            continue;
        }
        if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        match file.write(remaining) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(length) => remaining = &remaining[length..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

struct Bridge {
    icon: Option<Icon>,
    registry: Option<wl_registry::WlRegistry>,
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
            return Err("Window does not use native Wayland".into());
        };

        let (backend, surface_id) = unsafe {
            (
                Backend::from_foreign_display(display.display.as_ptr().cast()),
                ObjectId::from_ptr(
                    wl_surface::WlSurface::interface(),
                    surface.surface.as_ptr().cast(),
                )
                .map_err(|e| e.to_string())?,
            )
        };
        let connection = Connection::from_backend(backend);
        let parent =
            wl_surface::WlSurface::from_id(&connection, surface_id).map_err(|e| e.to_string())?;
        let queue = connection.new_event_queue::<ProtocolState>();
        let registry = connection.display().get_registry(&queue.handle(), ());
        connection.display().sync(&queue.handle(), ());
        let weak = window.as_weak();
        let state = ProtocolState {
            parent: Some(parent),
            registry_ready: false,
            manager: None,
            compositor: None,
            shm: None,
            seat: None,
            pointer: None,
            keyboard: None,
            device: None,
            focused: false,
            position: LogicalPosition::default(),
            left_press: None,
            keymap: None,
            ctrl: false,
            shift: false,
            offers: HashMap::new(),
            incoming: None,
            pending: HashMap::new(),
            source: None,
            next_drop: 0,
            events: vec![],
            hit_test: Rc::new(move |point| {
                weak.upgrade().is_some_and(|window| {
                    !window.get_busy()
                        && !window.get_operation_active()
                        && !window.get_trash_view()
                        && window.invoke_drop_target(point.x, point.y) != INVALID_TARGET
                })
            }),
        };
        let bridge = Self {
            icon: None,
            registry: Some(registry),
            queue: Some(queue),
            connection: Some(connection),
            state,
            provider: Some(provider),
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
        self.flush()
    }
    fn flush(&self) -> Result<(), String> {
        match self.connection.as_ref().unwrap().flush() {
            Ok(()) => Ok(()),
            Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
}
impl Drop for Bridge {
    fn drop(&mut self) {
        self.icon = None;
        self.state.end_source();
        if let Some(incoming) = self.state.incoming.take() {
            incoming.offer.proxy.destroy();
        }
        for (_, drop) in self.state.pending.drain() {
            drop.offer.proxy.destroy();
        }
        for (_, offer) in self.state.offers.drain() {
            offer.proxy.destroy();
        }
        if let Some(device) = self.state.device.take() {
            if device.version() >= 2 {
                device.release();
            }
        }
        if let Some(pointer) = self.state.pointer.take() {
            if pointer.version() >= 3 {
                pointer.release();
            }
        }
        if let Some(keyboard) = self.state.keyboard.take() {
            if keyboard.version() >= 3 {
                keyboard.release();
            }
        }
        if let Some(seat) = self.state.seat.take() {
            if seat.version() >= 5 {
                seat.release();
            }
        }
        if let Some(connection) = &self.connection {
            let _ = connection.flush();
        }
        self.state.parent = None;
        self.state.manager = None;
        self.state.compositor = None;
        self.state.shm = None;
        self.registry = None;
        self.queue = None;
        self.connection = None;
        self.provider = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn stalled_performed_source_is_reclaimed_after_grace() {
        let ancient = Instant::now().checked_sub(2 * SOURCE_RECLAIM).unwrap();
        assert!(!source_reclaim_due(None, Instant::now()));

        let fresh = Instant::now()
            .checked_sub(Duration::from_millis(50))
            .unwrap();
        assert!(!source_reclaim_due(Some(fresh), Instant::now()));

        assert!(source_reclaim_due(Some(ancient), Instant::now()));
    }

    #[test]
    fn modifiers_never_turn_a_missing_action_into_a_destructive_move() {
        let both = Actions::Copy | Actions::Move;
        assert_eq!(preferred_action(false, false, both), Actions::Copy);
        assert_eq!(preferred_action(true, false, both), Actions::Copy);
        assert_eq!(preferred_action(false, true, both), Actions::Move);
        assert_eq!(preferred_action(true, true, both), Actions::Copy);
        assert_eq!(preferred_action(false, true, Actions::Copy), Actions::None);
        assert_eq!(preferred_action(true, false, Actions::Move), Actions::None);
        assert_eq!(selected_action(Actions::None, 3), None);
        assert_eq!(selected_action(Actions::Ask, 3), None);
        assert_eq!(selected_action(Actions::Move, 3), Some(Action::Move));
        assert_eq!(selected_action(Actions::None, 2), Some(Action::Copy));
    }
    #[test]
    fn native_source_sends_the_complete_uri_list_and_closes_its_pipe() {
        let paths = vec![PathBuf::from("/tmp/file with # %\n.txt")];
        let (_, bytes) = rix_files::clipboard::encode(&paths, false).unwrap();
        let (read, write) = pipe().unwrap();
        write_bounded(File::from(write), &bytes, Duration::from_secs(1)).unwrap();
        let mut received = vec![];
        File::from(read).read_to_end(&mut received).unwrap();
        assert_eq!(
            rix_files::clipboard::decode(&received, false).unwrap().0,
            paths
        );
    }
    #[test]
    fn native_source_stalled_reader_cannot_hold_a_worker_forever() {
        let (_read, write) = pipe().unwrap();
        let start = Instant::now();
        assert_eq!(
            write_bounded(
                File::from(write),
                &vec![1u8; 1024 * 1024],
                Duration::from_millis(30)
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
