use super::{set_surface_input_region, CompositorState};
use crate::{SurfaceClip, SurfaceRect};
use smithay_client_toolkit::shell::WaylandSurface;
use std::{
    collections::HashMap,
    os::{
        fd::AsRawFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use wayland_client::{globals::registry_queue_init, Connection, Proxy};
use wayland_protocols_wlr::layer_shell::v1::server::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use wayland_server::{
    backend::ObjectId,
    protocol::{
        wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source,
        wl_pointer, wl_region, wl_seat, wl_shm, wl_shm_pool, wl_surface,
    },
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New, Resource,
};

type Rectangle = (i32, i32, i32, i32);

#[derive(Default)]
struct Server {
    regions: HashMap<ObjectId, Vec<Rectangle>>,
    input_regions: Vec<Option<Vec<Rectangle>>>,
    layer_namespaces: Vec<String>,
    exclusive_zones: Vec<i32>,
    pointer: Option<wl_pointer::WlPointer>,
    data_device: Option<wl_data_device::WlDataDevice>,
    copied_text: Arc<std::sync::Mutex<Option<String>>>,
}

impl GlobalDispatch<wl_shm::WlShm, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_shm::WlShm>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let shm = init.init(resource, ());
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
        _: wl_shm_pool::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}
impl GlobalDispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
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
        if let zwlr_layer_shell_v1::Request::GetLayerSurface { id, namespace, .. } = request {
            state.layer_namespaces.push(namespace);
            init.init(id, ());
        }
    }
}
impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        request: zwlr_layer_surface_v1::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let zwlr_layer_surface_v1::Request::SetExclusiveZone { zone } = request {
            state.exclusive_zones.push(zone);
        }
    }
}

impl GlobalDispatch<wl_compositor::WlCompositor, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_compositor::WlCompositor>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
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
                let surface = init.init(id, ());
                if let Some(pointer) = &state.pointer {
                    pointer.enter(40, &surface, 4., 4.);
                    pointer.button(41, 1, 0x110, wl_pointer::ButtonState::Pressed);
                    pointer.frame();
                }
            }
            wl_compositor::Request::CreateRegion { id } => {
                let region = init.init(id, ());
                state.regions.insert(region.id(), Vec::new());
            }
            _ => {}
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
        seat.capabilities(wl_seat::Capability::Pointer);
        seat.name("clipboard-fixture".into());
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
        if let wl_seat::Request::GetPointer { id } = request {
            state.pointer = Some(init.init(id, ()));
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_pointer::WlPointer,
        _: wl_pointer::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
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
            wl_data_device_manager::Request::CreateDataSource { id } => {
                init.init(id, ());
            }
            wl_data_device_manager::Request::GetDataDevice { id, .. } => {
                let device = init.init(id, ());
                let offer = client
                    .create_resource::<wl_data_offer::WlDataOffer, (), Server>(handle, 3, ())
                    .unwrap();
                device.data_offer(&offer);
                offer.offer("text/plain;charset=utf-8".into());
                device.selection(Some(&offer));
                state.data_device = Some(device);
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_data_offer::WlDataOffer, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_data_offer::WlDataOffer,
        request: wl_data_offer::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let wl_data_offer::Request::Receive { mime_type, fd } = request {
            use std::io::Write;
            assert_eq!(mime_type, "text/plain;charset=utf-8");
            std::fs::File::from(fd)
                .write_all("External buffer\n世界".as_bytes())
                .unwrap();
        }
    }
}
impl Dispatch<wl_data_device::WlDataDevice, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wl_data_device::WlDataDevice,
        request: wl_data_device::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let wl_data_device::Request::SetSelection {
            source: Some(source),
            serial,
        } = request
        {
            use std::os::fd::{AsFd, FromRawFd};
            assert_eq!(
                serial, 41,
                "Clipboard ownership must use the actual input serial"
            );
            let mut fds = [0; 2];
            assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
            let mut read = unsafe { std::fs::File::from_raw_fd(fds[0]) };
            let write = unsafe { std::fs::File::from_raw_fd(fds[1]) };
            source.send("text/plain;charset=utf-8".into(), write.as_fd());

            state.data_device.as_ref().unwrap().selection(None);
            let copied = state.copied_text.clone();
            std::thread::spawn(move || {
                use std::io::Read;
                let mut text = String::new();
                read.read_to_string(&mut text).unwrap();
                *copied.lock().unwrap() = Some(text);
            });
        }
    }
}
impl Dispatch<wl_data_source::WlDataSource, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_data_source::WlDataSource,
        _: wl_data_source::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

