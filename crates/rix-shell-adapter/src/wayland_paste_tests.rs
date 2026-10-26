use std::{
    fs::File,
    io::{Seek, SeekFrom, Write},
    os::{
        fd::{AsFd, AsRawFd, FromRawFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use wayland_protocols_wlr::layer_shell::v1::server::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use wayland_server::{
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_data_device, wl_data_device_manager,
        wl_data_offer, wl_data_source, wl_keyboard, wl_pointer, wl_region, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    },
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New,
};

const MIME: &str = "text/plain;charset=utf-8";
const OFFER_DELAY: Duration = Duration::from_millis(100);

#[derive(Clone, Copy)]
enum Scenario {
    RepeatedPaste,
    FocusChange,
}
impl Scenario {
    fn name(self) -> &'static str {
        match self {
            Self::RepeatedPaste => "repeat",
            Self::FocusChange => "focus",
        }
    }
}

struct Server {
    scenario: Scenario,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    data_device: Option<wl_data_device::WlDataDevice>,
    offer: Option<wl_data_offer::WlDataOffer>,
    surface: Option<wl_surface::WlSurface>,
    layer: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
    configured: bool,
    mapped: bool,
    focused: bool,
    attached: Option<wl_buffer::WlBuffer>,
    callbacks: Vec<wl_callback::WlCallback>,
    incoming_pipe: Option<File>,
    input_started: Option<Instant>,
    transfer_sent: bool,
    ctrl_mask: u32,
    serial: u32,
}
impl Server {
    fn new(scenario: Scenario) -> Self {
        Self {
            scenario,
            keyboard: None,
            pointer: None,
            data_device: None,
            offer: None,
            surface: None,
            layer: None,
            configured: false,
            mapped: false,
            focused: false,
            attached: None,
            callbacks: Vec::new(),
            incoming_pipe: None,
            input_started: None,
            transfer_sent: false,
            ctrl_mask: 0,
            serial: 40,
        }
    }
    fn next_serial(&mut self) -> u32 {
        self.serial += 1;
        self.serial
    }
    fn key(&mut self, code: u32, pressed: bool) {
        let serial = self.next_serial();
        self.keyboard.as_ref().unwrap().key(
            serial,
            serial,
            code,
            if pressed {
                wl_keyboard::KeyState::Pressed
            } else {
                wl_keyboard::KeyState::Released
            },
        );
    }
    fn control(&mut self, pressed: bool) {
        self.key(29, pressed);
        let serial = self.next_serial();
        self.keyboard.as_ref().unwrap().modifiers(
            serial,
            if pressed { self.ctrl_mask } else { 0 },
            0,
            0,
            0,
        );
    }
    fn paste(&mut self) {
        self.control(true);
        self.key(47, true);
        self.key(47, false);
        self.control(false);
    }
    fn type_x(&mut self) {
        self.key(45, true);
        self.key(45, false);
    }
    fn advance(&mut self) {

        if !self.focused
            && self.mapped
            && self.keyboard.is_some()
            && self.pointer.is_some()
            && self.data_device.is_some()
        {
            let surface = self.surface.as_ref().unwrap().clone();
            let serial = self.next_serial();
            self.keyboard
                .as_ref()
                .unwrap()
                .enter(serial, &surface, Vec::new());
            let serial = self.next_serial();
            self.keyboard
                .as_ref()
                .unwrap()
                .modifiers(serial, 0, 0, 0, 0);
            let serial = self.next_serial();
            self.pointer
                .as_ref()
                .unwrap()
                .enter(serial, &surface, 12., 76.);
            self.pointer.as_ref().unwrap().frame();
            let device = self.data_device.as_ref().unwrap();
            let offer = self.offer.as_ref().unwrap();
            device.data_offer(offer);
            offer.offer(MIME.into());
            device.selection(Some(offer));
            self.focused = true;
        }

        if self.input_started.is_none() && self.focused && self.incoming_pipe.is_some() {
            self.paste();
            match self.scenario {
                Scenario::RepeatedPaste => {
                    self.type_x();
                    self.paste();
                }
                Scenario::FocusChange => {
                    let serial = self.next_serial();
                    self.pointer.as_ref().unwrap().button(
                        serial,
                        serial,
                        0x110,
                        wl_pointer::ButtonState::Pressed,
                    );
                    let serial = self.next_serial();
                    self.pointer.as_ref().unwrap().button(
                        serial,
                        serial,
                        0x110,
                        wl_pointer::ButtonState::Released,
                    );
                    self.pointer.as_ref().unwrap().frame();

                    self.type_x();
                }
            }
            self.input_started = Some(Instant::now());
        }
        if !self.transfer_sent
            && self
                .input_started
                .is_some_and(|started| started.elapsed() >= OFFER_DELAY)
        {
            self.incoming_pipe.take().unwrap().write_all(b"P").unwrap();
            self.transfer_sent = true;
        }
    }
}

impl GlobalDispatch<wl_compositor::WlCompositor, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        compositor: New<wl_compositor::WlCompositor>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(compositor, ());
    }
}
impl Dispatch<wl_compositor::WlCompositor, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wl_compositor::WlCompositor,
        request: wl_compositor::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_compositor::Request::CreateSurface { id } => {
                state.surface = Some(init.init(id, ()));
            }
            wl_compositor::Request::CreateRegion { id } => {
                init.init(id, ());
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_surface::WlSurface, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wl_surface::WlSurface,
        request: wl_surface::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_surface::Request::Attach { buffer, .. } => state.attached = buffer,
            wl_surface::Request::Frame { callback } => {
                state.callbacks.push(init.init(callback, ()));
            }
            wl_surface::Request::Commit => {
                if !state.configured {
                    state.layer.as_ref().unwrap().configure(1, 240, 120);
                    state.configured = true;
                }
                if let Some(buffer) = state.attached.take() {
                    buffer.release();
                    state.mapped = true;
                }
                for callback in state.callbacks.drain(..) {
                    callback.done(0);
                }
            }
            _ => {}
        }
    }
}
impl GlobalDispatch<wl_shm::WlShm, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        shm: New<wl_shm::WlShm>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let shm = init.init(shm, ());
        shm.format(wl_shm::Format::Argb8888);
        shm.format(wl_shm::Format::Xrgb8888);
    }
}
impl Dispatch<wl_shm::WlShm, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_shm::WlShm,
        request: wl_shm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm::Request::CreatePool { id, .. } = request {
            init.init(id, ());
        }
    }
}
impl Dispatch<wl_shm_pool::WlShmPool, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_shm_pool::WlShmPool,
        request: wl_shm_pool::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm_pool::Request::CreateBuffer { id, .. } = request {
            init.init(id, ());
        }
    }
}
impl GlobalDispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        shell: New<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(shell, ());
    }
}
impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
        request: zwlr_layer_shell_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let zwlr_layer_shell_v1::Request::GetLayerSurface { id, .. } = request {
            state.layer = Some(init.init(id, ()));
        }
    }
}
impl GlobalDispatch<wl_seat::WlSeat, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        seat: New<wl_seat::WlSeat>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let seat = init.init(seat, ());
        seat.capabilities(wl_seat::Capability::Keyboard | wl_seat::Capability::Pointer);
        seat.name("paste-fixture".into());
    }
}
impl Dispatch<wl_seat::WlSeat, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wl_seat::WlSeat,
        request: wl_seat::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_seat::Request::GetKeyboard { id } => {
                let keyboard = init.init(id, ());
                let keymap = xkbcommon::xkb::Keymap::new_from_names(
                    &xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS),
                    "",
                    "",
                    "us",
                    "",
                    None,
                    xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
                )
                .unwrap();
                let index = keymap.mod_get_index(xkbcommon::xkb::MOD_NAME_CTRL);
                assert!(index < 32);
                state.ctrl_mask = 1 << index;
                let mut contents = keymap
                    .get_as_string(xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1)
                    .into_bytes();
                contents.push(0);

                let fd =
                    unsafe { libc::memfd_create(c"rix-paste-keymap".as_ptr(), libc::MFD_CLOEXEC) };
                assert!(fd >= 0);
                let mut file = unsafe { File::from_raw_fd(fd) };
                file.write_all(&contents).unwrap();
                file.seek(SeekFrom::Start(0)).unwrap();
                keyboard.keymap(
                    wl_keyboard::KeymapFormat::XkbV1,
                    file.as_fd(),
                    contents.len() as u32,
                );
                keyboard.repeat_info(0, 0);
                state.keyboard = Some(keyboard);
            }
            wl_seat::Request::GetPointer { id } => {
                state.pointer = Some(init.init(id, ()));
            }
            _ => {}
        }
    }
}
impl GlobalDispatch<wl_data_device_manager::WlDataDeviceManager, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        manager: New<wl_data_device_manager::WlDataDeviceManager>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(manager, ());
    }
}
impl Dispatch<wl_data_device_manager::WlDataDeviceManager, ()> for Server {
    fn request(
        state: &mut Self,
        client: &Client,
        _: &wl_data_device_manager::WlDataDeviceManager,
        request: wl_data_device_manager::Request,
        _: &(),
        handle: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_data_device_manager::Request::GetDataDevice { id, .. } => {
                let device = init.init(id, ());
                let offer = client
                    .create_resource::<wl_data_offer::WlDataOffer, (), Server>(handle, 3, ())
                    .unwrap();
                state.data_device = Some(device);
                state.offer = Some(offer);
            }
            wl_data_device_manager::Request::CreateDataSource { id } => {
                init.init(id, ());
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_data_offer::WlDataOffer, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wl_data_offer::WlDataOffer,
        request: wl_data_offer::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let wl_data_offer::Request::Receive { mime_type, fd } = request {
            assert_eq!(mime_type, MIME);
            assert!(state.incoming_pipe.is_none());
            state.incoming_pipe = Some(File::from(fd));
        }
    }
}

macro_rules! ignore_requests {
    ($($module:ident::$interface:ident),+ $(,)?) => {$(
        impl Dispatch<$module::$interface, ()> for Server {
            fn request(
                _: &mut Self,
                _: &Client,
                _: &$module::$interface,
                _: $module::Request,
                _: &(),
                _: &DisplayHandle,
                _: &mut DataInit<'_, Self>,
            ) {}
        }
    )+ };
}
ignore_requests!(
    wl_buffer::WlBuffer,
    wl_callback::WlCallback,
    wl_region::WlRegion,
    wl_keyboard::WlKeyboard,
    wl_pointer::WlPointer,
    wl_data_device::WlDataDevice,
    wl_data_source::WlDataSource,
    zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
);

fn run_fixture(scenario: Scenario) {
    let (server_socket, client_socket) = UnixStream::pair().unwrap();
    let mut display = Display::<Server>::new().unwrap();
    let mut handle = display.handle();
    handle.create_global::<Server, wl_compositor::WlCompositor, _>(6, ());
    handle.create_global::<Server, wl_shm::WlShm, _>(1, ());
    handle.create_global::<Server, zwlr_layer_shell_v1::ZwlrLayerShellV1, _>(4, ());
    handle.create_global::<Server, wl_seat::WlSeat, _>(8, ());
    handle.create_global::<Server, wl_data_device_manager::WlDataDeviceManager, _>(3, ());
    handle.insert_client(server_socket, Arc::new(())).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let server_done = done.clone();
    let server = std::thread::spawn(move || {
        let mut state = Server::new(scenario);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !server_done.load(Ordering::Acquire) {
            display.dispatch_clients(&mut state).unwrap();
            state.advance();
            display.flush_clients().unwrap();
            assert!(Instant::now() < deadline, "Delayed-paste fixture timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(state.transfer_sent, "The delayed transfer was never sent");
    });
    let fd = client_socket.as_raw_fd();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "wayland::paste_tests::paste_fixture_client",
        "--ignored",
        "--nocapture",
    ]);
    command
        .env("WAYLAND_SOCKET", fd.to_string())
        .env("RIX_SHELL_RENDERER", "software")
        .env("RIX_ADAPTER_PASTE_TEST", scenario.name())
        .env_remove("WAYLAND_DISPLAY");

    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command.output().unwrap();
    done.store(true, Ordering::Release);
    let server_result = server.join();
    assert!(
        output.status.success(),
        "{} delayed-paste client failed:\n{}\n{}",
        scenario.name(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    server_result.unwrap();
}

#[test]
fn delayed_clipboard_preserves_multiple_pastes_and_intervening_typing() {
    run_fixture(Scenario::RepeatedPaste);
}

#[test]
fn clicking_another_text_input_cancels_a_delayed_paste() {
    run_fixture(Scenario::FocusChange);
}

#[test]
#[ignore = "subprocess fixture for the disposable delayed-paste server"]
fn paste_fixture_client() {
    let scenario = std::env::var("RIX_ADAPTER_PASTE_TEST").unwrap();
    assert!(matches!(scenario.as_str(), "repeat" | "focus"));
    slint::slint! {
        export component PasteProbe inherits Window {
            width: 240px;
            height: 120px;
            in-out property <string> first-text <=> first.text;
            in-out property <string> second-text <=> second.text;
            out property <bool> first-focused: first.has-focus;
            out property <bool> second-focused: second.has-focus;
            callback focus-first();
            focus-first => { first.focus(); }
            first := TextInput {
                x: 0px;
                y: 0px;
                width: 240px;
                height: 50px;
            }
            second := TextInput {
                x: 0px;
                y: 60px;
                width: 240px;
                height: 50px;
            }
        }
    }
    super::install(Default::default()).unwrap();
    let ui = PasteProbe::new().unwrap();
    ui.show().unwrap();
    ui.invoke_focus_first();
    let weak = ui.as_weak();
    let start = Instant::now();
    let ready_at = std::rc::Rc::new(std::cell::Cell::new(None));
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(10),
        move || {
            let ready = i_slint_core::context::with_global_context(
                || unreachable!(),
                |context| {
                    context
                        .platform()
                        .clipboard_text(slint::platform::Clipboard::DefaultClipboard)
                        .as_deref()
                        == Some("P")
                },
            )
            .unwrap();
            if ready && ready_at.get().is_none() {
                ready_at.set(Some(Instant::now()));
            }
            if ready_at
                .get()
                .is_some_and(|at: Instant| at.elapsed() >= Duration::from_millis(50))
            {
                let ui = weak.upgrade().unwrap();
                let expected = if scenario == "repeat" {
                    ("PxP", "")
                } else {
                    ("", "x")
                };
                assert_eq!(ui.get_first_text().as_str(), expected.0, "First field");
                assert_eq!(ui.get_second_text().as_str(), expected.1, "Second field");
                if scenario == "repeat" {
                    assert!(ui.get_first_focused());
                } else {
                    assert!(ui.get_second_focused());
                }
                slint::quit_event_loop().unwrap();
            }
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "Paste was never delivered"
            );
        },
    );
    slint::run_event_loop().unwrap();
}
