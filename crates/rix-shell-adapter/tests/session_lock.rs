use std::{
    collections::HashMap,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};
use wayland_protocols::ext::session_lock::v1::server::{
    ext_session_lock_manager_v1 as manager, ext_session_lock_surface_v1 as role,
    ext_session_lock_v1 as lock,
};
use wayland_protocols::wp::{
    fractional_scale::v1::server::{
        wp_fractional_scale_manager_v1 as fractional_manager, wp_fractional_scale_v1 as fractional,
    },
    viewporter::server::{wp_viewport as viewport, wp_viewporter as viewporter},
};
use wayland_server::{
    backend::ObjectId,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_output, wl_region, wl_shm, wl_shm_pool,
        wl_surface,
    },
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New, Resource,
};

#[derive(Clone)]
struct OutputInfo {
    index: u32,
}
#[derive(Clone)]
struct BufferInfo {
    width: i32,
    height: i32,
    stride: i32,
    offset: i32,
    fd: Arc<OwnedFd>,
    format: u32,
}
#[derive(Default)]
struct SurfaceInfo {
    role: Option<role::ExtSessionLockSurfaceV1>,
    output: u32,
    acked: bool,
    expected: (u32, u32),
    scale: i32,
    fractional: u32,
    destination: Option<(i32, i32)>,
    buffer: Option<BufferInfo>,
    callbacks: Vec<wl_callback::WlCallback>,
}
#[derive(Default)]
struct Server {
    surfaces: HashMap<ObjectId, SurfaceInfo>,
    frames: HashMap<u32, usize>,
    lock: Option<lock::ExtSessionLockV1>,
    locked: bool,
    unlocks: usize,
    roles_destroyed: usize,
    resized: bool,
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
                state.surfaces.insert(
                    surface.id(),
                    SurfaceInfo {
                        scale: 1,
                        ..Default::default()
                    },
                );
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
            wl_surface::Request::Attach { buffer, .. } => {
                assert!(
                    info.role.is_some(),
                    "A lock surface must exist before attaching pixels"
                );
                info.buffer = Some(
                    buffer
                        .expect("Lock surfaces must never attach a null buffer")
                        .data::<BufferInfo>()
                        .unwrap()
                        .clone(),
                );
            }
            wl_surface::Request::SetBufferScale { scale } => {
                assert!(scale > 0);
                info.scale = scale;
            }
            wl_surface::Request::Frame { callback } => info.callbacks.push(init.init(callback, ())),
            wl_surface::Request::Commit => {
                assert!(info.acked, "Lock surface committed before ack_configure");
                let buffer = info.buffer.as_ref().expect("Null lock commit");
                assert_eq!(
                    (buffer.width, buffer.height),
                    if info.destination.is_some() {
                        assert_eq!(info.scale, 1);
                        assert_eq!(
                            info.destination,
                            Some((info.expected.0 as i32, info.expected.1 as i32))
                        );
                        (
                            (info.expected.0 as f64 * info.fractional as f64 / 120.).ceil() as i32,
                            (info.expected.1 as f64 * info.fractional as f64 / 120.).ceil() as i32,
                        )
                    } else {
                        (
                            info.expected.0 as i32 * info.scale,
                            info.expected.1 as i32 * info.scale,
                        )
                    }
                );
                assert_eq!(
                    buffer.format,
                    wl_shm::Format::Xrgb8888 as u32,
                    "Lock buffers must remain opaque"
                );

                let offset =
                    buffer.offset + buffer.stride * (buffer.height / 2) + 4 * (buffer.width / 2);
                let mut pixel = [0u8; 4];
                let count = unsafe {
                    libc::pread(
                        buffer.fd.as_raw_fd(),
                        pixel.as_mut_ptr().cast(),
                        4,
                        offset as _,
                    )
                };
                assert_eq!(count, 4);
                assert_eq!(pixel[3], 255);
                *state.frames.entry(info.output).or_default() += 1;
                for callback in info.callbacks.drain(..) {
                    callback.done(1);
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
            format,
        } = request
        {
            init.init(
                id,
                BufferInfo {
                    width,
                    height,
                    stride,
                    offset,
                    fd: fd.clone(),
                    format: format.into(),
                },
            );
        }
    }
}
impl Dispatch<wl_buffer::WlBuffer, BufferInfo> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Request,
        _: &BufferInfo,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}
