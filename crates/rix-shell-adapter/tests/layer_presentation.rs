use std::{
    collections::HashMap,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use wayland_protocols_wlr::layer_shell::v1::server::{
    zwlr_layer_shell_v1 as shell, zwlr_layer_surface_v1 as layer,
};
use wayland_server::{
    backend::ObjectId,
    protocol::{wl_buffer, wl_callback, wl_compositor, wl_region, wl_shm, wl_shm_pool, wl_surface},
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New, Resource,
};

#[derive(Clone)]
struct Buffer {
    fd: Arc<OwnedFd>,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
}
impl Buffer {
    fn pixel(&self, x: i32, y: i32) -> [u8; 4] {
        let mut pixel = [0; 4];

        let count = unsafe {
            libc::pread(
                self.fd.as_raw_fd(),
                pixel.as_mut_ptr().cast(),
                4,
                (self.offset + y * self.stride + 4 * x) as _,
            )
        };
        assert_eq!(count, 4);
        pixel
    }
}
#[derive(Default)]
struct Surface {
    role: Option<layer::ZwlrLayerSurfaceV1>,
    configured: bool,
    acked: bool,
    buffer: Option<wl_buffer::WlBuffer>,
    callbacks: Vec<wl_callback::WlCallback>,
    current_layer: Option<shell::Layer>,
    pending_layer: Option<shell::Layer>,
    has_pixels: bool,
}
#[derive(Default)]
struct Server {
    surfaces: HashMap<ObjectId, Surface>,
    frames: Vec<(i32, i32)>,
    callbacks: usize,
    resized: bool,
    fullscreen_transition: bool,
    layer_commits: Vec<shell::Layer>,
    restored_with_pending_callback: bool,
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
                state.surfaces.insert(surface.id(), Surface::default());
            }
            wl_compositor::Request::CreateRegion { id } => {
                init.init(id, ());
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_region::WlRegion, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_region::WlRegion,
        _: wl_region::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}
impl Dispatch<wl_surface::WlSurface, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        surface: &wl_surface::WlSurface,
        request: wl_surface::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        let info = state.surfaces.get_mut(&surface.id()).unwrap();
        match request {
            wl_surface::Request::Attach { buffer, .. } => info.buffer = buffer,
            wl_surface::Request::Frame { callback } => info.callbacks.push(init.init(callback, ())),
            wl_surface::Request::Commit => {
                let Some(role) = &info.role else { return };
                if let Some(layer) = info.pending_layer.take() {
                    let previous = info.current_layer.replace(layer);
                    if state.layer_commits.last() != Some(&layer) {
                        state.layer_commits.push(layer);
                    }
                    if state.fullscreen_transition
                        && previous == Some(shell::Layer::Bottom)
                        && layer == shell::Layer::Top
                    {

                        assert!(info.has_pixels);
                        assert!(info.buffer.is_none());
                        assert_eq!(state.frames.len(), 2);
                        assert_eq!(info.callbacks.len(), 1);
                        state.restored_with_pending_callback = true;
                        for callback in info.callbacks.drain(..) {
                            callback.done(48);
                            state.callbacks += 1;
                        }
                    }
                }
                if !info.configured {
                    info.configured = true;
                    role.configure(1, 1280, 800);
                }
                if let Some(buffer) = info.buffer.take() {
                    assert!(info.acked, "Pixels committed before ack_configure");
                    let pixels = buffer.data::<Buffer>().unwrap();
                    let expected = if state.resized {
                        (640, 480)
                    } else {
                        (1280, 800)
                    };
                    assert_eq!(
                        (pixels.width, pixels.height),
                        expected,
                        "First EGL swap must resize the 1×1 native window"
                    );

                    assert_eq!(pixels.pixel(500, 300), [72, 34, 24, 255]);
                    assert_eq!(
                        pixels.pixel(50, 50),
                        if state.frames.len() == if state.fullscreen_transition { 1 } else { 2 } {
                            [40, 220, 60, 255]
                        } else {
                            [40, 60, 220, 255]
                        },
                        "Layer UI content must be painted, not only its background"
                    );
                    assert_eq!(
                        pixels.pixel(250, 50),
                        [153, 102, 51, 255],
                        "Uploaded Slint image must survive drawable recreation"
                    );
                    state.frames.push(expected);
                    info.has_pixels = true;
                    buffer.release();
                    if !state.fullscreen_transition || state.frames.len() != 2 {
                        for callback in info.callbacks.drain(..) {
                            callback.done(state.frames.len() as u32 * 16);
                            state.callbacks += 1;
                        }
                    }
                    if !state.fullscreen_transition && !state.resized {
                        state.resized = true;
                        info.acked = false;
                        role.configure(2, 640, 480);
                    }
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_callback::WlCallback, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_callback::WlCallback,
        _: wl_callback::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
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
        if let wl_shm::Request::CreatePool { id, fd, .. } = request {
            init.init(id, Arc::new(fd));
        }
    }
}
impl Dispatch<wl_shm_pool::WlShmPool, Arc<OwnedFd>> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_shm_pool::WlShmPool,
        request: wl_shm_pool::Request,
        fd: &Arc<OwnedFd>,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm_pool::Request::CreateBuffer {
            id,
            offset,
            width,
            height,
            stride,
            ..
        } = request
        {
            init.init(
                id,
                Buffer {
                    fd: fd.clone(),
                    offset,
                    width,
                    height,
                    stride,
                },
            );
        }
    }
}
impl Dispatch<wl_buffer::WlBuffer, Buffer> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Request,
        _: &Buffer,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}