#[test]
fn shell_platform_copies_and_pastes_real_wayland_text_without_using_the_host_clipboard() {
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
    let copied = Arc::new(std::sync::Mutex::new(None));
    let result = copied.clone();
    let server = std::thread::spawn(move || {
        let mut state = Server {
            copied_text: copied,
            ..Default::default()
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !server_done.load(Ordering::Acquire) {
            display.dispatch_clients(&mut state).unwrap();
            display.flush_clients().unwrap();
            assert!(Instant::now() < deadline, "Clipboard fixture timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let fd = client_socket.as_raw_fd();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "wayland::input_tests::clipboard_fixture_client",
        "--ignored",
        "--nocapture",
    ]);
    command
        .env("WAYLAND_SOCKET", fd.to_string())
        .env("RIX_SHELL_RENDERER", "software")
        .env("RIX_ADAPTER_CLIPBOARD_TEST", "1")
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
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(result.lock().unwrap().as_deref(), Some("From shell\n世界"));
}

#[test]
#[ignore = "subprocess fixture for the disposable clipboard server"]
fn clipboard_fixture_client() {
    assert_eq!(
        std::env::var("RIX_ADAPTER_CLIPBOARD_TEST").as_deref(),
        Ok("1")
    );
    slint::slint! { export component ClipboardProbe inherits Window { width: 100px; height: 100px; } }
    super::install(Default::default()).unwrap();
    let _ui = ClipboardProbe::new().unwrap();
    let start = Instant::now();
    let copied_at = std::rc::Rc::new(std::cell::Cell::new(None));
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(10),
        move || {
            use slint::platform::Clipboard;
            i_slint_core::context::with_global_context(
                || unreachable!(),
                |context| {
                    if copied_at.get().is_none()
                        && context
                            .platform()
                            .clipboard_text(Clipboard::DefaultClipboard)
                            .as_deref()
                            == Some("External buffer\n世界")
                    {
                        context
                            .platform()
                            .set_clipboard_text("From shell\n世界", Clipboard::DefaultClipboard);
                        copied_at.set(Some(Instant::now()));
                    }
                },
            )
            .unwrap();
            if copied_at
                .get()
                .is_some_and(|at: Instant| at.elapsed() > Duration::from_millis(250))
            {
                i_slint_core::context::with_global_context(
                    || unreachable!(),
                    |context| {
                        assert_eq!(
                            context
                                .platform()
                                .clipboard_text(Clipboard::DefaultClipboard),
                            None,
                            "selection(NULL) must clear the cached text"
                        );
                    },
                )
                .unwrap();
                slint::quit_event_loop().unwrap();
            }
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "Incoming text was never delivered"
            );
        },
    );
    slint::run_event_loop().unwrap();
}
impl Dispatch<wl_region::WlRegion, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        region: &wl_region::WlRegion,
        request: wl_region::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_region::Request::Add {
                x,
                y,
                width,
                height,
            } => {
                state
                    .regions
                    .get_mut(&region.id())
                    .unwrap()
                    .push((x, y, width, height));
            }
            wl_region::Request::Destroy => {
                state.regions.remove(&region.id());
            }
            _ => panic!("Unexpected region request"),
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
        _: &mut DataInit<'_, Self>,
    ) {
        if let wl_surface::Request::SetInputRegion { region } = request {
            state
                .input_regions
                .push(region.map(|region| state.regions.get(&region.id()).unwrap().clone()));
        }
    }
}

struct FixtureClient;
impl<I: wayland_client::Proxy, U> wayland_client::Dispatch<I, U> for FixtureClient {
    fn event(
        _: &mut Self,
        _: &I,
        _: I::Event,
        _: &U,
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
    }
}