impl GlobalDispatch<wl_output::WlOutput, OutputInfo> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_output::WlOutput>,
        info: &OutputInfo,
        init: &mut DataInit<'_, Self>,
    ) {
        let output = init.init(resource, info.clone());
        output.geometry(
            0,
            0,
            320,
            200,
            wl_output::Subpixel::Unknown,
            "Rix test".into(),
            "isolated".into(),
            wl_output::Transform::Normal,
        );
        let scale = if info.index == 2 { 2 } else { 1 };
        output.mode(
            wl_output::Mode::Current | wl_output::Mode::Preferred,
            320 * scale,
            200 * scale,
            60_000,
        );
        if output.version() >= 2 {
            output.scale(scale);
        }
        if output.version() >= 4 {
            output.name(format!("TEST-{}", info.index));
            output.description("Disposable test output".into());
        }
        if output.version() >= 2 {
            output.done();
        }
    }
}
impl Dispatch<wl_output::WlOutput, OutputInfo> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_output::WlOutput,
        _: wl_output::Request,
        _: &OutputInfo,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}
impl GlobalDispatch<manager::ExtSessionLockManagerV1, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<manager::ExtSessionLockManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}
impl Dispatch<manager::ExtSessionLockManagerV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &manager::ExtSessionLockManagerV1,
        request: manager::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let manager::Request::Lock { id } = request {
            let lock = init.init(id, ());
            state.lock = Some(lock);
        }
    }
}
impl Dispatch<lock::ExtSessionLockV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &lock::ExtSessionLockV1,
        request: lock::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            lock::Request::GetLockSurface {
                id,
                surface,
                output,
            } => {
                let index = output.data::<OutputInfo>().unwrap().index;
                assert!(
                    !state
                        .surfaces
                        .values()
                        .any(|info| info.output == index && info.role.is_some()),
                    "Duplicate lock output"
                );
                let role = init.init(id, surface.clone());
                let info = state.surfaces.get_mut(&surface.id()).unwrap();
                info.role = Some(role.clone());
                info.output = index;
                info.expected = (320, 200);
                role.configure(index, 320, 200);
            }
            lock::Request::UnlockAndDestroy => {
                assert!(state.locked);
                state.locked = false;
                state.unlocks += 1;
            }
            lock::Request::Destroy => assert!(
                !state.locked,
                "Dropping an active lock must not send destroy"
            ),
            _ => {}
        }
    }
}
impl Dispatch<role::ExtSessionLockSurfaceV1, wl_surface::WlSurface> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &role::ExtSessionLockSurfaceV1,
        request: role::Request,
        surface: &wl_surface::WlSurface,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let info = state.surfaces.get_mut(&surface.id()).unwrap();
        match request {
            role::Request::AckConfigure { serial } => {
                assert_eq!(
                    serial,
                    if info.expected.0 == 420 {
                        400
                    } else {
                        info.output
                    }
                );
                info.acked = true;
            }
            role::Request::Destroy => {
                info.role = None;
                state.roles_destroyed += 1;
            }
            _ => {}
        }
    }
}
impl GlobalDispatch<viewporter::WpViewporter, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<viewporter::WpViewporter>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}
impl Dispatch<viewporter::WpViewporter, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &viewporter::WpViewporter,
        request: viewporter::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let viewporter::Request::GetViewport { id, surface } = request {
            init.init(id, surface);
        }
    }
}
impl Dispatch<viewport::WpViewport, wl_surface::WlSurface> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &viewport::WpViewport,
        request: viewport::Request,
        surface: &wl_surface::WlSurface,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let viewport::Request::SetDestination { width, height } = request {
            state.surfaces.get_mut(&surface.id()).unwrap().destination = Some((width, height));
        }
    }
}
impl GlobalDispatch<fractional_manager::WpFractionalScaleManagerV1, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<fractional_manager::WpFractionalScaleManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}
impl Dispatch<fractional_manager::WpFractionalScaleManagerV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &fractional_manager::WpFractionalScaleManagerV1,
        request: fractional_manager::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let fractional_manager::Request::GetFractionalScale { id, surface } = request {
            let scale = init.init(id, ());
            let info = state.surfaces.get_mut(&surface.id()).unwrap();
            info.fractional = match info.output {
                2 => 240,
                3 => 150,
                _ => 120,
            };
            scale.preferred_scale(info.fractional);
        }
    }
}
impl Dispatch<fractional::WpFractionalScaleV1, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &fractional::WpFractionalScaleV1,
        _: fractional::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