impl GlobalDispatch<shell::ZwlrLayerShellV1, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<shell::ZwlrLayerShellV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}
impl Dispatch<shell::ZwlrLayerShellV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &shell::ZwlrLayerShellV1,
        request: shell::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let shell::Request::GetLayerSurface {
            id,
            surface,
            namespace,
            layer,
            ..
        } = request
        {
            assert_eq!(namespace, "capture-presentation-probe");
            let role = init.init(id, surface.clone());
            let info = state.surfaces.get_mut(&surface.id()).unwrap();
            info.role = Some(role);
            info.pending_layer = Some(layer.into_result().unwrap());
        }
    }
}
impl Dispatch<layer::ZwlrLayerSurfaceV1, wl_surface::WlSurface> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &layer::ZwlrLayerSurfaceV1,
        request: layer::Request,
        surface: &wl_surface::WlSurface,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            layer::Request::AckConfigure { .. } => {
                state.surfaces.get_mut(&surface.id()).unwrap().acked = true;
            }
            layer::Request::SetSize { width, height } => assert_eq!((width, height), (0, 0)),
            layer::Request::SetLayer { layer } => {
                state.surfaces.get_mut(&surface.id()).unwrap().pending_layer =
                    Some(layer.into_result().unwrap());
            }
            _ => {}
        }
    }
}

#[test]
#[ignore = "requires Mesa software EGL; never accesses the host display"]
fn egl_layer_commits_visible_pixels_at_configured_size_and_resizes_after_frame() {
    let observed = run_fixture(false);
    assert_eq!(observed.frames, [(1280, 800), (640, 480), (640, 480)]);
    assert_eq!(observed.callbacks, 3);
}

#[test]
#[ignore = "requires Mesa software EGL; never accesses the host display"]
fn fullscreen_panel_restacks_and_repaints_with_a_pending_frame_callback() {
    let observed = run_fixture(true);
    assert_eq!(
        observed.layer_commits,
        [shell::Layer::Top, shell::Layer::Bottom, shell::Layer::Top]
    );
    assert!(observed.restored_with_pending_callback);
    assert_eq!(observed.frames, [(1280, 800); 3]);
    assert_eq!(observed.callbacks, 3);
}

