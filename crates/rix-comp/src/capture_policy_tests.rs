use super::*;
use crate::state::ClientState;
use rix_protocols::{
    capture_client::{
        rix_capture_policy_manager_v1 as manager, rix_capture_policy_surface_v1 as policy,
    },
    observer_client::{
        rix_toplevel_observer_manager_v1 as observer_manager, rix_toplevel_observer_v1 as observer,
    },
};
use smithay::{
    backend::{
        egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay},
        renderer::gles::GlesRenderer,
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
};
use std::{
    fs::File,
    io::Write,
    os::{
        fd::AsFd,
        unix::{
            fs::FileExt,
            net::{UnixListener, UnixStream},
        },
    },
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_buffer, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool, wl_subcompositor,
        wl_subsurface, wl_surface,
    },
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::ext::{
    foreign_toplevel_list::v1::client::{
        ext_foreign_toplevel_handle_v1 as toplevel_handle,
        ext_foreign_toplevel_list_v1 as toplevel_list,
    },
    image_capture_source::v1::client::ext_foreign_toplevel_image_capture_source_manager_v1 as source_manager,
    image_capture_source::v1::client::ext_image_capture_source_v1 as image_source,
    image_copy_capture::v1::client::{
        ext_image_copy_capture_frame_v1 as window_frame,
        ext_image_copy_capture_manager_v1 as window_manager,
        ext_image_copy_capture_session_v1 as window_session,
    },
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_plasma::blur::client::{org_kde_kwin_blur, org_kde_kwin_blur_manager};
use wayland_protocols_wlr::{
    layer_shell::v1::client::{zwlr_layer_shell_v1 as layers, zwlr_layer_surface_v1 as layer},
    screencopy::v1::client::{
        zwlr_screencopy_frame_v1 as copy_frame, zwlr_screencopy_manager_v1 as copy_manager,
    },
};

#[derive(Default)]
struct Events {
    applied: Vec<(u32, u32)>,
    observed: Option<(String, i32, i32, i32, i32, bool)>,
    observer_events: usize,
    ready: bool,
    failed: bool,
    handle: Option<toplevel_handle::ExtForeignToplevelHandleV1>,
    window_ready: bool,
    window_stopped: usize,
    window_sizes: Vec<(u32, u32)>,
    window_formats: Vec<wl_shm::Format>,
    window_constraint_batches: Vec<((u32, u32), Vec<wl_shm::Format>)>,
    window_buffer_constraints_failed: usize,
}
impl Dispatch<toplevel_list::ExtForeignToplevelListV1, ()> for Events {
    fn event(
        state: &mut Self,
        _: &toplevel_list::ExtForeignToplevelListV1,
        event: toplevel_list::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let toplevel_list::Event::Toplevel { toplevel } = event {
            state.handle = Some(toplevel);
        }
    }
    wayland_client::event_created_child!(Events, toplevel_list::ExtForeignToplevelListV1, [
        toplevel_list::EVT_TOPLEVEL_OPCODE => (toplevel_handle::ExtForeignToplevelHandleV1, ())
    ]);
}
impl Dispatch<window_session::ExtImageCopyCaptureSessionV1, ()> for Events {
    fn event(
        state: &mut Self,
        _: &window_session::ExtImageCopyCaptureSessionV1,
        event: window_session::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            window_session::Event::Stopped => state.window_stopped += 1,
            window_session::Event::BufferSize { width, height } => {
                state.window_sizes.push((width, height));
            }
            window_session::Event::ShmFormat { format } => {
                if let Ok(format) = format.into_result() {
                    state.window_formats.push(format);
                }
            }
            window_session::Event::Done => {
                if let Some(size) = state.window_sizes.last().copied() {
                    state
                        .window_constraint_batches
                        .push((size, std::mem::take(&mut state.window_formats)));
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<window_frame::ExtImageCopyCaptureFrameV1, ()> for Events {
    fn event(
        state: &mut Self,
        _: &window_frame::ExtImageCopyCaptureFrameV1,
        event: window_frame::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            window_frame::Event::Ready => state.window_ready = true,
            window_frame::Event::Failed { reason } => {
                state.failed = true;
                if matches!(
                    reason.into_result(),
                    Ok(window_frame::FailureReason::BufferConstraints)
                ) {
                    state.window_buffer_constraints_failed += 1;
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Events {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<policy::RixCapturePolicySurfaceV1, ()> for Events {
    fn event(
        state: &mut Self,
        _: &policy::RixCapturePolicySurfaceV1,
        event: policy::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let policy::Event::Applied { serial, policy } = event {
            state.applied.push((serial, policy.into()));
        }
    }
}
impl Dispatch<observer::RixToplevelObserverV1, ()> for Events {
    fn event(
        state: &mut Self,
        _: &observer::RixToplevelObserverV1,
        event: observer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.observer_events += 1;
        state.observed = match event {
            observer::Event::State {
                output,
                x,
                y,
                width,
                height,
                active,
                ..
            } => Some((output, x, y, width, height, active == 1)),
            observer::Event::Unavailable => None,
            _ => return,
        };
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for Events {
    fn event(
        _: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
        }
    }
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Events {
    fn event(
        _: &mut Self,
        base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}
impl Dispatch<layer::ZwlrLayerSurfaceV1, ()> for Events {
    fn event(
        _: &mut Self,
        surface: &layer::ZwlrLayerSurfaceV1,
        event: layer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let layer::Event::Configure { serial, .. } = event {
            surface.ack_configure(serial);
        }
    }
}
impl Dispatch<copy_frame::ZwlrScreencopyFrameV1, ()> for Events {
    fn event(
        state: &mut Self,
        _: &copy_frame::ZwlrScreencopyFrameV1,
        event: copy_frame::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            copy_frame::Event::Ready { .. } => state.ready = true,
            copy_frame::Event::Failed => state.failed = true,
            _ => {}
        }
    }
}
wayland_client::delegate_noop!(Events: ignore manager::RixCapturePolicyManagerV1);
wayland_client::delegate_noop!(Events: ignore observer_manager::RixToplevelObserverManagerV1);
wayland_client::delegate_noop!(Events: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(Events: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(Events: ignore wl_subcompositor::WlSubcompositor);
wayland_client::delegate_noop!(Events: ignore wl_subsurface::WlSubsurface);
wayland_client::delegate_noop!(Events: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(Events: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(Events: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(Events: ignore wl_output::WlOutput);
wayland_client::delegate_noop!(Events: ignore xdg_toplevel::XdgToplevel);
wayland_client::delegate_noop!(Events: ignore layers::ZwlrLayerShellV1);
wayland_client::delegate_noop!(Events: ignore org_kde_kwin_blur_manager::OrgKdeKwinBlurManager);
wayland_client::delegate_noop!(Events: ignore org_kde_kwin_blur::OrgKdeKwinBlur);
wayland_client::delegate_noop!(Events: ignore copy_manager::ZwlrScreencopyManagerV1);
wayland_client::delegate_noop!(Events: ignore toplevel_handle::ExtForeignToplevelHandleV1);
wayland_client::delegate_noop!(Events: ignore source_manager::ExtForeignToplevelImageCaptureSourceManagerV1);
wayland_client::delegate_noop!(Events: ignore image_source::ExtImageCaptureSourceV1);
wayland_client::delegate_noop!(Events: ignore window_manager::ExtImageCopyCaptureManagerV1);

enum Command {
    Inspect(u32, mpsc::Sender<(bool, bool)>),
    Edit(Box<dyn FnOnce(&mut RixState) + Send>, mpsc::Sender<()>),
    Pixels(mpsc::Sender<(image::RgbaImage, image::RgbaImage)>),
    Preview(tokio::sync::oneshot::Sender<Result<rix_ipc::IpcResponse, String>>),
}
struct Server {
    connection: Connection,
    sender: mpsc::Sender<Command>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    socket: std::path::PathBuf,
    _directory: tempfile::TempDir,
}
impl Server {
    fn new(gpu: bool) -> Self {
        let (server_socket, client_socket) = UnixStream::pair().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("wayland-capture-test");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (sender, commands) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut renderer = gpu.then(|| {
                let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
                let context = EGLContext::new(&display).unwrap();
                unsafe { GlesRenderer::new(context) }.unwrap()
            });
            let mut display = wayland_server::Display::<RixState>::new().unwrap();
            let mut state = RixState::new(&mut display, "capture-policy-test".into());
            state.set_server_side_decorations(false);
            state.animation_duration = 0;
            let output = Output::new(
                "test-output".into(),
                PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: Subpixel::Unknown,
                    make: "test".into(),
                    model: "test".into(),
                },
            );
            output.change_current_state(
                Some(Mode {
                    size: (32, 32).into(),
                    refresh: 60_000,
                }),
                None,
                None,
                Some((100, 80).into()),
            );
            output.create_global::<RixState>(&display.handle());
            state.space.map_output(&output, (100, 80));
            let client = display
                .handle()
                .insert_client(
                    server_socket,
                    Arc::new(ClientState {
                        allow_capture: gpu,
                        ..Default::default()
                    }),
                )
                .unwrap();
            let mut scenes = crate::render::SceneRenderer::default();
            let effects = rix_config::appearance::MaterialEffects {
                blur_radius: 6,
                noise_opacity: 0.,
                options: Default::default(),
            };
            let mut cursor = crate::backend::CursorRenderer::new(&Default::default());
            let deadline = Instant::now() + Duration::from_secs(60);
            while !done.load(Ordering::Relaxed) && Instant::now() < deadline {
                while let Ok((socket, _)) = listener.accept() {
                    display
                        .handle()
                        .insert_client(socket, Arc::new(ClientState::default()))
                        .unwrap();
                }
                let _ = display.dispatch_clients(&mut state);
                state.space.refresh();
                crate::toplevel_observer::refresh(&mut state);
                for command in commands.try_iter() {
                    match command {
                        Command::Inspect(id, reply) => {
                            let surface: WlSurface = client
                                .object_from_protocol_id(&display.handle(), id)
                                .unwrap();
                            reply
                                .send((
                                    excluded(&surface),
                                    crate::toplevel_observer::visible(&surface),
                                ))
                                .unwrap();
                        }
                        Command::Edit(edit, reply) => {
                            edit(&mut state);
                            crate::toplevel_observer::refresh(&mut state);
                            reply.send(()).unwrap();
                        }
                        Command::Pixels(reply) => {
                            let renderer = renderer.as_mut().unwrap();
                            let visible = scenes
                                .compose(renderer, &state, &output, None, &effects)
                                .unwrap();
                            let visible =
                                crate::screenshot::capture(renderer, &output, &visible).unwrap();
                            let captured = scenes
                                .compose_capture(renderer, &state, &output, None, &effects)
                                .unwrap();
                            let captured =
                                crate::screenshot::capture(renderer, &output, &captured).unwrap();
                            reply.send((visible, captured)).unwrap();
                        }
                        Command::Preview(reply) => {
                            let id = state.windows.entries.first().unwrap().0;
                            let mut previews = std::mem::take(&mut state.previews);
                            previews.submit(&state, renderer.as_mut().unwrap(), id, 64, 48, reply);
                            state.previews = previews;
                        }
                    }
                }
                if let Some(renderer) = &mut renderer {
                    state.poll_previews(renderer);
                    state.screencopy.poll(renderer);
                    if state.screencopy.needs_frame(&output) {
                        let elements = crate::backend::capture_output_elements(
                            &mut scenes,
                            renderer,
                            &state,
                            &output,
                            None,
                            &effects,
                            &mut cursor,
                        )
                        .unwrap();
                        let no_cursor = elements
                            .iter()
                            .filter(|element| {
                                matches!(element, crate::backend::CaptureElement::Scene(_))
                            })
                            .collect::<Vec<_>>();
                        state.screencopy.render(
                            renderer,
                            &output,
                            &elements,
                            &no_cursor,
                            Duration::ZERO,
                        );
                    }
                    crate::window_capture::render(&mut state, renderer, &mut cursor);
                }
                display.flush_clients().unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        Self {
            connection: Connection::from_socket(client_socket).unwrap(),
            sender,
            stop,
            thread: Some(thread),
            socket,
            _directory: directory,
        }
    }
    fn inspect(&self, surface: &wl_surface::WlSurface) -> (bool, bool) {
        let (reply, rx) = mpsc::channel();
        self.sender
            .send(Command::Inspect(surface.id().protocol_id(), reply))
            .unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap()
    }
    fn edit(&self, edit: impl FnOnce(&mut RixState) + Send + 'static) {
        let (reply, rx) = mpsc::channel();
        self.sender
            .send(Command::Edit(Box::new(edit), reply))
            .unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    fn pixels(&self) -> (image::RgbaImage, image::RgbaImage) {
        let (reply, rx) = mpsc::channel();
        self.sender.send(Command::Pixels(reply)).unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap()
    }
    fn preview(&self) -> Result<rix_ipc::IpcResponse, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.sender.send(Command::Preview(reply)).unwrap();
        rx.blocking_recv().unwrap()
    }
    fn preview_allowed(&self) -> bool {
        self.preview().is_ok()
    }
}

#[test]
#[ignore = "requires EGL and RIX_CAPTURE_PROBE_BINARY pointing to a native Wayland client test binary"]
fn native_client_negotiation_and_capture_use_real_peer_credentials() {
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    struct Probe(Child);
    impl Drop for Probe {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let server = Server::new(true);
    let binary =
        std::env::var_os("RIX_CAPTURE_PROBE_BINARY").expect("Supply the native client test binary");
    let mut probe = Probe(
        Command::new(binary)
            .arg("live_native_protocol_overlay_probe")
            .args(["--ignored", "--nocapture", "--test-threads=1"])
            .env("WAYLAND_DISPLAY", &server.socket)
            .env_remove("WAYLAND_SOCKET")
            .env("XDG_CURRENT_DESKTOP", "CapabilityDiscoveryTest")
            .env("RIX_CAPTURE_PROBE_SYNC", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = probe.0.stdout.take().unwrap();
    let (ready, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if line.contains("NATIVE_CAPTURE_PROBE_READY") {
                ready.send(()).unwrap();
            }
        }
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("Native client did not map its protected overlay");
    let (visible, captured) = server.pixels();
    assert!(
        visible
            .pixels()
            .all(|pixel| pixel[0] > 240 && pixel[2] > 240),
        "Native client marker must appear locally"
    );
    assert!(
        captured
            .pixels()
            .all(|pixel| pixel[0] < 3 && pixel[2] > 240),
        "Native client marker must be omitted from compositor capture"
    );
    probe.0.stdin.take().unwrap().write_all(b"done\n").unwrap();
    assert!(probe.0.wait().unwrap().success());
    reader.join().unwrap();
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn commits_are_atomic_inherited_and_persist_after_destroy() {
    let server = Server::new(false);
    let (globals, mut queue) = registry_queue_init::<Events>(&server.connection).unwrap();
    let qh = queue.handle();
    let mut events = Events::default();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=4, ()).unwrap();
    let subcompositor: wl_subcompositor::WlSubcompositor = globals.bind(&qh, 1..=1, ()).unwrap();
    let manager: manager::RixCapturePolicyManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
    let parent = compositor.create_surface(&qh, ());
    let child = compositor.create_surface(&qh, ());
    let _subsurface = subcompositor.get_subsurface(&child, &parent, &qh, ());
    let policy = manager.get_surface(&child, &qh, ());
    policy.set_policy(policy::Policy::Exclude, 11);
    queue.roundtrip(&mut events).unwrap();
    assert!(
        !server.inspect(&child).0,
        "Uncommitted requests cannot change capture"
    );
    assert!(events.applied.is_empty());
    child.commit();
    queue.roundtrip(&mut events).unwrap();
    assert!(!server.inspect(&child).0, "Sync child waits for its parent");
    assert!(
        events.applied.is_empty(),
        "No premature confirmation of a cached transaction"
    );
    parent.commit();
    queue.roundtrip(&mut events).unwrap();
    assert!(server.inspect(&child).0);
    assert_eq!(events.applied, [(11, 1)]);
    policy.destroy();
    child.commit();
    parent.commit();
    queue.roundtrip(&mut events).unwrap();
    assert!(
        server.inspect(&child).0,
        "Destroying the object must not expose pixels"
    );
    let child_policy = manager.get_surface(&child, &qh, ());
    child_policy.set_policy(policy::Policy::Include, 12);
    child.commit();
    parent.commit();
    queue.roundtrip(&mut events).unwrap();
    assert!(!server.inspect(&child).0);
    let parent_policy = manager.get_surface(&parent, &qh, ());
    parent_policy.set_policy(policy::Policy::Exclude, 13);
    parent.commit();
    queue.roundtrip(&mut events).unwrap();
    assert!(
        server.inspect(&child).0,
        "Include cannot override a protected ancestor"
    );
    parent_policy.set_policy(policy::Policy::Include, 14);
    parent.commit();
    queue.roundtrip(&mut events).unwrap();
    assert!(!server.inspect(&child).0);
}

#[test]
fn duplicate_policy_is_a_protocol_error_without_a_compositor_crash() {
    let server = Server::new(false);
    let (globals, mut queue) = registry_queue_init::<Events>(&server.connection).unwrap();
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=4, ()).unwrap();
    let manager: manager::RixCapturePolicyManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
    let surface = compositor.create_surface(&qh, ());
    let _first = manager.get_surface(&surface, &qh, ());
    let _second = manager.get_surface(&surface, &qh, ());
    assert!(queue.roundtrip(&mut Events::default()).is_err());
    server.edit(|_| {});
}

fn shm_buffer(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<Events>,
    pixels: &[u8],
) -> (File, wl_buffer::WlBuffer) {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(pixels).unwrap();
    let pool = shm.create_pool(file.as_fd(), pixels.len() as i32, qh, ());
    let buffer = pool.create_buffer(0, 32, 32, 128, wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    (file, buffer)
}

#[test]
#[ignore = "requires EGL; run with --ignored"]
fn gpu_window_capture_resizes_with_complete_constraints_and_retained_damage() {
    let server = Server::new(true);
    let (globals, mut queue) = registry_queue_init::<Events>(&server.connection).unwrap();
    let qh = queue.handle();
    let mut events = Events::default();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=4, ()).unwrap();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let wm: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=1, ()).unwrap();
    let _list: toplevel_list::ExtForeignToplevelListV1 = globals.bind(&qh, 1..=1, ()).unwrap();
    let sources: source_manager::ExtForeignToplevelImageCaptureSourceManagerV1 =
        globals.bind(&qh, 1..=1, ()).unwrap();
    let windows: window_manager::ExtImageCopyCaptureManagerV1 =
        globals.bind(&qh, 1..=1, ()).unwrap();
    let window = compositor.create_surface(&qh, ());
    let xdg = wm.get_xdg_surface(&window, &qh, ());
    let _toplevel = xdg.get_toplevel(&qh, ());
    window.commit();
    queue.roundtrip(&mut events).unwrap();
    let (_app_file, app_buffer) = shm_buffer(&shm, &qh, &[255, 0, 0, 255].repeat(32 * 32));
    window.attach(Some(&app_buffer), 0, 0);
    window.commit();
    queue.roundtrip(&mut events).unwrap();
    server.edit(|state| {
        let window = state.windows.entries.first().unwrap().1.clone();
        state.space.map_element(window, (100, 80), true);
    });
    let source = sources.create_source(events.handle.as_ref().unwrap(), &qh, ());
    let session = windows.create_session(&source, window_manager::Options::empty(), &qh, ());
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(
        events.window_constraint_batches.last().unwrap(),
        &((32, 32), vec![wl_shm::Format::Xrgb8888]),
    );
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(&[0; 4096]).unwrap();
    let pool = shm.create_pool(file.as_fd(), 4096, &qh, ());
    let capture_buffer = pool.create_buffer(0, 32, 32, 128, wl_shm::Format::Xrgb8888, &qh, ());
    pool.destroy();
    let frame = session.create_frame(&qh, ());
    frame.attach_buffer(&capture_buffer);
    frame.damage_buffer(0, 0, 32, 32);
    frame.capture();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !events.window_ready && !events.failed && Instant::now() < deadline {
        queue.roundtrip(&mut events).unwrap();
    }
    assert!(
        events.window_ready && !events.failed,
        "Initial frame must complete"
    );
    frame.destroy();

    let mut small_app_file = tempfile::tempfile().unwrap();
    small_app_file
        .write_all(&[0, 255, 0, 255].repeat(16 * 16))
        .unwrap();
    let pool = shm.create_pool(small_app_file.as_fd(), 1024, &qh, ());
    let small_app = pool.create_buffer(0, 16, 16, 64, wl_shm::Format::Argb8888, &qh, ());
    pool.destroy();
    window.attach(Some(&small_app), 0, 0);
    window.damage_buffer(0, 0, 16, 16);
    window.commit();
    queue.roundtrip(&mut events).unwrap();
    events.window_ready = false;
    events.failed = false;
    let frame = session.create_frame(&qh, ());
    frame.attach_buffer(&capture_buffer);
    frame.damage_buffer(0, 0, 32, 32);
    frame.capture();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !events.failed && Instant::now() < deadline {
        queue.roundtrip(&mut events).unwrap();
    }
    assert_eq!(events.window_buffer_constraints_failed, 1);
    assert_eq!(
        events.window_constraint_batches.last().unwrap(),
        &((16, 16), vec![wl_shm::Format::Xrgb8888]),
        "Resize must repeat supported formats, as portal-wlr replaces its constraints",
    );
    frame.destroy();

    let mut small_capture_file = tempfile::tempfile().unwrap();
    small_capture_file.write_all(&[0; 1024]).unwrap();
    let pool = shm.create_pool(small_capture_file.as_fd(), 1024, &qh, ());
    let small_capture = pool.create_buffer(0, 16, 16, 64, wl_shm::Format::Xrgb8888, &qh, ());
    pool.destroy();
    for repeat in 0..2 {
        events.window_ready = false;
        events.failed = false;
        let frame = session.create_frame(&qh, ());
        frame.attach_buffer(&small_capture);
        frame.damage_buffer(0, 0, 32, 32);
        frame.capture();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !events.window_ready && !events.failed && Instant::now() < deadline {
            queue.roundtrip(&mut events).unwrap();
        }
        assert!(
            events.window_ready && !events.failed,
            "Resized frame {repeat} must complete without disconnecting capture",
        );
        let mut pixels = [0; 1024];
        small_capture_file.read_exact_at(&mut pixels, 0).unwrap();
        assert!(pixels
            .chunks_exact(4)
            .all(|pixel| pixel[..3] == [0, 255, 0]));
        frame.destroy();
    }
    assert_eq!(events.window_stopped, 0);
    queue.roundtrip(&mut events).unwrap();
}

#[test]
#[ignore = "requires EGL; run with --ignored"]
fn gpu_capture_omits_overlay_and_its_blur_while_display_keeps_it() {
    let server = Server::new(true);
    let (globals, mut queue) = registry_queue_init::<Events>(&server.connection).unwrap();
    let qh = queue.handle();
    let mut events = Events::default();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=4, ()).unwrap();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let wm: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=1, ()).unwrap();
    let output: wl_output::WlOutput = globals.bind(&qh, 4..=4, ()).unwrap();
    let layers: layers::ZwlrLayerShellV1 = globals.bind(&qh, 4..=4, ()).unwrap();
    let manager: manager::RixCapturePolicyManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
    let observers: observer_manager::RixToplevelObserverManagerV1 =
        globals.bind(&qh, 1..=1, ()).unwrap();
    let blur: org_kde_kwin_blur_manager::OrgKdeKwinBlurManager =
        globals.bind(&qh, 1..=1, ()).unwrap();
    let window = compositor.create_surface(&qh, ());
    let xdg = wm.get_xdg_surface(&window, &qh, ());
    let _toplevel = xdg.get_toplevel(&qh, ());
    window.commit();
    queue.roundtrip(&mut events).unwrap();
    let (_blue_file, blue) = shm_buffer(&shm, &qh, &[255, 0, 0, 255].repeat(32 * 32));
    window.attach(Some(&blue), 0, 0);
    window.commit();
    queue.roundtrip(&mut events).unwrap();
    server.edit(|state| {
        let window = state.windows.entries.first().unwrap().1.clone();
        state.space.map_element(window.clone(), (100, 80), true);
        state.focus_window(&window);
    });
    let observer = observers.observe(&qh, ());
    let make_layer = || {
        let surface = compositor.create_surface(&qh, ());
        let layer = layers.get_layer_surface(
            &surface,
            Some(&output),
            layers::Layer::Overlay,
            "test-generic-overlay".into(),
            &qh,
            (),
        );
        layer.set_size(32, 32);
        layer.set_anchor(layer::Anchor::Top | layer::Anchor::Left);
        layer.set_exclusive_zone(-1);
        (surface, layer)
    };
    let (marker, _marker_layer) = make_layer();
    observer.attach_overlay(&marker);
    marker.commit();
    queue.roundtrip(&mut events).unwrap();
    let (_red_file, red) = shm_buffer(&shm, &qh, &[0, 0, 255, 255].repeat(32 * 32));
    marker.attach(Some(&red), 0, 0);
    marker.commit();
    let (glass, _glass_layer) = make_layer();
    let material = blur.create(&glass, &qh, ());
    material.commit();
    glass.commit();
    queue.roundtrip(&mut events).unwrap();
    let (_clear_file, clear) = shm_buffer(&shm, &qh, &[0; 4096]);
    glass.attach(Some(&clear), 0, 0);
    glass.commit();
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(
        events.observed.as_ref().unwrap(),
        &("test-output".into(), 100, 80, 32, 32, true)
    );
    let (visible, unprotected) = server.pixels();
    assert!(visible.pixels().all(|pixel| pixel[0] > 240));
    assert!(
        unprotected.pixels().all(|pixel| pixel[0] > 240),
        "Control capture must show the marker"
    );
    let policy = manager.get_surface(&marker, &qh, ());
    policy.set_policy(policy::Policy::Exclude, 21);
    marker.commit();
    queue.roundtrip(&mut events).unwrap();
    assert!(events.applied.contains(&(21, 1)));
    let (visible, protected) = server.pixels();
    assert!(
        visible.pixels().all(|pixel| pixel[0] > 240),
        "Local presentation must remain visible"
    );
    assert!(
        protected
            .pixels()
            .all(|pixel| pixel[0] < 3 && pixel[2] > 240),
        "Capture blur must be rebuilt from the blue underlying scene"
    );
    let copier: copy_manager::ZwlrScreencopyManagerV1 = globals.bind(&qh, 3..=3, ()).unwrap();
    let frame = copier.capture_output(0, &output, &qh, ());
    queue.roundtrip(&mut events).unwrap();
    let (file, buffer) = shm_buffer(&shm, &qh, &[0; 4096]);

    let pool = shm.create_pool(file.as_fd(), 4096, &qh, ());
    let capture_buffer = pool.create_buffer(0, 32, 32, 128, wl_shm::Format::Xrgb8888, &qh, ());
    pool.destroy();
    buffer.destroy();
    frame.copy(&capture_buffer);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !events.ready && !events.failed && Instant::now() < deadline {
        queue.roundtrip(&mut events).unwrap();
    }
    assert!(
        events.ready && !events.failed,
        "Actual wlr-screencopy frame must complete"
    );
    let mut bytes = vec![0; 4096];
    file.read_exact_at(&mut bytes, 0).unwrap();
    assert!(bytes
        .chunks_exact(4)
        .all(|pixel| pixel[0] > 240 && pixel[2] < 3));

    let _list: toplevel_list::ExtForeignToplevelListV1 = globals.bind(&qh, 1..=1, ()).unwrap();
    let sources: source_manager::ExtForeignToplevelImageCaptureSourceManagerV1 =
        globals.bind(&qh, 1..=1, ()).unwrap();
    let windows: window_manager::ExtImageCopyCaptureManagerV1 =
        globals.bind(&qh, 1..=1, ()).unwrap();
    queue.roundtrip(&mut events).unwrap();
    let source = sources.create_source(events.handle.as_ref().unwrap(), &qh, ());
    let session = windows.create_session(&source, window_manager::Options::empty(), &qh, ());
    queue.roundtrip(&mut events).unwrap();
    let frame = session.create_frame(&qh, ());
    frame.attach_buffer(&capture_buffer);
    frame.damage_buffer(0, 0, 32, 32);
    frame.capture();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !events.window_ready && !events.failed && Instant::now() < deadline {
        queue.roundtrip(&mut events).unwrap();
    }
    assert!(events.window_ready && !events.failed);
    file.read_exact_at(&mut bytes, 0).unwrap();
    assert!(bytes
        .chunks_exact(4)
        .all(|pixel| pixel[0] > 240 && pixel[2] < 3));
    frame.destroy();
    assert!(server.preview_allowed());
    let window_policy = manager.get_surface(&window, &qh, ());
    window_policy.set_policy(policy::Policy::Exclude, 22);
    window.commit();
    queue.roundtrip(&mut events).unwrap();
    assert!(
        events.window_stopped > 0,
        "Existing capture sessions are revoked on commit"
    );
    let stopped = events.window_stopped;
    let _denied_session =
        windows.create_session(&source, window_manager::Options::empty(), &qh, ());
    queue.roundtrip(&mut events).unwrap();
    assert!(
        events.window_stopped > stopped,
        "A new capture session cannot bypass exclusion"
    );
    assert!(
        !server.preview_allowed(),
        "Panel thumbnails must obey the same policy"
    );
    window_policy.set_policy(policy::Policy::Include, 23);
    window.commit();
    queue.roundtrip(&mut events).unwrap();

    server.edit(|state| state.windows.active = None);
    let (visible, _) = server.pixels();
    assert!(visible.pixels().all(|pixel| pixel[0] < 3 && pixel[2] > 240));
    assert!(!server.inspect(&marker).1);
    queue.roundtrip(&mut events).unwrap();
    let event_count = events.observer_events;
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(
        events.observer_events, event_count,
        "Unchanged geometry must not flood the client"
    );

    let pattern = [
        &[255, 0, 0, 255].repeat(32 * 16)[..],
        &[0, 255, 0, 255].repeat(32 * 16)[..],
    ]
    .concat();
    let (_pattern_file, pattern_buffer) = shm_buffer(&shm, &qh, &pattern);
    window.attach(Some(&pattern_buffer), 0, 0);
    window.damage_buffer(0, 0, 32, 32);
    window.commit();
    queue.roundtrip(&mut events).unwrap();
    let rix_ipc::IpcResponse::WindowPreview {
        width,
        height,
        rgba,
        ..
    } = server.preview().unwrap()
    else {
        panic!("Expected a preview")
    };
    assert_eq!((width, height), (32, 32));
    assert_eq!(&rgba[..4], &[0, 0, 255, 255], "Preview top row");
    assert_eq!(
        &rgba[rgba.len() - 4..],
        &[0, 255, 0, 255],
        "Preview bottom row"
    );
    let session = windows.create_session(&source, window_manager::Options::empty(), &qh, ());
    queue.roundtrip(&mut events).unwrap();
    let frame = session.create_frame(&qh, ());
    events.window_ready = false;
    events.failed = false;
    frame.attach_buffer(&capture_buffer);
    frame.damage_buffer(0, 0, 32, 32);
    frame.capture();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !events.window_ready && !events.failed && Instant::now() < deadline {
        queue.roundtrip(&mut events).unwrap();
    }
    assert!(events.window_ready && !events.failed);
    file.read_exact_at(&mut bytes, 0).unwrap();
    assert_eq!(&bytes[..3], &[255, 0, 0], "Window capture top row");
    assert_eq!(
        &bytes[bytes.len() - 4..bytes.len() - 1],
        &[0, 255, 0],
        "Window capture bottom row"
    );
    frame.destroy();

    for index in 0..12 {
        let color = if index % 2 == 0 {
            [255, 0, 0, 255]
        } else {
            [0, 255, 0, 255]
        };
        let (_file, app_buffer) = shm_buffer(&shm, &qh, &color.repeat(32 * 32));
        window.attach(Some(&app_buffer), 0, 0);
        window.damage_buffer(0, 0, 32, 32);
        window.commit();
        let frame = copier.capture_output(0, &output, &qh, ());
        queue.roundtrip(&mut events).unwrap();
        events.ready = false;
        events.failed = false;
        if index % 2 == 0 {
            frame.copy(&capture_buffer);
        } else {

            frame.copy_with_damage(&capture_buffer);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !events.ready && !events.failed && Instant::now() < deadline {
            queue.roundtrip(&mut events).unwrap();
        }
        assert!(
            events.ready && !events.failed,
            "Streaming frame {index} did not complete"
        );
        file.read_exact_at(&mut bytes, 0).unwrap();
        assert_eq!(
            &bytes[..3],
            &color[..3],
            "Streaming frame {index} used stale pixels"
        );
        frame.destroy();
        events.window_ready = false;
        events.failed = false;
        let window_frame = session.create_frame(&qh, ());
        window_frame.attach_buffer(&capture_buffer);
        window_frame.damage_buffer(0, 0, 32, 32);
        window_frame.capture();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !events.window_ready && !events.failed && Instant::now() < deadline {
            queue.roundtrip(&mut events).unwrap();
        }
        assert!(
            events.window_ready && !events.failed,
            "Window streaming frame {index} did not complete"
        );
        file.read_exact_at(&mut bytes, 0).unwrap();
        assert_eq!(
            &bytes[..3],
            &color[..3],
            "Window streaming frame {index} used stale pixels"
        );
        window_frame.destroy();
    }

    let frame = copier.capture_output(0, &output, &qh, ());
    queue.roundtrip(&mut events).unwrap();
    events.ready = false;
    events.failed = false;
    frame.copy_with_damage(&capture_buffer);
    let idle_deadline = Instant::now() + Duration::from_millis(100);
    while Instant::now() < idle_deadline {
        queue.roundtrip(&mut events).unwrap();
    }
    assert!(
        !events.ready && !events.failed,
        "An unchanged scene must keep the damage-aware request pending"
    );
    let (_file, app_buffer) = shm_buffer(&shm, &qh, &[255, 0, 0, 255].repeat(32 * 32));
    window.attach(Some(&app_buffer), 0, 0);
    window.damage_buffer(0, 0, 32, 32);
    window.commit();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !events.ready && !events.failed && Instant::now() < deadline {
        queue.roundtrip(&mut events).unwrap();
    }
    assert!(
        events.ready && !events.failed,
        "A repaint must resume the pending damage-aware capture"
    );
    file.read_exact_at(&mut bytes, 0).unwrap();
    assert_eq!(&bytes[..3], &[255, 0, 0]);
    frame.destroy();
}