#[test]
fn session_lock_renders_each_output_hotplug_and_fail_closed_exit() {
    for mode in ["unlock", "disconnect"] {
        let (server, child_socket) = UnixStream::pair().unwrap();
        let mut display = Display::<Server>::new().unwrap();
        let mut handle = display.handle();
        handle.create_global::<Server, wl_compositor::WlCompositor, _>(6, ());
        handle.create_global::<Server, wl_shm::WlShm, _>(1, ());
        handle.create_global::<Server, manager::ExtSessionLockManagerV1, _>(1, ());
        if mode == "unlock" {
            handle.create_global::<Server, viewporter::WpViewporter, _>(1, ());
            handle
                .create_global::<Server, fractional_manager::WpFractionalScaleManagerV1, _>(1, ());
        }
        handle.create_global::<Server, wl_output::WlOutput, _>(4, OutputInfo { index: 1 });
        let second =
            handle.create_global::<Server, wl_output::WlOutput, _>(4, OutputInfo { index: 2 });
        handle.insert_client(server, Arc::new(())).unwrap();
        let fd = child_socket.as_raw_fd();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "session_lock_fixture_child", "--nocapture"])
            .env("RIX_LOCK_FIXTURE", mode)
            .env("WAYLAND_SOCKET", fd.to_string())
            .env_remove("WAYLAND_DISPLAY");
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        drop(child_socket);
        let mut state = Server::default();
        let mut third_added = false;
        let mut second_removed = false;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            display.dispatch_clients(&mut state).unwrap();

            if !state.locked
                && state.unlocks == 0
                && state.frames.contains_key(&1)
                && state.frames.contains_key(&2)
            {
                state.lock.as_ref().unwrap().locked();
                state.locked = true;
            }
            if !third_added && state.frames.contains_key(&1) && state.frames.contains_key(&2) {
                handle.create_global::<Server, wl_output::WlOutput, _>(4, OutputInfo { index: 3 });
                third_added = true;
            }
            if !second_removed && state.frames.contains_key(&3) {
                handle.disable_global::<Server>(second.clone());
                second_removed = true;
                let info = state
                    .surfaces
                    .values_mut()
                    .find(|info| info.output == 1)
                    .unwrap();
                info.expected = (420, 200);
                info.acked = false;
                info.role.as_ref().unwrap().configure(400, 420, 200);
                state.resized = true;
            }
            display.flush_clients().unwrap();
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "fixture client failed: {status}");
                break;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("Session lock fixture timed out");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(third_added && second_removed && state.resized);
        assert!(state.frames[&1] >= 2 && state.frames[&2] > 0 && state.frames[&3] > 0);
        assert!(
            state.roles_destroyed >= 1,
            "Removed output did not destroy its lock surface"
        );
        assert_eq!(state.unlocks, usize::from(mode == "unlock"));
        assert_eq!(
            state.locked,
            mode == "disconnect",
            "A non-authenticated exit must leave the compositor locked"
        );
    }
}

#[test]
fn session_lock_fixture_child() {
    let Ok(mode) = std::env::var("RIX_LOCK_FIXTURE") else {
        return;
    };
    use slint::ComponentHandle;
    use std::{cell::RefCell, rc::Rc};
    slint::slint! { export component Probe inherits Window { background: #123456; Text { text: "Rix locked"; color: white; } } }
    let controller = rix_shell_adapter::install_session_lock().unwrap();
    assert_eq!(controller.output_count(), 2);
    assert!(!controller.is_locked());
    assert!(controller.unlock().is_err());
    let windows = Rc::new(RefCell::new(Vec::new()));
    for _ in 0..controller.output_count() {
        let window = Probe::new().unwrap();
        window.show().unwrap();
        windows.borrow_mut().push(window);
    }
    let added = windows.clone();
    controller.on_output_added(move || {
        let window = Probe::new()?;
        window.show()?;
        added.borrow_mut().push(window);
        Ok(())
    });
    let began = Instant::now();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(50),
        move || {
            if began.elapsed() >= Duration::from_millis(700) {
                assert_eq!(windows.borrow().len(), 3);
                assert_eq!(controller.output_count(), 2);
                assert!(controller.is_locked());
                if mode == "unlock" {
                    controller.unlock().unwrap();
                } else {
                    slint::quit_event_loop().unwrap();
                }
            }
        },
    );
    slint::run_event_loop().unwrap();
}