#[test]
fn absent_geometry_accepts_pointer_input_while_explicit_empty_geometry_does_not() {
    let (server_socket, client_socket) = UnixStream::pair().unwrap();
    let mut display = Display::<Server>::new().unwrap();
    let mut handle = display.handle();
    handle.create_global::<Server, wl_compositor::WlCompositor, _>(6, ());
    handle.insert_client(server_socket, Arc::new(())).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let server_done = done.clone();
    let server = std::thread::spawn(move || {
        let mut state = Server::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !server_done.load(Ordering::Acquire) {
            display.dispatch_clients(&mut state).unwrap();
            display.flush_clients().unwrap();
            assert!(Instant::now() < deadline, "Input-region fixture timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
        state.input_regions
    });

    let connection = Connection::from_socket(client_socket).unwrap();
    let (globals, mut queue) = registry_queue_init::<FixtureClient>(&connection).unwrap();
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).unwrap();
    let surface = compositor.create_surface(&qh);

    assert!(set_surface_input_region(&surface, &compositor, None)
        .unwrap()
        .is_none());

    let empty = set_surface_input_region(&surface, &compositor, Some(&[])).unwrap();
    assert!(empty.is_some());
    let shape = SurfaceRect {
        x: 8.,
        y: 24.25,
        width: 40.,
        height: 40.,
        radius: 10.,
        opacity: 1.,
        clip: Some(SurfaceClip {
            x: 0.,
            y: 0.,
            width: 64.,
            height: 48.,
        }),
    };
    let rounded = set_surface_input_region(&surface, &compositor, Some(&[shape])).unwrap();
    assert!(rounded.is_some());

    set_surface_input_region(&surface, &compositor, None).unwrap();
    queue.roundtrip(&mut FixtureClient).unwrap();
    done.store(true, Ordering::Release);
    let observed = server.join().unwrap();
    assert_eq!(observed.len(), 4);
    assert_eq!(
        observed[0], None,
        "Default region must accept the entire surface"
    );
    assert_eq!(
        observed[1],
        Some(Vec::new()),
        "Explicit empty region must reject all input"
    );
    assert_eq!(
        observed[3], None,
        "Restrictive input must be reset to the default"
    );
    let rectangles = observed[2].as_ref().unwrap();
    let contains = |px, py| {
        rectangles
            .iter()
            .any(|&(x, y, w, h)| px >= x && px < x + w && py >= y && py < y + h)
    };
    assert!(contains(25, 40), "Visible card body must receive input");
    assert!(contains(8, 47), "Clipping must preserve the original shape");
    assert!(
        !contains(8, 24),
        "Rounded transparent corner must pass input through"
    );
    assert!(!contains(25, 49), "Clipped pixels must pass input through");
}

#[test]
fn reserve_space_updates_are_idempotent_and_preserve_the_surface_and_input_region() {
    let (server_socket, client_socket) = UnixStream::pair().unwrap();
    let mut display = Display::<Server>::new().unwrap();
    let mut handle = display.handle();
    handle.create_global::<Server, wl_compositor::WlCompositor, _>(6, ());
    handle.create_global::<Server, wl_shm::WlShm, _>(1, ());
    handle.create_global::<Server, zwlr_layer_shell_v1::ZwlrLayerShellV1, _>(4, ());
    handle.insert_client(server_socket, Arc::new(())).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let server_done = done.clone();
    let server = std::thread::spawn(move || {
        let mut state = Server::default();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !server_done.load(Ordering::Acquire) {
            display.dispatch_clients(&mut state).unwrap();
            display.flush_clients().unwrap();
            assert!(
                Instant::now() < deadline,
                "Exclusive-zone fixture timed out"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        state
    });

    let fd = client_socket.as_raw_fd();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "wayland::input_tests::exclusive_zone_fixture_client",
        "--ignored",
        "--nocapture",
    ]);
    command.env("WAYLAND_SOCKET", fd.to_string());
    command.env("RIX_SHELL_RENDERER", "software");
    command.env("RIX_ADAPTER_RESERVE_TEST", "1");

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
    let observed = server.join().unwrap();
    assert!(
        output.status.success(),
        "Fixture child failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(observed.exclusive_zones, [50, 46, 50]);
    assert_eq!(observed.layer_namespaces, ["reserve-regression"]);
    assert_eq!(observed.input_regions, [Some(vec![(8, 24, 40, 24)])]);
}

#[test]
#[ignore = "subprocess fixture for the disposable Wayland test"]
fn exclusive_zone_fixture_client() {
    assert_eq!(
        std::env::var("RIX_ADAPTER_RESERVE_TEST").as_deref(),
        Ok("1"),
        "This fixture must only run against its disposable Wayland server"
    );
    slint::slint! {
        export component ReserveProbe inherits Window {
            width: 640px;
            height: 46px;
        }
    }
    let original = crate::LayerSurfaceSpec {
        namespace: "reserve-regression".into(),
        exclusive_zone: 50,
        keyboard_interactive: true,
        ..Default::default()
    };
    let control = super::install(original.clone()).unwrap();
    let _window = ReserveProbe::new().unwrap();
    let adapter = control.0.borrow().adapter.upgrade().unwrap();
    let input = set_surface_input_region(
        adapter.layer.wl_surface(),
        &adapter.compositor,
        Some(&[SurfaceRect {
            x: 8.,
            y: 24.,
            width: 40.,
            height: 24.,
            radius: 0.,
            opacity: 1.,
            clip: None,
        }]),
    )
    .unwrap();
    control.set_exclusive_zone(50);
    control.set_exclusive_zone(46);
    control.set_exclusive_zone(46);
    assert_eq!(control.0.borrow().spec.exclusive_zone, 46);
    control.set_exclusive_zone(50);
    control.set_exclusive_zone(50);
    assert_eq!(control.0.borrow().spec, original);
    let backend = adapter.layer.wl_surface().backend().upgrade().unwrap();
    let connection = Connection::from_backend(backend);
    let mut queue = connection.new_event_queue::<FixtureClient>();
    queue.roundtrip(&mut FixtureClient).unwrap();
    drop(input);
}