fn run_fixture(fullscreen_transition: bool) -> Server {
    let (server_socket, client_socket) = UnixStream::pair().unwrap();
    let mut display = Display::<Server>::new().unwrap();
    let mut handle = display.handle();
    handle.create_global::<Server, wl_compositor::WlCompositor, _>(6, ());
    handle.create_global::<Server, wl_shm::WlShm, _>(1, ());
    handle.create_global::<Server, shell::ZwlrLayerShellV1, _>(4, ());
    handle.insert_client(server_socket, Arc::new(())).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let server_done = done.clone();
    let server = std::thread::spawn(move || {
        let mut state = Server {
            fullscreen_transition,
            ..Default::default()
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !server_done.load(Ordering::Acquire) {
            display.dispatch_clients(&mut state).unwrap();
            display.flush_clients().unwrap();
            assert!(
                Instant::now() < deadline,
                "Layer presentation fixture timed out"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        state
    });
    let fd = client_socket.as_raw_fd();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "layer_presentation_fixture_client",
        "--ignored",
        "--nocapture",
    ]);
    command.env("WAYLAND_SOCKET", fd.to_string());
    command.env("RIX_LAYER_PRESENTATION_TEST", "1");
    command.env(
        "RIX_LAYER_RETURN_TEST",
        if fullscreen_transition { "1" } else { "0" },
    );
    command.env("LIBGL_ALWAYS_SOFTWARE", "1");
    command.env("MESA_LOADER_DRIVER_OVERRIDE", "swrast");
    command.env_remove("RIX_SHELL_RENDERER");
    command.env_remove("WAYLAND_DISPLAY");

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
    observed
}

#[test]
#[ignore = "subprocess fixture; only runs with the disposable Wayland server"]
fn layer_presentation_fixture_client() {
    assert_eq!(
        std::env::var("RIX_LAYER_PRESENTATION_TEST").as_deref(),
        Ok("1")
    );
    let fullscreen_transition = std::env::var("RIX_LAYER_RETURN_TEST").as_deref() == Ok("1");
    slint::slint! {
        export component Probe inherits Window {
            preferred-width: 1000px;
            preferred-height: 700px;
            background: #182248;
            in property <image> marker;
            in property <bool> after;
            FocusScope {
                width: 100%; height: 100%;
                Rectangle { x: 20px; y: 20px; width: 160px; height: 100px; background: root.after ? #3cdc28 : #dc3c28; }
                Image { x: 200px; y: 20px; width: 100px; height: 100px; source: root.marker; image-fit: fill; }
            }
        }
    }
    let control = rix_shell_adapter::install(rix_shell_adapter::LayerSurfaceSpec {
        namespace: "capture-presentation-probe".into(),
        layer: if fullscreen_transition {
            rix_shell_adapter::ShellLayer::Top
        } else {
            rix_shell_adapter::ShellLayer::Overlay
        },
        exclusive_zone: -1,
        anchors: rix_shell_adapter::ShellAnchors {
            top: true,
            bottom: true,
            left: true,
            right: true,
        },
        keyboard_interactive: true,
        ..Default::default()
    })
    .unwrap();
    if !fullscreen_transition {
        control.set_output_fullscreen(true);
    }
    let window = Probe::new().unwrap();
    use slint::ComponentHandle;
    window.set_marker(slint::Image::from_rgba8(slint::SharedPixelBuffer::<
        slint::Rgba8Pixel,
    >::clone_from_slice(
        &[51, 102, 153, 255], 1, 1
    )));
    window.show().unwrap();
    let weak = window.as_weak();
    let draw_control = control.clone();
    let repaint = slint::Timer::default();
    let mut transition_stage = 0;
    repaint.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(20),
        move || {
            if fullscreen_transition && draw_control.frame_stats().0 == 1 && transition_stage == 0 {
                transition_stage = 1;
                draw_control.set_output_fullscreen(true);
                weak.upgrade().unwrap().set_after(true);
            } else if fullscreen_transition
                && draw_control.frame_stats().0 == 2
                && transition_stage == 1
            {
                transition_stage = 2;
                draw_control.set_output_fullscreen(false);
                weak.upgrade().unwrap().set_after(false);
            } else if !fullscreen_transition && draw_control.frame_stats().0 == 2 {
                weak.upgrade().unwrap().set_after(true);
            } else if draw_control.frame_stats().0 >= 3 {
                slint::quit_event_loop().unwrap();
            }
        },
    );
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::SingleShot, Duration::from_secs(3), || {
        slint::quit_event_loop().unwrap();
    });
    slint::run_event_loop().unwrap();
    assert_eq!(
        window.window().size(),
        if fullscreen_transition {
            slint::PhysicalSize::new(1280, 800)
        } else {
            slint::PhysicalSize::new(640, 480)
        }
    );
    assert_eq!(control.frame_stats().0, 3);
}
