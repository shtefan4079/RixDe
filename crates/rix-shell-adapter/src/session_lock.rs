use slint::{
    platform::{
        self,
        software_renderer::{PremultipliedRgbaColor, RepaintBufferType, SoftwareRenderer},
        EventLoopProxy, Platform, WindowAdapter, WindowEvent,
    },
    LogicalPosition, LogicalSize, PhysicalSize, PlatformError, Window,
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers, RepeatInfo},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shm::{
        slot::{Buffer, SlotPool},
        Shm, ShmHandler,
    },
};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
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
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1},
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};
use wayland_protocols::{
    ext::session_lock::v1::client::{
        ext_session_lock_manager_v1::ExtSessionLockManagerV1,
        ext_session_lock_surface_v1::{self, ExtSessionLockSurfaceV1},
        ext_session_lock_v1::{self, ExtSessionLockV1},
    },
    wp::{
        fractional_scale::v1::client::{
            wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
            wp_fractional_scale_v1::{self, WpFractionalScaleV1},
        },
        viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
    },
};

fn error(e: impl std::fmt::Display) -> PlatformError {
    PlatformError::Other(e.to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lifecycle {
    Pending,
    Locked,
    UnlockRequested,
    Unlocked,
    Finished,
}
impl Lifecycle {
    fn request_unlock(&mut self) -> Result<(), PlatformError> {
        if *self != Self::Locked {
            return Err(error("Cannot unlock before compositor confirmation"));
        }
        *self = Self::UnlockRequested;
        Ok(())
    }
}
type OutputAdded = Rc<dyn Fn() -> Result<(), PlatformError>>;
struct Control {
    lifecycle: Lifecycle,
    output_count: usize,
    output_added: Option<OutputAdded>,
    locked: Option<Rc<dyn Fn()>>,
    layout_changed: Option<Rc<dyn Fn(String, bool)>>,
    focus_changed: Option<Rc<dyn Fn(usize)>>,
    notify_locked: bool,
    layout: Option<(String, bool)>,
    focus: Option<usize>,
}

#[derive(Clone)]
pub struct SessionLockController(Rc<RefCell<Control>>);
impl SessionLockController {
    pub fn output_count(&self) -> usize {
        self.0.borrow().output_count
    }
    pub fn is_locked(&self) -> bool {
        self.0.borrow().lifecycle == Lifecycle::Locked
    }
    pub fn on_output_added(&self, callback: impl Fn() -> Result<(), PlatformError> + 'static) {
        self.0.borrow_mut().output_added = Some(Rc::new(callback));
    }
    pub fn on_locked(&self, callback: impl Fn() + 'static) {
        self.0.borrow_mut().locked = Some(Rc::new(callback));
    }
    pub fn on_layout_changed(&self, callback: impl Fn(String, bool) + 'static) {
        self.0.borrow_mut().layout_changed = Some(Rc::new(callback));
    }
    pub fn on_focus_changed(&self, callback: impl Fn(usize) + 'static) {
        self.0.borrow_mut().focus_changed = Some(Rc::new(callback));
    }
    pub fn unlock(&self) -> Result<(), PlatformError> {
        self.0.borrow_mut().lifecycle.request_unlock()
    }
}

pub fn install_session_lock() -> Result<SessionLockController, PlatformError> {
    let conn = Connection::connect_to_env().map_err(error)?;
    let (globals, mut queue) = registry_queue_init(&conn).map_err(error)?;
    let qh = queue.handle();
    let manager: ExtSessionLockManagerV1 = globals.bind(&qh, 1..=1, ()).map_err(error)?;
    let control = SessionLockController(Rc::new(RefCell::new(Control {
        lifecycle: Lifecycle::Pending,
        output_count: 0,
        output_added: None,
        locked: None,
        layout_changed: None,
        focus_changed: None,
        notify_locked: false,
        layout: None,
        focus: None,
    })));
    let mut state = LockState {
        registry: RegistryState::new(&globals),
        seats: SeatState::new(&globals, &qh),
        outputs: OutputState::new(&globals, &qh),
        shm: Shm::bind(&globals, &qh).map_err(error)?,
        compositor: CompositorState::bind(&globals, &qh).map_err(error)?,
        viewporter: globals.bind(&qh, 1..=1, ()).ok(),
        fractional_scale: globals.bind(&qh, 1..=1, ()).ok(),
        cursor_shape: globals.bind(&qh, 1..=1, ()).ok(),
        cursor_device: None,
        lock: None,
        windows: Vec::new(),
        pending_outputs: VecDeque::new(),
        control: control.clone(),
        keyboard: None,
        pointer: None,
        focused: Weak::new(),
        pressed: Default::default(),
        keymap: None,
        modifiers: Modifiers::default(),
        repeat_info: RepeatInfo::Disable,
        repeating: None,
        repeat_delay: slint::Timer::default(),
        repeat_timer: Rc::new(slint::Timer::default()),
    };
    queue.roundtrip(&mut state).map_err(error)?;

    state.lock = Some(manager.lock(&qh, ()));
    let (wake_read, wake_write) = UnixStream::pair().map_err(error)?;
    wake_read.set_nonblocking(true).map_err(error)?;
    wake_write.set_nonblocking(true).map_err(error)?;
    let (sender, receiver) = mpsc::sync_channel(MESSAGE_QUEUE_CAPACITY);
    platform::set_platform(Box::new(LockPlatform {
        conn,
        queue: RefCell::new(queue),
        state: RefCell::new(state),
        control: control.clone(),
        wake_read: RefCell::new(wake_read),
        receiver,
        proxy: LoopProxy {
            sender,
            wake: Arc::new(Mutex::new(wake_write)),
            mailbox: Arc::new(Mailbox::default()),
            loop_thread: std::thread::current().id(),
        },
    }))
    .map_err(error)?;
    Ok(control)
}

const MESSAGE_QUEUE_CAPACITY: usize = 64;

const QUEUE_BACKPRESSURE_WAIT: std::time::Duration = std::time::Duration::from_millis(5);

enum Message {
    Invoke(Box<dyn FnOnce() + Send>),
    Quit,
}

#[derive(Default)]
struct Mailbox {

    closed: AtomicBool,

    quit: AtomicBool,
}
#[derive(Clone)]
struct LoopProxy {
    sender: mpsc::SyncSender<Message>,
    wake: Arc<Mutex<UnixStream>>,
    mailbox: Arc<Mailbox>,

    loop_thread: std::thread::ThreadId,
}
impl LoopProxy {
    fn send(&self, message: Message) -> Result<(), slint::EventLoopError> {
        self.queue(message)?;

        let _ = crate::wayland::lock_or_recover(&self.wake).write(&[1]);
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

    fn close(&self) {
        self.mailbox.closed.store(true, Ordering::Release);
    }
}
impl EventLoopProxy for LoopProxy {
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
struct LockPlatform {
    conn: Connection,
    queue: RefCell<EventQueue<LockState>>,
    state: RefCell<LockState>,
    control: SessionLockController,
    wake_read: RefCell<UnixStream>,
    receiver: mpsc::Receiver<Message>,
    proxy: LoopProxy,
}
impl Platform for LockPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let mut state = self.state.borrow_mut();
        let output = state
            .pending_outputs
            .pop_front()
            .ok_or_else(|| error("No unclaimed lock output"))?;
        let qh = self.queue.borrow().handle();
        let surface = state.compositor.create_surface(&qh);
        let role = state
            .lock
            .as_ref()
            .ok_or_else(|| error("Missing session lock"))?
            .get_lock_surface(&surface, &output, &qh, surface.clone());
        let viewport = state
            .viewporter
            .as_ref()
            .map(|m| m.get_viewport(&surface, &qh, ()));
        let fractional_scale = viewport
            .as_ref()
            .and_then(|_| state.fractional_scale.as_ref())
            .map(|m| m.get_fractional_scale(&surface, &qh, surface.clone()));
        let scale = state
            .outputs
            .info(&output)
            .map(|i| i.scale_factor.max(1))
            .unwrap_or(1);
        let pool = SlotPool::new(4096, &state.shm).map_err(error)?;
        let window: Rc<LockWindow> = Rc::<LockWindow>::new_cyclic(|weak| LockWindow {
            window: Window::new(weak.clone()),
            renderer: SoftwareRenderer::new_with_repaint_buffer_type(RepaintBufferType::NewBuffer),
            surface,
            role,
            output,
            viewport,
            fractional_scale,
            qh,
            pool: RefCell::new(pool),
            buffers: RefCell::new(Vec::new()),
            pixels: RefCell::new(Vec::new()),
            size: Cell::new(PhysicalSize::new(1, 1)),
            logical_size: Cell::new((1, 1)),
            scale: Cell::new(scale as f64),
            configured: Cell::new(false),
            visible: Cell::new(false),
            retired: Cell::new(false),
            dirty: Cell::new(true),
            frame_pending: Cell::new(false),
        });
        window
            .surface
            .set_buffer_scale(if window.viewport.is_some() { 1 } else { scale });
        window
            .window
            .dispatch_event(WindowEvent::ScaleFactorChanged {
                scale_factor: scale as f32,
            });
        state.windows.push(Rc::downgrade(&window));
        Ok(window)
    }
    fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
        Some(Box::new(self.proxy.clone()))
    }
    fn run_event_loop(&self) -> Result<(), PlatformError> {

        struct CloseOnDrop<'a>(&'a LoopProxy);
        impl<'a> Drop for CloseOnDrop<'a> {
            fn drop(&mut self) {
                self.0.close();
            }
        }
        let _close = CloseOnDrop(&self.proxy);
        loop {
            while let Ok(message) = self.receiver.try_recv() {
                match message {
                    Message::Invoke(f) => f(),
                    Message::Quit => return Ok(()),
                }
            }
            if self.proxy.mailbox.quit.swap(false, Ordering::AcqRel) {
                return Ok(());
            }
            platform::update_timers_and_animations();
            self.queue
                .borrow_mut()
                .dispatch_pending(&mut self.state.borrow_mut())
                .map_err(error)?;
            let lifecycle = self.control.0.borrow().lifecycle;
            match lifecycle {
                Lifecycle::Finished => {
                    return Err(error("Compositor refused or cancelled the session lock"))
                }
                Lifecycle::UnlockRequested => {

                    match self.state.borrow().lock.as_ref() {
                        Some(lock) => lock.unlock_and_destroy(),
                        None => tracing::warn!("Unlock requested without a lock proxy"),
                    }

                    self.queue
                        .borrow_mut()
                        .roundtrip(&mut self.state.borrow_mut())
                        .map_err(error)?;
                    self.control.0.borrow_mut().lifecycle = Lifecycle::Unlocked;
                    return Ok(());
                }
                _ => {}
            }
            let (locked, layout, focus) = {
                let mut c = self.control.0.borrow_mut();
                let locked = if c.notify_locked {
                    c.notify_locked = false;
                    c.locked.clone()
                } else {
                    None
                };
                let layout = c.layout_changed.clone().zip(c.layout.take());
                let focus = c.focus_changed.clone().zip(c.focus.take());
                (locked, layout, focus)
            };
            if let Some(f) = locked {
                f();
            }
            if let Some((f, (text, caps))) = layout {
                f(text, caps);
            }
            if let Some((f, index)) = focus {
                f(index);
            }
            while !self.state.borrow().pending_outputs.is_empty() {
                let callback = self
                    .control
                    .0
                    .borrow()
                    .output_added
                    .clone()
                    .ok_or_else(|| error("No UI factory for new lock output"))?;
                let before = self.state.borrow().pending_outputs.len();
                callback()?;
                if self.state.borrow().pending_outputs.len() >= before {
                    return Err(error("Lock UI factory did not create an output window"));
                }
            }
            let windows: Vec<_> = self
                .state
                .borrow()
                .windows
                .iter()
                .filter_map(Weak::upgrade)
                .collect();
            for window in &windows {
                window.draw()?;
            }
            self.conn.flush().map_err(error)?;
            let Some(guard) = self.queue.borrow().prepare_read() else {
                continue;
            };
            let timeout = if windows.iter().any(|w| {
                !w.retired.get() && w.window.has_active_animations() && !w.frame_pending.get()
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
                return Err(error("Wayland connection closed while locked"));
            }
            if fds[0].revents & libc::POLLIN != 0 {
                guard.read().map_err(error)?;
            } else {
                drop(guard);
            }
            if fds[1].revents & libc::POLLIN != 0 {
                let mut bytes = [0; 512];
                loop {
                    match self.wake_read.borrow_mut().read(&mut bytes) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            }
        }
    }
}

struct LockWindow {
    window: Window,
    renderer: SoftwareRenderer,
    surface: wl_surface::WlSurface,
    role: ExtSessionLockSurfaceV1,
    output: wl_output::WlOutput,
    viewport: Option<WpViewport>,
    fractional_scale: Option<WpFractionalScaleV1>,
    qh: QueueHandle<LockState>,
    pool: RefCell<SlotPool>,
    buffers: RefCell<Vec<Buffer>>,
    pixels: RefCell<Vec<PremultipliedRgbaColor>>,
    size: Cell<PhysicalSize>,
    logical_size: Cell<(u32, u32)>,
    scale: Cell<f64>,
    configured: Cell<bool>,
    visible: Cell<bool>,
    retired: Cell<bool>,
    dirty: Cell<bool>,
    frame_pending: Cell<bool>,
}
impl LockWindow {
    fn resize(&self, width: u32, height: u32) -> Result<(), PlatformError> {
        let size = buffer_size(width, height, self.scale.get())?;
        self.logical_size.set((width, height));
        if let Some(viewport) = &self.viewport {
            viewport.set_destination(width as i32, height as i32);
        }
        self.size.set(size);
        self.buffers.borrow_mut().clear();
        self.pixels.borrow_mut().resize(
            size.width as usize * size.height as usize,
            PremultipliedRgbaColor::default(),
        );
        self.window.dispatch_event(WindowEvent::Resized {
            size: LogicalSize::new(width as f32, height as f32),
        });
        self.dirty.set(true);
        Ok(())
    }
    fn scale_changed(&self, scale: f64) {
        if !scale.is_finite() || scale <= 0. || self.scale.replace(scale) == scale {
            return;
        }
        self.surface.set_buffer_scale(if self.viewport.is_some() {
            1
        } else {
            scale as i32
        });
        self.window.dispatch_event(WindowEvent::ScaleFactorChanged {
            scale_factor: scale as f32,
        });
        let (width, height) = self.logical_size.get();

        if self.resize(width, height).is_err() {
            self.retired.set(true);
        }
    }
    fn draw(&self) -> Result<(), PlatformError> {
        if self.retired.get()
            || !self.configured.get()
            || !self.visible.get()
            || self.frame_pending.get()
            || !(self.dirty.replace(false) || self.window.has_active_animations())
        {
            return Ok(());
        }
        let size = self.size.get();
        let mut pixels = self.pixels.borrow_mut();
        self.renderer.render(&mut pixels, size.width as usize);
        let mut pool = self.pool.borrow_mut();
        let mut buffers = self.buffers.borrow_mut();
        let free = buffers.iter().position(|b| b.canvas(&mut pool).is_some());
        let index = match free {
            Some(index) => index,
            None => {
                if buffers.len() >= 3 {
                    self.dirty.set(true);
                    return Ok(());
                }
                let (buffer, _) = pool
                    .create_buffer(
                        size.width as i32,
                        size.height as i32,
                        size.width as i32 * 4,
                        wl_shm::Format::Xrgb8888,
                    )
                    .map_err(error)?;
                buffers.push(buffer);
                buffers.len() - 1
            }
        };
        let buffer = &buffers[index];
        let canvas = buffer
            .canvas(&mut pool)
            .ok_or_else(|| error("Lock buffer is still in use"))?;
        for (pixel, bytes) in pixels.iter().zip(canvas.as_chunks_mut::<4>().0) {

            bytes.copy_from_slice(&[pixel.blue, pixel.green, pixel.red, 255]);
        }
        self.surface
            .damage_buffer(0, 0, size.width as i32, size.height as i32);
        self.surface.frame(&self.qh, self.surface.clone());
        self.frame_pending.set(true);
        buffer.attach_to(&self.surface).map_err(error)?;
        self.surface.commit();
        Ok(())
    }
    fn retire(&self) {
        if !self.retired.replace(true) {
            self.role.destroy();
            if let Some(scale) = &self.fractional_scale {
                scale.destroy();
            }
            if let Some(viewport) = &self.viewport {
                viewport.destroy();
            }
            self.surface.destroy();
        }
    }
}
impl Drop for LockWindow {
    fn drop(&mut self) {
        self.retire();
    }
}
impl WindowAdapter for LockWindow {
    fn window(&self) -> &Window {
        &self.window
    }
    fn renderer(&self) -> &dyn platform::Renderer {
        &self.renderer
    }
    fn size(&self) -> PhysicalSize {
        self.size.get()
    }
    fn request_redraw(&self) {
        self.dirty.set(true);
    }
    fn set_visible(&self, visible: bool) -> Result<(), PlatformError> {

        self.visible.set(visible);
        self.dirty.set(true);
        Ok(())
    }
}

fn buffer_size(width: u32, height: u32, scale: f64) -> Result<PhysicalSize, PlatformError> {
    if width == 0 || height == 0 || !scale.is_finite() || scale <= 0. {
        return Err(error("Invalid lock surface geometry"));
    }
    let w = (width as f64 * scale).ceil();
    let h = (height as f64 * scale).ceil();
    if w > 32768. || h > 32768. || w * h > 67_108_864. {
        return Err(error("Lock surface is too large"));
    }
    Ok(PhysicalSize::new(w as u32, h as u32))
}

struct LockState {
    registry: RegistryState,
    seats: SeatState,
    outputs: OutputState,
    shm: Shm,
    compositor: CompositorState,
    viewporter: Option<WpViewporter>,
    fractional_scale: Option<WpFractionalScaleManagerV1>,
    cursor_shape: Option<WpCursorShapeManagerV1>,
    cursor_device: Option<WpCursorShapeDeviceV1>,
    lock: Option<ExtSessionLockV1>,
    windows: Vec<Weak<LockWindow>>,
    pending_outputs: VecDeque<wl_output::WlOutput>,
    control: SessionLockController,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    focused: Weak<LockWindow>,
    pressed: std::collections::HashMap<u32, slint::SharedString>,
    keymap: Option<xkbcommon::xkb::Keymap>,
    modifiers: Modifiers,
    repeat_info: RepeatInfo,
    repeating: Option<u32>,
    repeat_delay: slint::Timer,
    repeat_timer: Rc<slint::Timer>,
}
impl LockState {
    fn window_for(&self, surface: &wl_surface::WlSurface) -> Option<Rc<LockWindow>> {
        self.windows
            .iter()
            .filter_map(Weak::upgrade)
            .find(|w| !w.retired.get() && w.surface == *surface)
    }
    fn stop_repeat(&mut self) {
        self.repeating = None;
        self.repeat_delay.stop();
        self.repeat_timer.stop();
    }
}
impl CompositorHandler for LockState {
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        scale: i32,
    ) {
        if let Some(w) = self.window_for(surface) {
            if w.fractional_scale.is_none() {
                w.scale_changed(scale.max(1) as f64);
            }
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
        if let Some(w) = self.window_for(surface) {
            w.frame_pending.set(false);
        }
    }
}
impl OutputHandler for LockState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        if !self.pending_outputs.contains(&output)
            && !self
                .windows
                .iter()
                .filter_map(Weak::upgrade)
                .any(|w| w.output == output && !w.retired.get())
        {
            self.pending_outputs.push_back(output);
            self.control.0.borrow_mut().output_count += 1;
        }
    }
    fn update_output(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if let Some(info) = self.outputs.info(&output) {
            for w in self
                .windows
                .iter()
                .filter_map(Weak::upgrade)
                .filter(|w| w.output == output && w.fractional_scale.is_none())
            {
                w.scale_changed(info.scale_factor.max(1) as f64);
            }
        }
    }
    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.pending_outputs.retain(|o| *o != output);
        for w in self
            .windows
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|w| w.output == output)
        {
            w.retire();
        }
        let count = self.control.output_count();
        self.control.0.borrow_mut().output_count = count.saturating_sub(1);
    }
}
impl Dispatch<ExtSessionLockV1, ()> for LockState {
    fn event(
        state: &mut Self,
        _: &ExtSessionLockV1,
        event: ext_session_lock_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut control = state.control.0.borrow_mut();
        match event {
            ext_session_lock_v1::Event::Locked if control.lifecycle == Lifecycle::Pending => {
                control.lifecycle = Lifecycle::Locked;
                control.notify_locked = true;
            }
            ext_session_lock_v1::Event::Finished => control.lifecycle = Lifecycle::Finished,
            _ => {}
        }
    }
}
impl Dispatch<ExtSessionLockSurfaceV1, wl_surface::WlSurface> for LockState {
    fn event(
        state: &mut Self,
        proxy: &ExtSessionLockSurfaceV1,
        event: ext_session_lock_surface_v1::Event,
        surface: &wl_surface::WlSurface,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_session_lock_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            if let Some(w) = state.window_for(surface) {
                if w.resize(width, height).is_ok() {
                    proxy.ack_configure(serial);
                    w.configured.set(true);

                    w.frame_pending.set(false);
                } else {
                    state.control.0.borrow_mut().lifecycle = Lifecycle::Finished;
                }
            }
        }
    }
}
impl Dispatch<WpFractionalScaleV1, wl_surface::WlSurface> for LockState {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        surface: &wl_surface::WlSurface,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            if scale > 0 {
                if let Some(w) = state.window_for(surface) {
                    w.scale_changed(scale as f64 / 120.);
                }
            }
        }
    }
}

