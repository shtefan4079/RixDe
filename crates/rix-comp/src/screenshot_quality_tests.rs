use super::Writer;
use crate::state::{ClientState, RixState};
use image::{Rgba, RgbaImage};
use smithay::{
    backend::{
        allocator::Fourcc,
        egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay},
        renderer::{element::texture::TextureRenderBuffer, gles::GlesRenderer},
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    utils::Transform,
};
use std::{
    io::Write,
    os::{fd::AsFd, unix::net::UnixStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::{
    wp::viewporter::client::{wp_viewport, wp_viewporter},
    xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
};

struct NativeEvents;
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for NativeEvents {
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
impl Dispatch<xdg_surface::XdgSurface, ()> for NativeEvents {
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
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for NativeEvents {
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
wayland_client::delegate_noop!(NativeEvents: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(NativeEvents: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(NativeEvents: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(NativeEvents: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(NativeEvents: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(NativeEvents: ignore xdg_toplevel::XdgToplevel);
wayland_client::delegate_noop!(NativeEvents: ignore wp_viewporter::WpViewporter);
wayland_client::delegate_noop!(NativeEvents: ignore wp_viewport::WpViewport);

struct NativeClient {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for NativeClient {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn map_native_client(
    display: &mut wayland_server::Display<RixState>,
    state: &mut RixState,
    source: &RgbaImage,
    scale: f64,
) -> NativeClient {
    let (server_socket, client_socket) = UnixStream::pair().unwrap();
    let client = display
        .handle()
        .insert_client(server_socket, Arc::new(ClientState::default()))
        .unwrap();
    let bgra: Vec<_> = source
        .pixels()
        .flat_map(|pixel| [pixel[2], pixel[1], pixel[0], pixel[3]])
        .collect();
    let (ready, mapped) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let client_stop = stop.clone();
    let thread = std::thread::spawn(move || {
        let connection = Connection::from_socket(client_socket).unwrap();
        let (globals, mut queue) = registry_queue_init::<NativeEvents>(&connection).unwrap();
        let qh = queue.handle();
        let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=4, ()).unwrap();
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
        let wm: xdg_wm_base::XdgWmBase = globals.bind(&qh, 3..=6, ()).unwrap();
        let viewporter: wp_viewporter::WpViewporter = globals.bind(&qh, 1..=1, ()).unwrap();
        let surface = compositor.create_surface(&qh, ());
        let xdg = wm.get_xdg_surface(&surface, &qh, ());
        let _toplevel = xdg.get_toplevel(&qh, ());
        let viewport = viewporter.get_viewport(&surface, &qh, ());
        viewport.set_destination(
            (1920. / scale).round() as i32,
            (1080. / scale).round() as i32,
        );
        surface.commit();
        queue.roundtrip(&mut NativeEvents).unwrap();
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&bgra).unwrap();
        let pool = shm.create_pool(file.as_fd(), bgra.len() as i32, &qh, ());
        let buffer = pool.create_buffer(0, 1920, 1080, 1920 * 4, wl_shm::Format::Argb8888, &qh, ());
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, 1920, 1080);
        surface.commit();
        queue.roundtrip(&mut NativeEvents).unwrap();
        ready.send(surface.id().protocol_id()).unwrap();
        while !client_stop.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let surface_id = loop {
        display.dispatch_clients(state).unwrap();
        display.flush_clients().unwrap();
        match mapped.try_recv() {
            Ok(id) => break id,
            Err(mpsc::TryRecvError::Empty) => {
                assert!(
                    Instant::now() < deadline,
                    "Synthetic application failed to map"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("Synthetic application disconnected: {error}"),
        }
    };
    let surface: wayland_server::protocol::wl_surface::WlSurface = client
        .object_from_protocol_id(&display.handle(), surface_id)
        .unwrap();
    let window = state
        .windows
        .entries
        .iter()
        .find(|(_, window)| {
            window
                .toplevel()
                .is_some_and(|toplevel| toplevel.wl_surface() == &surface)
        })
        .unwrap()
        .1
        .clone();
    state.space.map_element(window.clone(), (600, -50), true);
    state.focus_window(&window);
    state.space.refresh();
    NativeClient {
        stop,
        thread: Some(thread),
    }
}

fn sharp_scene() -> RgbaImage {
    RgbaImage::from_fn(1920, 1080, |x, y| {
        if y == 0 {
            Rgba([241, 11, 37, 255])
        } else if y == 1079 {
            Rgba([7, 19, 239, 255])
        } else if x >= 1440 && y >= 780 {
            let value = if (x + y) % 2 == 0 { 0 } else { 255 };
            Rgba([value, value, value, 255])
        } else if x % 17 == 0 {
            Rgba([9, 21, 35, 255])
        } else if y % 29 == 0 {
            Rgba([183, 31, 57, 255])
        } else {
            Rgba([230, 235, 242, 255])
        }
    })
}

#[test]
#[ignore = "requires EGL; run explicitly to verify native GPU screenshot pixels"]
fn native_full_hd_output_capture_and_png_writer_preserve_sharp_pixels() {
    let egl = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
    let context = EGLContext::new(&egl).unwrap();
    let mut renderer = unsafe { GlesRenderer::new(context) }.unwrap();
    let mut display = wayland_server::Display::<RixState>::new().unwrap();
    let mut state = RixState::new(&mut display, "screenshot-quality-test".into());
    state.set_server_side_decorations(false);
    state.animation_duration = 0;
    let output = Output::new(
        "synthetic-full-hd".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "test".into(),
            model: "test".into(),
        },
    );
    output.change_current_state(
        Some(Mode {
            size: (1920, 1080).into(),
            refresh: 60_000,
        }),
        Some(Transform::Normal),
        Some(Scale::Integer(1)),
        Some((600, -50).into()),
    );
    state.space.map_output(&output, (600, -50));
    let source = sharp_scene();
    let wallpaper = TextureRenderBuffer::from_memory(
        &mut renderer,
        source.as_raw(),
        Fourcc::Abgr8888,
        (1920, 1080),
        false,
        1,
        Transform::Normal,
        None,
    )
    .unwrap();
    let mut scenes = crate::render::SceneRenderer::default();
    let mut cursor = crate::backend::CursorRenderer::new(&Default::default());
    let effects = rix_config::appearance::MaterialEffects {
        blur_radius: 0,
        noise_opacity: 0.,
        options: Default::default(),
    };
    let directory = tempfile::tempdir().unwrap();
    let mut writer = Writer::default();

    for (index, scale) in [1., 1.25].into_iter().enumerate() {
        output.change_current_state(None, None, Some(Scale::Fractional(scale)), None);
        state.space.refresh();

        for native in [false, true] {
            let application =
                native.then(|| map_native_client(&mut display, &mut state, &source, scale));
            let scene = crate::backend::capture_output_elements(
                &mut scenes,
                &mut renderer,
                &state,
                &output,
                Some(&wallpaper),
                &effects,
                &mut cursor,
            )
            .unwrap();
            let path = directory
                .path()
                .join(format!("native-{index}-{native}.png"));
            let (reply, mut response) = tokio::sync::oneshot::channel();
            writer.submit(
                &mut renderer,
                &output,
                &scene,
                vec![rix_ipc::ScreenshotCommand {
                    output: Some(output.name()),
                    path: Some(path.to_string_lossy().into_owned()),
                    responder: reply,
                }],
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            let saved = loop {
                writer.poll(&mut renderer, std::slice::from_ref(&output));
                match response.try_recv() {
                    Ok(result) => break result.unwrap(),
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                        assert!(Instant::now() < deadline, "Screenshot writer timed out");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("Screenshot writer disconnected: {error}"),
                }
            };
            assert_eq!(std::path::Path::new(&saved), path);
            let captured = image::open(&path).unwrap().into_rgba8();
            assert_eq!(captured.dimensions(), (1920, 1080));
            for (x, y, actual) in captured.enumerate_pixels() {
                assert_eq!(
                actual,
                source.get_pixel(x, y),
                "GPU screenshot changed native pixel ({x}, {y}) at scale{scale}, application={native}"
            );
            }
            drop(application);
            if native {
                display.dispatch_clients(&mut state).unwrap();
                state.space.refresh();
            }
        }
    }
}