impl SeatHandler for LockState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
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
            self.focused = Weak::new();
            if let Some(keyboard) = self.keyboard.take() {
                keyboard.release();
            }
        }
        if capability == Capability::Pointer {
            if let Some(device) = self.cursor_device.take() {
                device.destroy();
            }
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}
impl KeyboardHandler for LockState {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        self.stop_repeat();
        self.pressed.clear();
        if let Some(w) = self.window_for(surface) {
            self.focused = Rc::downgrade(&w);
            w.window
                .dispatch_event(WindowEvent::WindowActiveChanged(true));
            self.control.0.borrow_mut().focus = self
                .windows
                .iter()
                .position(|weak| weak.ptr_eq(&self.focused));
        }
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
        self.stop_repeat();
        if let Some(w) = self.focused.upgrade() {
            for (_, text) in self.pressed.drain() {
                w.window.dispatch_event(WindowEvent::KeyReleased { text });
            }
            w.window
                .dispatch_event(WindowEvent::WindowActiveChanged(false));
        }
        self.focused = Weak::new();
    }
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        let Some(w) = self.focused.upgrade().filter(|w| !w.retired.get()) else {
            return;
        };
        self.stop_repeat();
        let text = crate::wayland::key_text(&event, self.modifiers);
        if text.is_empty() {
            return;
        }
        self.pressed.insert(event.raw_code, text.clone());
        w.window
            .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        if self
            .keymap
            .as_ref()
            .is_some_and(|map| map.key_repeats(xkbcommon::xkb::Keycode::new(event.raw_code + 8)))
        {
            if let RepeatInfo::Repeat { delay, rate } = self.repeat_info {
                self.repeating = Some(event.raw_code);
                let weak = self.focused.clone();
                let timer = self.repeat_timer.clone();
                self.repeat_delay.start(
                    slint::TimerMode::SingleShot,
                    std::time::Duration::from_millis(delay as u64),
                    move || {
                        if let Some(w) = weak.upgrade().filter(|w| !w.retired.get()) {
                            w.window.dispatch_event(WindowEvent::KeyPressRepeated {
                                text: text.clone(),
                            });
                        }
                        let weak = weak.clone();
                        let text = text.clone();
                        timer.start(
                            slint::TimerMode::Repeated,
                            std::time::Duration::from_secs_f64(1. / rate.get() as f64),
                            move || {
                                if let Some(w) = weak.upgrade().filter(|w| !w.retired.get()) {
                                    w.window.dispatch_event(WindowEvent::KeyPressRepeated {
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
        if let Some(w) = self.focused.upgrade().filter(|w| !w.retired.get()) {
            w.window.dispatch_event(WindowEvent::KeyPressRepeated {
                text: crate::wayland::key_text(&event, self.modifiers),
            });
        }
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
        if let (Some(w), Some(text)) =
            (self.focused.upgrade(), self.pressed.remove(&event.raw_code))
        {
            w.window.dispatch_event(WindowEvent::KeyReleased { text });
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
        self.control.0.borrow_mut().layout = Some((layout, modifiers.caps_lock));
    }
}
impl PointerHandler for LockState {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for e in events {
            let Some(w) = self.window_for(&e.surface) else {
                continue;
            };
            let position = LogicalPosition::new(e.position.0 as f32, e.position.1 as f32);
            let event = match e.kind {
                PointerEventKind::Enter { serial, .. } => {
                    if let Some(device) = &self.cursor_device {
                        device.set_shape(serial, Shape::Default);
                    }
                    WindowEvent::PointerMoved { position }
                }
                PointerEventKind::Motion { .. } => WindowEvent::PointerMoved { position },
                PointerEventKind::Leave { .. } => WindowEvent::PointerExited,
                PointerEventKind::Press { button, .. } => WindowEvent::PointerPressed {
                    position,
                    button: crate::wayland::pointer_button(button),
                },
                PointerEventKind::Release { button, .. } => WindowEvent::PointerReleased {
                    position,
                    button: crate::wayland::pointer_button(button),
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
            w.window.dispatch_event(event);
        }
    }
}

impl ShmHandler for LockState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}
impl ProvidesRegistryState for LockState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}
delegate_compositor!(LockState);
delegate_output!(LockState);
delegate_shm!(LockState);
delegate_seat!(LockState);
delegate_keyboard!(LockState);
delegate_pointer!(LockState);
delegate_registry!(LockState);
wayland_client::delegate_noop!(LockState: ignore ExtSessionLockManagerV1);
wayland_client::delegate_noop!(LockState: ignore WpViewporter);
wayland_client::delegate_noop!(LockState: ignore WpViewport);
wayland_client::delegate_noop!(LockState: ignore WpFractionalScaleManagerV1);
wayland_client::delegate_noop!(LockState: ignore WpCursorShapeManagerV1);
wayland_client::delegate_noop!(LockState: ignore WpCursorShapeDeviceV1);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_a_confirmed_lock_can_be_unlocked() {
        for phase in [
            Lifecycle::Pending,
            Lifecycle::Finished,
            Lifecycle::Unlocked,
            Lifecycle::UnlockRequested,
        ] {
            let mut lifecycle = phase;
            assert!(lifecycle.request_unlock().is_err());
            assert_eq!(lifecycle, phase);
        }
        let mut lifecycle = Lifecycle::Locked;
        assert!(lifecycle.request_unlock().is_ok());
        assert_eq!(lifecycle, Lifecycle::UnlockRequested);
        assert!(lifecycle.request_unlock().is_err());
    }
    #[test]
    fn configure_dimensions_are_exact_with_integer_and_fractional_scale() {
        assert_eq!(
            buffer_size(1920, 1080, 2.).unwrap(),
            PhysicalSize::new(3840, 2160)
        );
        assert_eq!(
            buffer_size(1001, 777, 1.25).unwrap(),
            PhysicalSize::new(1252, 972)
        );
        for (w, h, s) in [
            (0, 1080, 1.),
            (1920, 0, 1.),
            (1920, 1080, 0.),
            (1920, 1080, f64::NAN),
            (u32::MAX, 1080, 1.),
        ] {
            assert!(buffer_size(w, h, s).is_err());
        }
    }
}
