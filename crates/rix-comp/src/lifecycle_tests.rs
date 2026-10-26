use crate::state::{ClientState, RixState};

#[path = "drag_drop_tests.rs"]
mod drag_drop_tests;
#[path = "popup_focus_tests.rs"]
mod popup_focus_tests;
#[path = "startup_scale_tests.rs"]
mod startup_scale_tests;
#[path = "window_move_tests.rs"]
mod window_move_tests;
use std::{
    os::{fd::AsFd, unix::net::UnixStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_subcompositor,
        wl_subsurface, wl_surface,
    },
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::wp::idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1, zwp_idle_inhibitor_v1,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_misc::server_decoration::client::{
    org_kde_kwin_server_decoration as legacy_decoration,
    org_kde_kwin_server_decoration_manager as legacy_manager,
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
wayland_client::delegate_noop!(Events: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for Events {
    fn event(
        _: &mut Self,
        surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = event {
            surface.ack_configure(serial);
        }
    }
}
wayland_client::delegate_noop!(Events: ignore zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1);
wayland_client::delegate_noop!(Events: ignore zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1);

#[derive(Default)]
struct Events {
    offered: Vec<String>,
    selection: Option<wayland_client::protocol::wl_data_offer::WlDataOffer>,
    configures: usize,
    defer_configure_ack: bool,
    last_configure_serial: Option<u32>,
    toplevel_size: (i32, i32),
    fullscreen: bool,
    maximized: bool,
    legacy_modes: Vec<legacy_decoration::Mode>,
    close_requests: usize,
    startup_scale_events: Vec<startup_scale_tests::StartupScaleEvent>,
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
impl Dispatch<xdg_surface::XdgSurface, ()> for Events {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            state
                .startup_scale_events
                .push(startup_scale_tests::StartupScaleEvent::Configured);
            state.last_configure_serial = Some(serial);
            if !state.defer_configure_ack {
                surface.ack_configure(serial);
            }
            state.configures += 1;
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
wayland_client::delegate_noop!(Events: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(Events: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(Events: ignore wl_subcompositor::WlSubcompositor);
wayland_client::delegate_noop!(Events: ignore wl_subsurface::WlSubsurface);
wayland_client::delegate_noop!(Events: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(Events: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(Events: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(Events: ignore wp_viewporter::WpViewporter);
wayland_client::delegate_noop!(Events: ignore wp_viewport::WpViewport);
impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Events {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, xdg_toplevel::Event::Close) {
            state.close_requests += 1;
        }
        if let xdg_toplevel::Event::Configure {
            width,
            height,
            states,
        } = event
        {
            state.toplevel_size = (width, height);
            state.fullscreen = states.chunks_exact(4).any(|bytes| {
                u32::from_ne_bytes(bytes.try_into().unwrap())
                    == xdg_toplevel::State::Fullscreen as u32
            });
            state.maximized = states.chunks_exact(4).any(|bytes| {
                u32::from_ne_bytes(bytes.try_into().unwrap())
                    == xdg_toplevel::State::Maximized as u32
            });
        }
    }
}

#[test]
fn initial_maximize_configures_before_mapping_and_restores_a_client_chosen_size() {
    for server_frame in [false, true] {
        let mut desktop = TestDesktop::new();
        let qh = desktop.queue.handle();
        let surface = desktop.compositor.create_surface(&qh, ());
        let xdg = desktop.wm.get_xdg_surface(&surface, &qh, ());
        let top = xdg.get_toplevel(&qh, ());
        let _decoration = server_frame.then(|| {
            let manager: zxdg_decoration_manager_v1::ZxdgDecorationManagerV1 =
                desktop.globals.bind(&qh, 1..=1, ()).unwrap();
            let decoration = manager.get_toplevel_decoration(&top, &qh, ());
            decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
            decoration
        });
        top.set_maximized();
        surface.commit();
        desktop.roundtrip();
        assert!(
            desktop.events.maximized,
            "Initial configure must include the request"
        );
        assert_eq!(desktop.inspect(|state| state.windows.entries.len()), 0);
        let size = desktop.events.toplevel_size;
        assert_eq!(size.0, 1000);
        let header = desktop.inspect(|state| state.decoration_theme.height);
        assert_eq!(size.1, 700 - if server_frame { header } else { 0 });
        let maximized = desktop.sized_buffer(size.0, size.1);
        xdg.set_window_geometry(0, 0, size.0, size.1);
        surface.attach(Some(&maximized), 0, 0);
        surface.commit();
        desktop.roundtrip();
        desktop.inspect(move |state| {
            let (id, window) = &state.windows.entries[0];
            assert!(state.windows.maximized.contains_key(id));
            assert_eq!(
                state.space.element_location(window),
                Some((0, if server_frame { header } else { 0 }).into())
            );
        });
        desktop.events.defer_configure_ack = true;
        top.unset_maximized();
        desktop.roundtrip();
        assert!(!desktop.events.maximized);
        assert_eq!(
            desktop.events.toplevel_size,
            (0, 0),
            "Client has never provided a normal size"
        );
        xdg.ack_configure(desktop.events.last_configure_serial.unwrap());
        surface.commit();
        desktop.roundtrip();
        desktop.inspect(move |state| {
            let (id, window) = &state.windows.entries[0];
            assert!(
                state.windows.pending_placement.contains_key(id),
                "ACK alone keeps the initial maximized frame intact"
            );
            assert_eq!(
                state.space.element_location(window),
                Some((0, if server_frame { header } else { 0 }).into())
            );
        });
        let normal = desktop.sized_buffer(420, 260);
        xdg.set_window_geometry(0, 0, 420, 260);
        surface.attach(Some(&normal), 0, 0);
        surface.commit();
        desktop.roundtrip();
        desktop.inspect(|state| {
            let (id, window) = &state.windows.entries[0];
            assert!(!state.windows.maximized.contains_key(id));
            assert!(!state.windows.pending_placement.contains_key(id));
            assert_eq!(window.geometry().size, (420, 260).into());
        });
    }
}

#[test]
fn unset_initial_maximize_is_honored_and_remapping_does_not_reuse_the_intent() {
    let mut desktop = TestDesktop::new();
    let qh = desktop.queue.handle();
    let surface = desktop.compositor.create_surface(&qh, ());
    let xdg = desktop.wm.get_xdg_surface(&surface, &qh, ());
    let top = xdg.get_toplevel(&qh, ());
    top.set_maximized();
    top.unset_maximized();
    surface.commit();
    desktop.roundtrip();
    assert!(!desktop.events.maximized);
    surface.attach(Some(&desktop.buffer), 0, 0);
    surface.commit();
    desktop.roundtrip();
    assert!(desktop.inspect(|state| state.windows.maximized.is_empty()));
    surface.attach(None, 0, 0);
    surface.commit();
    desktop.roundtrip();
    top.set_maximized();
    surface.commit();
    desktop.roundtrip();
    assert!(desktop.events.maximized);
    surface.attach(Some(&desktop.sized_buffer(1000, 700)), 0, 0);
    xdg.set_window_geometry(0, 0, 1000, 700);
    surface.commit();
    desktop.roundtrip();
    surface.attach(None, 0, 0);
    surface.commit();
    desktop.roundtrip();
    surface.commit();
    desktop.roundtrip();
    assert!(!desktop.events.maximized);
}
wayland_client::delegate_noop!(Events: ignore zxdg_decoration_manager_v1::ZxdgDecorationManagerV1);
wayland_client::delegate_noop!(Events: ignore zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1);
wayland_client::delegate_noop!(Events: ignore legacy_manager::OrgKdeKwinServerDecorationManager);
impl Dispatch<legacy_decoration::OrgKdeKwinServerDecoration, ()> for Events {
    fn event(
        state: &mut Self,
        _: &legacy_decoration::OrgKdeKwinServerDecoration,
        event: legacy_decoration::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let legacy_decoration::Event::Mode {
            mode: wayland_client::WEnum::Value(mode),
        } = event
        {
            state.legacy_modes.push(mode);
        }
    }
}

#[derive(Default, Clone)]
struct Snapshot {
    idle_inhibited: bool,
    windows: Vec<rix_ipc::WindowInfo>,
    count: usize,
    width: i32,
    active: Option<u64>,
    shifted_geometry_hit_correct: bool,
    titlebar_top: Option<i32>,
    work_area_top: i32,
}

type Inspect = Box<dyn FnOnce(&mut RixState) + Send>;
pub(crate) struct TestDesktop {
    work: std::sync::mpsc::Sender<Inspect>,
    done: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    globals: wayland_client::globals::GlobalList,
    queue: wayland_client::EventQueue<Events>,
    events: Events,
    compositor: wl_compositor::WlCompositor,
    wm: xdg_wm_base::XdgWmBase,
    buffer: wl_buffer::WlBuffer,
    _pool: wl_shm_pool::WlShmPool,
}
impl TestDesktop {
    pub(crate) fn new() -> Self {
        let (server_socket, client_socket) = UnixStream::pair().unwrap();
        let (work, receiver) = std::sync::mpsc::channel::<Inspect>();
        let done = Arc::new(AtomicBool::new(false));
        let server_done = done.clone();
        let thread = std::thread::spawn(move || {
            let mut display = wayland_server::Display::<RixState>::new().unwrap();
            let mut state = RixState::new(&mut display, "window-control-test".into());
            state
                .seat
                .add_keyboard(Default::default(), 500, 25)
                .unwrap();
            let output = smithay::output::Output::new(
                "test".into(),
                smithay::output::PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: smithay::output::Subpixel::Unknown,
                    make: "test".into(),
                    model: "test".into(),
                },
            );
            output.change_current_state(
                Some(smithay::output::Mode {
                    size: (1000, 700).into(),
                    refresh: 60_000,
                }),
                None,
                None,
                Some((0, 0).into()),
            );
            state.space.map_output(&output, (0, 0));
            let _client = display
                .handle()
                .insert_client(server_socket, Arc::new(ClientState::default()))
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !server_done.load(Ordering::Relaxed) && Instant::now() < deadline {
                display.dispatch_clients(&mut state).unwrap();
                state.apply_pending_pointer_constraint_focus();
                while let Ok(inspect) = receiver.try_recv() {
                    inspect(&mut state);
                }
                state.apply_pending_pointer_constraint_focus();
                display.flush_clients().unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let connection = Connection::from_socket(client_socket).unwrap();
        let (globals, queue) = registry_queue_init::<Events>(&connection).unwrap();
        let qh = queue.handle();
        let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=4, ()).unwrap();
        let wm: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=1, ()).unwrap();
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
        let file = tempfile::tempfile().unwrap();
        file.set_len(4096).unwrap();
        let pool = shm.create_pool(file.as_fd(), 4096, &qh, ());
        let buffer = pool.create_buffer(0, 16, 16, 64, wl_shm::Format::Argb8888, &qh, ());
        Self {
            work,
            done,
            thread: Some(thread),
            globals,
            queue,
            events: Events::default(),
            compositor,
            wm,
            buffer,
            _pool: pool,
        }
    }
    pub(crate) fn inspect<T: Send + 'static>(
        &self,
        inspect: impl FnOnce(&mut RixState) -> T + Send + 'static,
    ) -> T {
        self.inspect_with_timeout(Duration::from_secs(2), inspect)
    }
    pub(crate) fn inspect_with_timeout<T: Send + 'static>(
        &self,
        timeout: Duration,
        inspect: impl FnOnce(&mut RixState) -> T + Send + 'static,
    ) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        self.work
            .send(Box::new(move |state| {
                tx.send(inspect(state)).unwrap();
            }))
            .unwrap();
        rx.recv_timeout(timeout).unwrap()
    }
    pub(crate) fn roundtrip(&mut self) {
        self.queue.roundtrip(&mut self.events).unwrap();
    }
    pub(crate) fn sized_buffer(&self, width: i32, height: i32) -> wl_buffer::WlBuffer {
        self.pixel_buffer(width, height, &[])
    }
    pub(crate) fn pixel_buffer(
        &self,
        width: i32,
        height: i32,
        pixels: &[u8],
    ) -> wl_buffer::WlBuffer {
        use std::io::Write;
        let qh = self.queue.handle();
        let shm: wl_shm::WlShm = self.globals.bind(&qh, 1..=1, ()).unwrap();
        let mut file = tempfile::tempfile().unwrap();
        let bytes = width * height * 4;
        file.set_len(bytes as u64).unwrap();
        if !pixels.is_empty() {
            assert_eq!(pixels.len(), bytes as usize);
            file.write_all(pixels).unwrap();
        }
        let pool = shm.create_pool(file.as_fd(), bytes, &qh, ());
        pool.create_buffer(
            0,
            width,
            height,
            width * 4,
            wl_shm::Format::Argb8888,
            &qh,
            (),
        )
    }
    pub(crate) fn viewport(&self, surface: &wl_surface::WlSurface) -> wp_viewport::WpViewport {
        let qh = self.queue.handle();
        let viewporter: wp_viewporter::WpViewporter = self.globals.bind(&qh, 1..=1, ()).unwrap();
        viewporter.get_viewport(surface, &qh, ())
    }
    pub(crate) fn subsurface(
        &self,
        parent: &wl_surface::WlSurface,
    ) -> (wl_surface::WlSurface, wl_subsurface::WlSubsurface) {
        let qh = self.queue.handle();
        let compositor: wl_subcompositor::WlSubcompositor =
            self.globals.bind(&qh, 1..=1, ()).unwrap();
        let surface = self.compositor.create_surface(&qh, ());
        let child = compositor.get_subsurface(&surface, parent, &qh, ());
        (surface, child)
    }
    pub(crate) fn window(
        &mut self,
        name: &str,
    ) -> (
        wl_surface::WlSurface,
        xdg_surface::XdgSurface,
        xdg_toplevel::XdgToplevel,
    ) {
        let qh = self.queue.handle();
        let surface = self.compositor.create_surface(&qh, ());
        let xdg = self.wm.get_xdg_surface(&surface, &qh, ());
        let top = xdg.get_toplevel(&qh, ());
        top.set_title(name.into());
        surface.commit();
        self.roundtrip();
        surface.attach(Some(&self.buffer), 0, 0);
        surface.commit();
        self.roundtrip();
        (surface, xdg, top)
    }
    fn action(&self, action: rix_ipc::WindowAction) -> Vec<rix_ipc::WindowInfo> {
        self.inspect(move |state| {
            state.apply_window_action(action).unwrap();
            state.list_windows()
        })
    }
    fn focus_snapshot(&self) -> (Vec<u64>, Option<u64>, Option<u64>, Vec<u64>) {
        self.inspect(|state| {
            let stack = state
                .space
                .elements()
                .map(|window| {
                    state
                        .windows
                        .entries
                        .iter()
                        .find(|(_, known)| known == window)
                        .unwrap()
                        .0
                })
                .collect();
            let focus = state.seat.get_keyboard().and_then(|k| k.current_focus());
            let keyboard = state.windows.entries.iter().find_map(|(id, window)| {
                (window.toplevel().map(|top| top.wl_surface()) == focus.as_ref())
                    .then_some(*id)
            });
            let activated = state
                .windows
                .entries
                .iter()
                .filter_map(|(id, window)| {
                    window.toplevel()?.with_pending_state(|pending| {
                        pending
                            .states
                            .contains(wayland_protocols::xdg::shell::server::xdg_toplevel::State::Activated)
                            .then_some(*id)
                    })
                })
                .collect();
            (stack, state.windows.active, keyboard, activated)
        })
    }
}
impl Drop for TestDesktop {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {

            let deadline = Instant::now() + Duration::from_millis(100);
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            if thread.is_finished() {
                let result = thread.join();
                if !std::thread::panicking() {
                    result.unwrap();
                }
            }
        }
    }
}

#[test]
fn fullscreen_foreground_tracks_switching_without_exiting_or_minimizing_the_game() {
    use rix_ipc::WindowAction;
    let mut desktop = TestDesktop::new();
    let game = desktop.window("Fullscreen game");
    let _normal = desktop.window("Normal application");
    let (game_id, normal_id) =
        desktop.inspect(|state| (state.windows.entries[0].0, state.windows.entries[1].0));
    game.2.set_fullscreen(None);
    desktop.roundtrip();
    let fullscreen_buffer = desktop.sized_buffer(1000, 700);
    game.0.attach(Some(&fullscreen_buffer), 0, 0);
    game.0.commit();
    desktop.roundtrip();

    let foreground = |windows: &[rix_ipc::WindowInfo], id| {
        windows
            .iter()
            .find(|window| window.id == id)
            .unwrap()
            .foreground_outputs
            .clone()
            .unwrap()
    };
    let windows = desktop.action(WindowAction::Focus(game_id));
    assert_eq!(foreground(&windows, game_id), vec!["test"]);
    assert!(foreground(&windows, normal_id).is_empty());

    let windows = desktop.action(WindowAction::Focus(normal_id));
    assert!(
        windows
            .iter()
            .find(|window| window.id == game_id)
            .unwrap()
            .is_fullscreen
    );
    assert!(
        foreground(&windows, game_id).is_empty(),
        "A background fullscreen game must no longer hide the panel"
    );
    assert_eq!(foreground(&windows, normal_id), vec!["test"]);

    let windows = desktop.action(WindowAction::Focus(game_id));
    assert_eq!(foreground(&windows, game_id), vec!["test"]);
    let windows = desktop.action(WindowAction::Minimize(game_id));
    assert!(foreground(&windows, game_id).is_empty());
    assert_eq!(foreground(&windows, normal_id), vec!["test"]);

    let windows = desktop.action(WindowAction::ShowDesktop);
    assert!(windows
        .iter()
        .all(|window| window.foreground_outputs == Some(vec![])));
}

#[test]
fn fullscreen_foreground_is_per_output_and_geometry_changes_publish_once() {
    let mut desktop = TestDesktop::new();
    let game = desktop.window("Game on first output");
    let _normal = desktop.window("Application on second output");
    game.2.set_fullscreen(None);
    desktop.roundtrip();
    let fullscreen_buffer = desktop.sized_buffer(1000, 700);
    game.0.attach(Some(&fullscreen_buffer), 0, 0);
    game.0.commit();
    desktop.roundtrip();
    let (game_id, normal_id) = desktop.inspect(|state| {
        let output = smithay::output::Output::new(
            "second".into(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        output.change_current_state(
            Some(smithay::output::Mode {
                size: (1000, 700).into(),
                refresh: 60_000,
            }),
            None,
            None,
            Some((1000, 0).into()),
        );
        state.space.map_output(&output, (1000, 0));
        let normal = state.windows.entries[1].1.clone();
        state.space.map_element(normal, (1100, 100), false);
        state.space.refresh();
        (state.windows.entries[0].0, state.windows.entries[1].0)
    });
    let (sender, mut events) = tokio::sync::broadcast::channel(16);
    let windows = desktop.inspect(move |state| {
        state.ipc_event_sender = Some(sender);
        crate::toplevel_observer::refresh(state);
        state.list_windows()
    });
    assert!(matches!(
        events.try_recv(),
        Ok(rix_ipc::IpcEvent::WindowsChanged)
    ));
    let foreground = |windows: &[rix_ipc::WindowInfo], id| {
        windows
            .iter()
            .find(|window| window.id == id)
            .unwrap()
            .foreground_outputs
            .clone()
            .unwrap()
    };
    assert!(
        !windows
            .iter()
            .find(|window| window.id == game_id)
            .unwrap()
            .is_active
    );
    assert_eq!(
        foreground(&windows, game_id),
        vec!["test"],
        "An inactive fullscreen window stays frontmost on its own output"
    );
    assert_eq!(foreground(&windows, normal_id), vec!["second"]);
    desktop.inspect(|state| {
        for _ in 0..5 {
            crate::toplevel_observer::refresh(state);
        }
    });
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));

    let windows = desktop.inspect(move |state| {
        let normal = state
            .windows
            .entries
            .iter()
            .find(|(id, _)| *id == normal_id)
            .unwrap()
            .1
            .clone();
        state.space.map_element(normal, (200, 200), false);

        let windows = state.list_windows();
        crate::toplevel_observer::refresh(state);
        windows
    });
    assert_eq!(foreground(&windows, normal_id), vec!["test"]);
    assert!(foreground(&windows, game_id).is_empty());
    assert!(matches!(
        events.try_recv(),
        Ok(rix_ipc::IpcEvent::WindowsChanged)
    ));
    desktop.inspect(crate::toplevel_observer::refresh);
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));

    let windows = desktop.inspect(move |state| {
        let normal = state
            .windows
            .entries
            .iter()
            .find(|(id, _)| *id == normal_id)
            .unwrap()
            .1
            .clone();
        state.space.map_element(normal, (1100, 100), false);
        crate::toplevel_observer::refresh(state);
        state.list_windows()
    });
    assert_eq!(foreground(&windows, game_id), vec!["test"]);
    assert_eq!(foreground(&windows, normal_id), vec!["second"]);
    assert!(matches!(
        events.try_recv(),
        Ok(rix_ipc::IpcEvent::WindowsChanged)
    ));
}

#[test]
fn fullscreen_exit_preserves_foreground_window_and_stacking() {
    let mut desktop = TestDesktop::new();
    let first = desktop.window("Video browser");
    let _second = desktop.window("Other application");
    let first_id = desktop.inspect(|state| state.windows.entries[0].0);
    desktop.action(rix_ipc::WindowAction::Focus(first_id));
    let before = desktop.focus_snapshot();
    assert_eq!(before.0.last(), Some(&first_id));
    first.2.set_fullscreen(None);
    desktop.roundtrip();
    first.0.commit();
    desktop.roundtrip();
    assert_eq!(desktop.focus_snapshot(), before);
    first.2.unset_fullscreen();
    desktop.roundtrip();
    first.0.commit();
    desktop.roundtrip();
    assert_eq!(
        desktop.focus_snapshot(),
        before,
        "Leaving video fullscreen must keep the browser above other applications"
    );
}

#[test]
fn work_area_reflow_preserves_focus_stacking_and_minimized_windows() {
    let mut desktop = TestDesktop::new();
    let _first = desktop.window("Older foreground application");
    let _second = desktop.window("Newer background application");
    let _third = desktop.window("Minimized application");
    let (first_id, third_id) =
        desktop.inspect(|state| (state.windows.entries[0].0, state.windows.entries[2].0));
    desktop.action(rix_ipc::WindowAction::Minimize(third_id));
    desktop.action(rix_ipc::WindowAction::Focus(first_id));
    let before = desktop.focus_snapshot();
    desktop.inspect(|state| state.reflow_output_layout());
    assert_eq!(desktop.focus_snapshot(), before);

    let qh = desktop.queue.handle();
    let shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 =
        desktop.globals.bind(&qh, 1..=1, ()).unwrap();
    let surface = desktop.compositor.create_surface(&qh, ());
    let panel = shell.get_layer_surface(
        &surface,
        None,
        zwlr_layer_shell_v1::Layer::Top,
        "stacking-reservation-test".into(),
        &qh,
        (),
    );
    panel.set_size(0, 40);
    panel.set_anchor(
        zwlr_layer_surface_v1::Anchor::Top
            | zwlr_layer_surface_v1::Anchor::Left
            | zwlr_layer_surface_v1::Anchor::Right,
    );
    for extent in [52, 40, 52] {
        panel.set_exclusive_zone(extent);
        surface.commit();
        desktop.roundtrip();
        assert_eq!(desktop.focus_snapshot(), before);
        assert_eq!(
            desktop.inspect(|state| state.work_area(state.space.outputs().next().unwrap()).loc.y),
            extent
        );
    }
    panel.destroy();
    surface.destroy();
    desktop.roundtrip();
    assert_eq!(desktop.focus_snapshot(), before);
    assert!(desktop.inspect(move |state| state.windows.minimized.contains_key(&third_id)));
}

#[test]
fn background_and_minimized_fullscreen_requests_do_not_steal_activation() {
    let mut desktop = TestDesktop::new();
    let _first = desktop.window("Foreground application");
    let second = desktop.window("Background application");
    let third = desktop.window("Minimized application");
    let (first_id, third_id) =
        desktop.inspect(|state| (state.windows.entries[0].0, state.windows.entries[2].0));
    desktop.action(rix_ipc::WindowAction::Minimize(third_id));
    desktop.action(rix_ipc::WindowAction::Focus(first_id));
    let before = desktop.focus_snapshot();
    second.2.set_title("Another chat".into());
    second.0.commit();
    desktop.roundtrip();
    assert_eq!(
        desktop.focus_snapshot(),
        before,
        "An ordinary title/content update must not change focus or stacking"
    );
    for window in [&second, &third] {
        window.2.set_fullscreen(None);
        desktop.roundtrip();
        window.0.commit();
        desktop.roundtrip();
        assert_eq!(
            desktop.focus_snapshot(),
            before,
            "A geometry request must not raise or activate a background window"
        );
        window.2.unset_fullscreen();
        desktop.roundtrip();
        window.0.commit();
        desktop.roundtrip();
        assert_eq!(desktop.focus_snapshot(), before);
    }
    assert!(desktop.inspect(move |state| state.windows.minimized.contains_key(&third_id)));
}

#[test]
fn show_desktop_restores_visible_windows_focus_and_stacking() {
    use rix_ipc::WindowAction;
    let mut desktop = TestDesktop::new();
    let _first = desktop.window("A");
    let second = desktop.window("B");
    let _third = desktop.window("C");
    let windows = desktop.inspect(|state| state.list_windows());
    let (a, b, c) = (windows[0].id, windows[1].id, windows[2].id);
    desktop.action(WindowAction::Minimize(c));
    desktop.action(WindowAction::Focus(a));
    let before = desktop.inspect(|state| state.space.elements().cloned().collect::<Vec<_>>());
    assert!(desktop
        .action(WindowAction::ShowDesktop)
        .iter()
        .all(|window| window.is_minimized));
    assert_eq!(desktop.inspect(|state| state.space.elements().count()), 0);
    let restored = desktop.action(WindowAction::ShowDesktop);
    assert!(
        restored
            .iter()
            .find(|window| window.id == a)
            .unwrap()
            .is_active
    );
    assert!(
        !restored
            .iter()
            .find(|window| window.id == b)
            .unwrap()
            .is_minimized
    );
    assert!(
        restored
            .iter()
            .find(|window| window.id == c)
            .unwrap()
            .is_minimized
    );
    assert_eq!(
        desktop.inspect(|state| state.space.elements().cloned().collect::<Vec<_>>()),
        before
    );
    desktop.action(WindowAction::ShowDesktop);
    second.2.destroy();
    second.1.destroy();
    second.0.destroy();
    desktop.roundtrip();
    let _new = desktop.window("D");
    let hidden = desktop.action(WindowAction::ShowDesktop);
    assert!(hidden.iter().all(|w| w.is_minimized));
    let restored = desktop.action(WindowAction::ShowDesktop);
    assert_eq!(restored.len(), 3);
    assert!(
        restored
            .iter()
            .find(|window| window.title == "D")
            .unwrap()
            .is_active
    );
    assert!(
        !restored
            .iter()
            .find(|window| window.id == a)
            .unwrap()
            .is_minimized
    );
    assert!(
        !restored
            .iter()
            .find(|window| window.title == "D")
            .unwrap()
            .is_minimized
    );
    assert!(
        restored
            .iter()
            .find(|window| window.id == c)
            .unwrap()
            .is_minimized
    );
    assert!(desktop.inspect(|state| {
        state.lock.active = true;
        state
            .apply_window_action(WindowAction::ShowDesktop)
            .is_err()
            && state.windows.desktop_restore.is_none()
    }));
}

#[test]
fn gtk_legacy_decoration_negotiates_native_frame_without_double_titlebars() {
    let mut desktop = TestDesktop::new();
    let qh = desktop.queue.handle();
    let manager: legacy_manager::OrgKdeKwinServerDecorationManager =
        desktop.globals.bind(&qh, 1..=1, ()).unwrap();

    let surface = desktop.compositor.create_surface(&qh, ());
    let decoration = manager.create(&surface, &qh, ());
    decoration.request_mode(legacy_decoration::Mode::Server);
    let xdg = desktop.wm.get_xdg_surface(&surface, &qh, ());
    let top = xdg.get_toplevel(&qh, ());
    surface.commit();
    desktop.roundtrip();
    surface.attach(Some(&desktop.buffer), 0, 0);
    surface.commit();
    desktop.roundtrip();
    assert_eq!(
        desktop.events.legacy_modes.last(),
        Some(&legacy_decoration::Mode::Server)
    );
    assert!(
        desktop.inspect(|state| crate::decoration::enabled(state.space.elements().next().unwrap()))
    );
    decoration.request_mode(legacy_decoration::Mode::Client);
    desktop.roundtrip();
    surface.commit();
    desktop.roundtrip();
    assert_eq!(
        desktop.events.legacy_modes.last(),
        Some(&legacy_decoration::Mode::Client)
    );
    assert!(!desktop
        .inspect(|state| crate::decoration::enabled(state.space.elements().next().unwrap())));
    let modes = desktop.events.legacy_modes.len();
    desktop.roundtrip();
    assert_eq!(
        desktop.events.legacy_modes.len(),
        modes,
        "No mode feedback loop"
    );
    decoration.request_mode(legacy_decoration::Mode::None);
    desktop.roundtrip();
    surface.commit();
    desktop.roundtrip();
    assert!(!desktop
        .inspect(|state| crate::decoration::enabled(state.space.elements().next().unwrap())));
    decoration.release();
    top.destroy();
    xdg.destroy();
    surface.destroy();
    desktop.roundtrip();
    assert!(desktop.inspect(|state| state.kde_decorations.is_empty()));

    let orphan = desktop.compositor.create_surface(&qh, ());
    let orphan_decoration = manager.create(&orphan, &qh, ());
    desktop.roundtrip();
    assert_eq!(desktop.inspect(|state| state.kde_decorations.len()), 1);
    orphan.destroy();
    desktop.roundtrip();
    assert!(desktop.inspect(|state| state.kde_decorations.is_empty()));
    orphan_decoration.release();
    desktop.roundtrip();
}

#[test]
fn desktop_drag_ends_on_left_release_and_escape() {
    let mut display = wayland_server::Display::<RixState>::new().unwrap();
    let mut state = RixState::new(&mut display, "selection-test".into());
    let pointer = state.seat.add_pointer();
    state
        .seat
        .add_keyboard(Default::default(), 200, 25)
        .unwrap();
    state.configure_keyboard(Default::default()).unwrap();
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    pointer.motion(
        &mut state,
        None,
        &smithay::input::pointer::MotionEvent {
            location: (300., 200.).into(),
            serial,
            time: 0,
        },
    );
    state.pointer_pressed(0x110, serial, 0);
    assert_eq!(state.selection_start, Some((300., 200.).into()));
    pointer.motion(
        &mut state,
        None,
        &smithay::input::pointer::MotionEvent {
            location: (20., 10.).into(),
            serial,
            time: 1,
        },
    );
    state.pointer_released(0x111);
    assert!(
        state.selection_start.is_some(),
        "An unrelated button must not end the drag"
    );
    state.pointer_released(0x110);
    assert!(state.selection_start.is_none());
    state.pointer_pressed(0x110, serial, 1);
    state.handle_keyboard_key(
        9u32.into(),
        smithay::backend::input::KeyState::Pressed,
        serial,
        2,
        false,
    );
    assert!(
        state.selection_start.is_none(),
        "Escape must cancel the desktop drag"
    );
}

fn decoration_pointer(
    state: &mut RixState,
    position: smithay::utils::Point<f64, smithay::utils::Logical>,
    pressed: Option<bool>,
) {
    let pointer = state.seat.get_pointer().unwrap();
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    pointer.motion(
        state,
        state.surface_under(position),
        &smithay::input::pointer::MotionEvent {
            location: position,
            serial,
            time: 1,
        },
    );
    if let Some(pressed) = pressed {
        pointer.button(
            state,
            &smithay::input::pointer::ButtonEvent {
                serial,
                time: 1,
                button: 0x110,
                state: if pressed {
                    smithay::backend::input::ButtonState::Pressed
                } else {
                    smithay::backend::input::ButtonState::Released
                },
            },
        );
        if pressed {
            state.pointer_pressed(0x110, serial, 1);
        } else {
            state.pointer_released(0x110);
        }
    }
    pointer.frame(state);
}

#[test]
fn top_left_resize_anchors_each_committed_buffer_and_honors_content_geometry() {
    use smithay::{
        input::pointer::{Focus, GrabStartData},
        utils::{Point, Size},
    };
    use wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge;
    for edge in [ResizeEdge::Left, ResizeEdge::Top, ResizeEdge::TopLeft] {
        let left = matches!(edge, ResizeEdge::Left | ResizeEdge::TopLeft);
        let top = matches!(edge, ResizeEdge::Top | ResizeEdge::TopLeft);
        let mut desktop = TestDesktop::new();
        let window = desktop.window("Slow client with CSD shadows");
        let initial = desktop.sized_buffer(320, 240);
        window.1.set_window_geometry(10, 20, 300, 200);
        window.2.set_min_size(200, 120);
        window.0.attach(Some(&initial), 0, 0);
        window.0.commit();
        desktop.roundtrip();
        let origin = Point::from((100, 150));
        let start = Point::from((160., 190.));
        let id = desktop.inspect(move |state| {
            let (id, window) = state.windows.entries[0].clone();
            assert_eq!(window.bbox().size, Size::from((320, 240)));
            assert_eq!(window.geometry().size, Size::from((300, 200)));
            state.space.map_element(window.clone(), origin, false);
            let pointer = state.seat.add_pointer();
            decoration_pointer(state, start, Some(true));
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            pointer.set_grab(
                state,
                crate::grabs::ResizeSurfaceGrab {
                    start_data: GrabStartData {
                        focus: None,
                        button: 0x110,
                        location: start,
                    },
                    initial_window_location: origin,
                    initial_window_size: window.geometry().size,
                    window,
                    edges: edge,
                },
                serial,
                Focus::Clear,
            );
            id
        });
        desktop.events.defer_configure_ack = true;
        desktop
            .inspect(move |state| decoration_pointer(state, start + Point::from((40., 30.)), None));
        desktop.roundtrip();
        let first_serial = desktop.events.last_configure_serial.unwrap();
        let partial: Size<i32, smithay::utils::Logical> =
            (if left { 260 } else { 300 }, if top { 170 } else { 200 }).into();
        assert_eq!(desktop.events.toplevel_size, (partial.w, partial.h));
        desktop.inspect(move |state| {
            assert_eq!(
                state.space.element_location(&state.windows.entries[0].1),
                Some(origin)
            )
        });

        window.1.ack_configure(first_serial);
        window.0.commit();
        desktop.roundtrip();
        desktop.inspect(move |state| {
            assert_eq!(
                state.space.element_location(&state.windows.entries[0].1),
                Some(origin)
            )
        });
        desktop.inspect(move |state| {
            decoration_pointer(state, start + Point::from((500., 400.)), None)
        });
        desktop.roundtrip();
        let final_size: Size<i32, smithay::utils::Logical> =
            (if left { 200 } else { 300 }, if top { 120 } else { 200 }).into();
        assert_eq!(
            desktop.events.toplevel_size,
            (final_size.w, final_size.h),
            "Client minimum size is respected"
        );
        let partial_buffer = desktop.sized_buffer(partial.w + 20, partial.h + 40);
        window.1.set_window_geometry(10, 20, partial.w, partial.h);
        window.0.attach(Some(&partial_buffer), 0, 0);
        window.0.commit();
        desktop.roundtrip();
        desktop.inspect(move |state| {
            let window = &state.windows.entries[0].1;
            let expected = origin
                + Point::from((
                    if left { 300 - partial.w } else { 0 },
                    if top { 200 - partial.h } else { 0 },
                ));
            assert_eq!(
                state.space.element_location(window),
                Some(expected),
                "An older, intermediate configure also preserves the opposite edge"
            );
            assert!(state.windows.resize_placement.contains_key(&id));
        });
        desktop.inspect(move |state| {
            decoration_pointer(state, start + Point::from((500., 400.)), Some(false))
        });
        desktop.roundtrip();
        window
            .1
            .ack_configure(desktop.events.last_configure_serial.unwrap());
        let final_buffer = desktop.sized_buffer(final_size.w + 20, final_size.h + 40);
        window
            .1
            .set_window_geometry(10, 20, final_size.w, final_size.h);
        window.0.attach(Some(&final_buffer), 0, 0);
        window.0.commit();
        desktop.roundtrip();
        desktop.inspect(move |state| {
            let window = &state.windows.entries[0].1;
            let expected = origin
                + Point::from((
                    if left { 300 - final_size.w } else { 0 },
                    if top { 200 - final_size.h } else { 0 },
                ));
            assert_eq!(state.space.element_location(window), Some(expected));
            assert!(!state.windows.resize_placement.contains_key(&id));
        });
    }
}

#[test]
fn maximize_and_restore_apply_position_only_with_the_configured_buffer() {
    use smithay::utils::{Point, Size};
    for scale in [1., 1.5] {
        let mut desktop = TestDesktop::new();
        let window = desktop.window("Asynchronous geometry client");
        let qh = desktop.queue.handle();
        let decorations: zxdg_decoration_manager_v1::ZxdgDecorationManagerV1 =
            desktop.globals.bind(&qh, 1..=1, ()).unwrap();
        decorations
            .get_toplevel_decoration(&window.2, &qh, ())
            .set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
        desktop.roundtrip();
        let shm: wl_shm::WlShm = desktop.globals.bind(&qh, 1..=1, ()).unwrap();
        let make_buffer = |width: i32, height: i32| {
            let file = tempfile::tempfile().unwrap();
            file.set_len((width * height * 4) as u64).unwrap();
            let pool = shm.create_pool(file.as_fd(), width * height * 4, &qh, ());
            pool.create_buffer(
                0,
                width,
                height,
                width * 4,
                wl_shm::Format::Argb8888,
                &qh,
                (),
            )
        };
        let normal = make_buffer(600, 400);
        window.1.set_window_geometry(0, 0, 600, 400);
        window.0.attach(Some(&normal), 0, 0);
        window.0.commit();
        desktop.roundtrip();
        let origin = Point::from((100, 120));
        let id = desktop.inspect(move |state| {
            let output = state.space.outputs().next().unwrap().clone();
            output.change_current_state(
                Some(smithay::output::Mode {
                    size: (1800, 1200).into(),
                    refresh: 60_000,
                }),
                None,
                Some(smithay::output::Scale::Fractional(scale)),
                None,
            );
            state.reflow_output_layout();
            let (id, window) = state.windows.entries[0].clone();
            state.space.map_element(window, origin, false);
            state.space.refresh();
            id
        });
        desktop.events.defer_configure_ack = true;
        desktop.action(rix_ipc::WindowAction::Maximize(id));
        desktop.roundtrip();
        let max_serial = desktop.events.last_configure_serial.unwrap();
        let max_size: Size<i32, smithay::utils::Logical> = desktop.events.toplevel_size.into();
        let max_origin = desktop.inspect(move |state| {
            let window = &state.windows.entries[0].1;
            assert_eq!(state.space.element_location(window), Some(origin));
            assert!(state.decoration_for_window(window).left > 0);
            state.windows.pending_placement[&id].target.loc
        });
        window.0.commit();
        desktop.roundtrip();
        desktop.inspect(move |state| {
            state.reflow_output_layout();
            assert_eq!(
                state.space.element_location(&state.windows.entries[0].1),
                Some(origin)
            );
            assert!(state.windows.pending_placement.contains_key(&id));
        });

        window.1.ack_configure(max_serial);
        window.0.commit();
        desktop.roundtrip();
        desktop.inspect(move |state| {
            assert_eq!(
                state.space.element_location(&state.windows.entries[0].1),
                Some(origin)
            );
            assert!(state.windows.pending_placement.contains_key(&id));
        });
        let maximized = make_buffer(max_size.w, max_size.h);
        window.1.set_window_geometry(0, 0, max_size.w, max_size.h);
        window.0.attach(Some(&maximized), 0, 0);
        window.0.commit();
        desktop.roundtrip();
        desktop.inspect(move |state| {
            let window = &state.windows.entries[0].1;
            assert_eq!(window.geometry().size, max_size);
            if let Some(pending) = state.windows.pending_placement.get(&id) {
                let serial = smithay::wayland::compositor::with_states(
                    window.toplevel().unwrap().wl_surface(),
                    |states| {
                        states
                            .data_map
                            .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                            .unwrap()
                            .lock()
                            .unwrap()
                            .current_serial
                    },
                );
                assert_eq!(
                    (pending.target.size, Some(pending.serial)),
                    (max_size, serial),
                    "Pending target or committed serial changed"
                );
            }
            assert_eq!(state.space.element_location(window), Some(max_origin));
            assert_eq!(state.decoration_for_window(window).left, 0);
            assert!(!state.windows.pending_placement.contains_key(&id));
        });
        desktop.action(rix_ipc::WindowAction::Maximize(id));
        desktop.roundtrip();
        window
            .1
            .ack_configure(desktop.events.last_configure_serial.unwrap());
        window.0.commit();
        desktop.roundtrip();
        desktop.inspect(move |state| {
            state.reflow_output_layout();
            let window = &state.windows.entries[0].1;
            assert_eq!(state.space.element_location(window), Some(max_origin));
            assert_eq!(state.decoration_for_window(window).left, 0);
            assert!(state.windows.pending_placement.contains_key(&id));
        });
        window.1.set_window_geometry(0, 0, 600, 400);
        window.0.attach(Some(&normal), 0, 0);
        window.0.commit();
        desktop.roundtrip();
        desktop.inspect(move |state| {
            let window = &state.windows.entries[0].1;
            assert_eq!(state.space.element_location(window), Some(origin));
            assert!(state.decoration_for_window(window).left > 0);
            assert!(!state.windows.pending_placement.contains_key(&id));
        });

        desktop.action(rix_ipc::WindowAction::Maximize(id));
        desktop.roundtrip();
        let obsolete = desktop.events.last_configure_serial.unwrap();
        desktop.action(rix_ipc::WindowAction::Maximize(id));
        desktop.roundtrip();
        let latest = desktop.events.last_configure_serial.unwrap();
        window.1.ack_configure(obsolete);
        window.1.set_window_geometry(0, 0, max_size.w, max_size.h);
        window.0.attach(Some(&maximized), 0, 0);
        window.0.commit();
        desktop.roundtrip();
        desktop.inspect(move |state| {
            assert_eq!(
                state.space.element_location(&state.windows.entries[0].1),
                Some(origin)
            );
            assert!(state.windows.pending_placement.contains_key(&id));
        });
        window.1.ack_configure(latest);
        window.0.commit();
        desktop.roundtrip();
        desktop.inspect(move |state| assert!(state.windows.pending_placement.contains_key(&id)));
        window.1.set_window_geometry(0, 0, 600, 400);
        window.0.attach(Some(&normal), 0, 0);
        window.0.commit();
        desktop.roundtrip();
        desktop.inspect(move |state| {
            assert_eq!(
                state.space.element_location(&state.windows.entries[0].1),
                Some(origin)
            );
            assert!(!state.windows.pending_placement.contains_key(&id));
        });

        desktop.action(rix_ipc::WindowAction::Maximize(id));
        desktop.roundtrip();
        window
            .1
            .ack_configure(desktop.events.last_configure_serial.unwrap());
        window.1.set_window_geometry(0, 0, max_size.w, max_size.h);
        window.0.attach(Some(&maximized), 0, 0);
        window.0.commit();
        desktop.roundtrip();
        desktop.action(rix_ipc::WindowAction::Maximize(id));
        desktop.roundtrip();
        desktop.action(rix_ipc::WindowAction::Maximize(id));
        desktop.roundtrip();
        window
            .1
            .ack_configure(desktop.events.last_configure_serial.unwrap());
        window.0.commit();
        desktop.roundtrip();
        desktop.inspect(move |state| assert_eq!(state.windows.maximized[&id].0, origin));
        desktop.action(rix_ipc::WindowAction::Maximize(id));
        desktop.roundtrip();
        window
            .1
            .ack_configure(desktop.events.last_configure_serial.unwrap());
        window.1.set_window_geometry(0, 0, 600, 400);
        window.0.attach(Some(&normal), 0, 0);
        window.0.commit();
        desktop.roundtrip();
        desktop.inspect(move |state| {
            assert_eq!(
                state.space.element_location(&state.windows.entries[0].1),
                Some(origin)
            )
        });

        desktop.action(rix_ipc::WindowAction::Maximize(id));
        desktop.roundtrip();
        window
            .1
            .ack_configure(desktop.events.last_configure_serial.unwrap());
        window.1.set_window_geometry(0, 0, max_size.w, max_size.h);
        window.0.attach(Some(&maximized), 0, 0);
        window.0.commit();
        desktop.roundtrip();
        desktop.action(rix_ipc::WindowAction::Maximize(id));
        desktop.roundtrip();
        let constrained = desktop.inspect(move |state| {
            let output = state.space.outputs().next().unwrap().clone();
            output.change_current_state(
                Some(smithay::output::Mode {
                    size: (500, 350).into(),
                    refresh: 60_000,
                }),
                None,
                None,
                None,
            );
            state.reflow_output_layout();
            let pending = &state.windows.pending_placement[&id];
            let outer = state
                .decoration_for_window_layout(&state.windows.entries[0].1)
                .bounds(pending.target);
            let area = state.work_area(&output);
            assert!(outer.loc.x >= area.loc.x && outer.loc.y >= area.loc.y);
            assert!(outer.loc.x + outer.size.w <= area.loc.x + area.size.w);
            assert!(outer.loc.y + outer.size.h <= area.loc.y + area.size.h);
            assert_eq!(
                state.space.element_location(&state.windows.entries[0].1),
                Some(max_origin)
            );
            pending.target
        });
        desktop.roundtrip();
        window
            .1
            .ack_configure(desktop.events.last_configure_serial.unwrap());
        let constrained_buffer = make_buffer(constrained.size.w, constrained.size.h);
        window
            .1
            .set_window_geometry(0, 0, constrained.size.w, constrained.size.h);
        window.0.attach(Some(&constrained_buffer), 0, 0);
        window.0.commit();
        desktop.roundtrip();
        desktop.inspect(move |state| {
            assert_eq!(
                state.space.element_geometry(&state.windows.entries[0].1),
                Some(constrained)
            );
            assert!(!state.windows.pending_placement.contains_key(&id));
        });
    }
}

#[test]
fn decoration_buttons_activate_on_matching_release_and_restore_at_fractional_scales() {
    use crate::decoration::DecorationHit;
    use smithay::utils::{Logical, Point};
    for scale in [1., 1.5, 2.] {
        let mut desktop = TestDesktop::new();
        let window = desktop.window("Decoration controls");
        let qh = desktop.queue.handle();
        let manager: zxdg_decoration_manager_v1::ZxdgDecorationManagerV1 =
            desktop.globals.bind(&qh, 1..=1, ()).unwrap();
        let decoration = manager.get_toplevel_decoration(&window.2, &qh, ());
        decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
        desktop.roundtrip();
        let shm: wl_shm::WlShm = desktop.globals.bind(&qh, 1..=1, ()).unwrap();
        let file = tempfile::tempfile().unwrap();
        file.set_len(600 * 400 * 4).unwrap();
        let pool = shm.create_pool(file.as_fd(), 600 * 400 * 4, &qh, ());
        let buffer = pool.create_buffer(0, 600, 400, 600 * 4, wl_shm::Format::Argb8888, &qh, ());
        window.0.attach(Some(&buffer), 0, 0);
        window.0.commit();
        desktop.roundtrip();
        let (id, location) = desktop.inspect(move |state| {
            state.seat.add_pointer();
            let settings = serde_json::from_str(include_str!(
                "../../../assets/rix/themes/fairywren-dark/theme.json"
            ))
            .unwrap();
            state.set_decoration_theme(&settings);
            let output = state.space.outputs().next().unwrap().clone();
            output.change_current_state(
                Some(smithay::output::Mode {
                    size: (2400, 1600).into(),
                    refresh: 60_000,
                }),
                None,
                Some(smithay::output::Scale::Fractional(scale)),
                None,
            );
            let (id, window) = &state.windows.entries[0];
            (*id, state.space.element_location(window).unwrap())
        });
        let center = |desktop: &TestDesktop, index: usize| -> Point<f64, Logical> {
            desktop.inspect(move |state| {
                let window = &state.windows.entries[0].1;
                let body = state.space.element_geometry(window).unwrap();
                let theme = state.decoration_for_window(window);
                let bounds = theme.bounds(body);
                let rect = theme.button_rect(bounds.size.w, index);
                (bounds.loc + rect.loc).to_f64()
                    + Point::from((rect.size.w as f64 / 2., rect.size.h as f64 / 2.))
            })
        };
        let close = center(&desktop, 2);
        desktop.inspect(move |state| {
            decoration_pointer(state, close, Some(true));
            assert_eq!(
                state.decoration_button_press,
                Some((id, DecorationHit::Close))
            );
            decoration_pointer(state, close - Point::from((0., 100.)), Some(false));
            assert!(state.decoration_button_press.is_none());
        });
        desktop.roundtrip();
        assert_eq!(
            desktop.events.close_requests, 0,
            "dragging away must cancel close at scale {scale}"
        );
        let maximize = center(&desktop, 1);
        desktop.inspect(move |state| {
            decoration_pointer(state, maximize, Some(true));
            assert!(
                !state.windows.maximized.contains_key(&id),
                "mouse-down must not maximize"
            );
            decoration_pointer(state, close, Some(false));
            assert!(
                !state.windows.maximized.contains_key(&id),
                "release on a different control cancels"
            );
            decoration_pointer(state, maximize, Some(true));
            decoration_pointer(state, maximize, Some(false));
            assert!(state.windows.maximized.contains_key(&id));
        });
        desktop.roundtrip();
        let (max_width, max_height) = desktop.events.toplevel_size;
        assert!(max_width > 600 && max_height > 400);
        let max_file = tempfile::tempfile().unwrap();
        let max_bytes = max_width * max_height * 4;
        max_file.set_len(max_bytes as u64).unwrap();
        let max_pool = shm.create_pool(max_file.as_fd(), max_bytes, &qh, ());
        let max_buffer = max_pool.create_buffer(
            0,
            max_width,
            max_height,
            max_width * 4,
            wl_shm::Format::Argb8888,
            &qh,
            (),
        );
        window.1.set_window_geometry(0, 0, max_width, max_height);
        window.0.attach(Some(&max_buffer), 0, 0);
        window.0.commit();
        desktop.roundtrip();
        let maximized_location = desktop.inspect(|state| {
            state
                .space
                .element_location(&state.windows.entries[0].1)
                .unwrap()
        });
        let restore = center(&desktop, 1);
        desktop.inspect(move |state| {
            decoration_pointer(state, restore, Some(true));
            assert!(
                state.windows.maximized.contains_key(&id),
                "restore waits for release"
            );
            decoration_pointer(state, restore, Some(false));
            assert!(!state.windows.maximized.contains_key(&id));
            assert_eq!(
                state.space.element_location(&state.windows.entries[0].1),
                Some(maximized_location)
            );

            state.reflow_output_layout();
            assert_eq!(
                state.space.element_location(&state.windows.entries[0].1),
                Some(maximized_location)
            );
            let top = state.windows.entries[0].1.toplevel().unwrap();
            assert_eq!(
                top.with_pending_state(|pending| pending.size),
                Some((600, 400).into())
            );
        });
        desktop.roundtrip();
        assert_eq!(desktop.events.toplevel_size, (600, 400));

        window.0.commit();
        desktop.roundtrip();
        desktop.inspect(move |state| {
            assert!(state.windows.restoring.contains_key(&id));
            state.reflow_output_layout();
            assert_eq!(
                state.space.element_location(&state.windows.entries[0].1),
                Some(maximized_location)
            );
            assert_eq!(
                state.windows.entries[0]
                    .1
                    .toplevel()
                    .unwrap()
                    .with_pending_state(|pending| pending.size),
                Some((600, 400).into())
            );
        });
        desktop.roundtrip();
        window.1.set_window_geometry(0, 0, 600, 400);
        window.0.attach(Some(&buffer), 0, 0);
        window.0.commit();
        desktop.roundtrip();
        desktop.inspect(move |state| {
            assert!(!state.windows.restoring.contains_key(&id));
            assert_eq!(
                state.windows.entries[0].1.geometry().size,
                (600, 400).into()
            );
            state.reflow_output_layout();
            assert_eq!(
                state.space.element_location(&state.windows.entries[0].1),
                Some(location)
            );
        });
        let minimize = center(&desktop, 0);
        desktop.inspect(move |state| {
            decoration_pointer(state, minimize, Some(true));
            assert!(!state.windows.minimized.contains_key(&id));
            decoration_pointer(state, minimize, Some(false));
            assert!(state.windows.minimized.contains_key(&id));
        });
        desktop.action(rix_ipc::WindowAction::Focus(id));
        let close = center(&desktop, 2);
        desktop.inspect(move |state| {
            decoration_pointer(state, close, Some(true));
            decoration_pointer(state, close, Some(false));
        });
        desktop.roundtrip();
        assert_eq!(
            desktop.events.close_requests, 1,
            "a completed click closes exactly once at scale {scale}"
        );
    }
}

#[test]
fn server_titlebar_drag_moves_window_and_releases_after_edge_preview() {
    use crate::decoration::DecorationHit;
    use smithay::utils::{Logical, Point};

    for scale in [1., 1.25] {
        let mut desktop = TestDesktop::new();
        let target = desktop.window("Synthetic terminal");
        let _background = desktop.window("Synthetic file manager");
        let qh = desktop.queue.handle();
        let manager: zxdg_decoration_manager_v1::ZxdgDecorationManagerV1 =
            desktop.globals.bind(&qh, 1..=1, ()).unwrap();
        let decoration = manager.get_toplevel_decoration(&target.2, &qh, ());
        decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
        desktop.roundtrip();
        let shm: wl_shm::WlShm = desktop.globals.bind(&qh, 1..=1, ()).unwrap();
        let file = tempfile::tempfile().unwrap();
        file.set_len(600 * 400 * 4).unwrap();
        let pool = shm.create_pool(file.as_fd(), 600 * 400 * 4, &qh, ());
        let buffer = pool.create_buffer(0, 600, 400, 600 * 4, wl_shm::Format::Argb8888, &qh, ());
        target.0.attach(Some(&buffer), 0, 0);
        target.0.commit();
        desktop.roundtrip();

        let (id, initial, titlebar) = desktop.inspect(move |state| {
            state.seat.add_pointer();
            let settings = serde_json::from_str(include_str!(
                "../../../assets/rix/themes/fairywren-dark/theme.json"
            ))
            .unwrap();
            state.set_decoration_theme(&settings);
            state.window_snap_settings.maximize_at_top_edge = true;
            let output = state.space.outputs().next().unwrap().clone();
            output.change_current_state(
                Some(smithay::output::Mode {
                    size: (1200, 900).into(),
                    refresh: 60_000,
                }),
                None,
                Some(smithay::output::Scale::Fractional(scale)),
                None,
            );
            state.reflow_output_layout();
            let (id, target) = state.windows.entries[0].clone();
            let background = state.windows.entries[1].1.clone();
            let initial: Point<i32, Logical> = (150, 200).into();
            state.space.map_element(target.clone(), initial, false);
            state.space.map_element(background.clone(), (30, 70), false);
            state.space.refresh();
            state.focus_window(&background);
            let body = state.space.element_geometry(&target).unwrap();
            let theme = state.decoration_for_window(&target);
            let titlebar: Point<f64, Logical> = (
                body.loc.x as f64 + 150.,
                body.loc.y as f64 - theme.height as f64 / 2.,
            )
                .into();
            assert_eq!(
                state.decoration_under(titlebar).map(|(_, hit)| hit),
                Some(DecorationHit::Move),
            );
            assert_ne!(state.windows.active, Some(id));
            (id, initial, titlebar)
        });
        desktop.inspect(move |state| {
            decoration_pointer(state, titlebar, Some(true));
            assert!(state.seat.get_pointer().unwrap().is_grabbed());
            assert_eq!(state.windows.active, Some(id));
        });
        let moved = titlebar + Point::from((45., 30.));
        desktop.inspect(move |state| {
            decoration_pointer(state, moved, None);
            let target = &state.windows.entries[0].1;
            assert_eq!(
                state.space.element_location(target),
                Some(initial + Point::from((45, 30))),
                "the first grabbed pointer motion must move the terminal",
            );
            assert_eq!(
                state.seat.get_keyboard().unwrap().current_focus().as_ref(),
                target.toplevel().map(|top| top.wl_surface()),
            );
        });
        desktop.inspect(move |state| {
            decoration_pointer(state, (300., 1.).into(), None);
            let preview = state.snap_preview.as_ref().unwrap();
            assert!(preview.visible);
            assert_eq!(preview.output_name, "test");
            assert_eq!(
                preview.geometry.size,
                ((1200. / scale) as i32, (900. / scale) as i32).into(),
            );
        });
        desktop.inspect(move |state| {
            decoration_pointer(state, moved, None);
            assert!(!state.snap_preview.as_ref().unwrap().visible);
            decoration_pointer(state, moved, Some(false));
            assert!(!state.seat.get_pointer().unwrap().is_grabbed());
            assert!(state.pressed_buttons.is_empty());
            assert!(!state.windows.maximized.contains_key(&id));
            assert_eq!(state.windows.active, Some(id));
            assert_eq!(
                state.space.element_location(&state.windows.entries[0].1),
                Some(initial + Point::from((45, 30))),
            );

            decoration_pointer(state, moved + Point::from((20., 20.)), None);
            assert_eq!(
                state.space.element_location(&state.windows.entries[0].1),
                Some(initial + Point::from((45, 30))),
            );
            assert_eq!(
                state.seat.get_keyboard().unwrap().current_focus().as_ref(),
                state.windows.entries[0]
                    .1
                    .toplevel()
                    .map(|top| top.wl_surface()),
                "terminal keyboard focus survives dragging and release",
            );
        });
        desktop.roundtrip();
        assert_eq!(desktop.events.close_requests, 0);
    }
}

#[test]
fn content_controls_mapping_and_subsurface_bounds() {
    let (server_socket, client_socket) = UnixStream::pair().unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let server_done = done.clone();
    let snapshot = Arc::new(Mutex::new(Snapshot::default()));
    let server_snapshot = snapshot.clone();
    let server = std::thread::spawn(move || {
        let mut display = wayland_server::Display::<RixState>::new().unwrap();
        let mut state = RixState::new(&mut display, "lifecycle-test".into());
        let output = smithay::output::Output::new(
            "test".into(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        output.change_current_state(
            Some(smithay::output::Mode {
                size: (1000, 700).into(),
                refresh: 60_000,
            }),
            None,
            None,
            Some((0, 0).into()),
        );
        state.space.map_output(&output, (0, 0));
        let _client = display
            .handle()
            .insert_client(server_socket, Arc::new(ClientState::default()))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !server_done.load(Ordering::Relaxed) && Instant::now() < deadline {
            display.dispatch_clients(&mut state).unwrap();
            let shifted_geometry_hit_correct =
                state.space.elements().next().is_some_and(|window| {
                    let geometry = window.geometry();
                    if geometry.loc.x == 0 {
                        return false;
                    }
                    let Some(location) = state.space.element_location(window) else {
                        return false;
                    };
                    let origin = location - geometry.loc;
                    let sample = (origin.x as f64 + 1.5, origin.y as f64 + 1.5).into();
                    state
                        .surface_under(sample)
                        .is_some_and(|(_, returned_origin)| returned_origin == origin.to_f64())
                });
            *server_snapshot.lock().unwrap() = Snapshot {
                idle_inhibited: state.idle_inhibited(),
                windows: state.list_windows(),
                count: state.space.elements().count(),
                width: state
                    .space
                    .elements()
                    .next()
                    .map(|w| w.bbox().size.w)
                    .unwrap_or(0),
                active: state.windows.active,
                shifted_geometry_hit_correct,
                titlebar_top: state.space.elements().next().and_then(|window| {
                    state.space.element_location(window).map(|location| {
                        location.y
                            + state
                                .decoration_theme
                                .for_window(window)
                                .bounds(window.geometry())
                                .loc
                                .y
                    })
                }),
                work_area_top: state.work_area(&output).loc.y,
            };
            display.flush_clients().unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
    });
    let connection = Connection::from_socket(client_socket).unwrap();
    let (globals, mut queue) = registry_queue_init::<Events>(&connection).unwrap();
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=4, ()).unwrap();
    let subcompositor: wl_subcompositor::WlSubcompositor = globals.bind(&qh, 1..=1, ()).unwrap();
    let wm: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=1, ()).unwrap();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = globals.bind(&qh, 1..=4, ()).unwrap();
    let idle: zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1 =
        globals.bind(&qh, 1..=1, ()).unwrap();
    let decoration_manager: zxdg_decoration_manager_v1::ZxdgDecorationManagerV1 =
        globals.bind(&qh, 1..=1, ()).unwrap();
    let mut events = Events::default();
    let surface = compositor.create_surface(&qh, ());
    let inhibitor_a = idle.create_inhibitor(&surface, &qh, ());
    let inhibitor_b = idle.create_inhibitor(&surface, &qh, ());
    let xdg = wm.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    let decoration = decoration_manager.get_toplevel_decoration(&toplevel, &qh, ());
    decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
    toplevel.set_title("Initial".into());
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(
        events.configures, 0,
        "Decoration creation must not configure before the initial commit"
    );
    surface.commit();
    queue.roundtrip(&mut events).unwrap();
    assert!(
        !snapshot.lock().unwrap().idle_inhibited,
        "An unmapped surface cannot prevent auto-lock"
    );
    assert!(events.configures > 0);
    assert_eq!(
        snapshot.lock().unwrap().count,
        0,
        "Bufferless window must not be mapped"
    );

    let file = tempfile::tempfile().unwrap();
    file.set_len(256).unwrap();
    let pool = shm.create_pool(file.as_fd(), 256, &qh, ());
    let small = pool.create_buffer(0, 4, 4, 16, wl_shm::Format::Argb8888, &qh, ());
    let wide = pool.create_buffer(64, 8, 4, 32, wl_shm::Format::Argb8888, &qh, ());
    surface.attach(Some(&small), 0, 0);
    surface.commit();
    queue.roundtrip(&mut events).unwrap();
    let first = snapshot.lock().unwrap().clone();
    assert_eq!(first.count, 1);
    assert_eq!(first.windows.len(), 1);
    assert_eq!(first.active, Some(first.windows[0].id));
    assert!(
        first.idle_inhibited,
        "A mapped video surface can inhibit idle lock"
    );
    xdg.set_window_geometry(1, 0, 3, 4);
    surface.commit();
    queue.roundtrip(&mut events).unwrap();
    assert!(
        snapshot.lock().unwrap().shifted_geometry_hit_correct,
        "Pointer coordinates must use the surface origin when window geometry excludes a shadow"
    );
    xdg.set_window_geometry(0, 0, 4, 4);
    surface.commit();
    queue.roundtrip(&mut events).unwrap();
    inhibitor_a.destroy();
    queue.roundtrip(&mut events).unwrap();
    assert!(
        snapshot.lock().unwrap().idle_inhibited,
        "The second inhibitor still applies"
    );
    toplevel.set_title("Updated".into());
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(snapshot.lock().unwrap().windows[0].title, "Updated");

    let panel_surface = compositor.create_surface(&qh, ());
    let panel = layer_shell.get_layer_surface(
        &panel_surface,
        None,
        zwlr_layer_shell_v1::Layer::Top,
        "work-area-fixture".into(),
        &qh,
        (),
    );
    panel.set_size(0, 40);
    panel.set_anchor(
        zwlr_layer_surface_v1::Anchor::Top
            | zwlr_layer_surface_v1::Anchor::Left
            | zwlr_layer_surface_v1::Anchor::Right,
    );
    panel.set_exclusive_zone(120);
    panel_surface.commit();
    queue.roundtrip(&mut events).unwrap();
    let reserved = snapshot.lock().unwrap().clone();
    assert_eq!(reserved.work_area_top, 120);
    assert!(
        reserved.titlebar_top.unwrap() >= 120,
        "New panel covers titlebar"
    );
    panel.set_exclusive_zone(180);
    panel_surface.commit();
    queue.roundtrip(&mut events).unwrap();
    let expanded = snapshot.lock().unwrap().clone();
    assert_eq!(expanded.work_area_top, 180);
    assert!(
        expanded.titlebar_top.unwrap() >= 180,
        "Updated reservation covers titlebar"
    );
    panel.destroy();
    panel_surface.destroy();
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(snapshot.lock().unwrap().work_area_top, 0);

    toplevel.set_fullscreen(None);
    queue.roundtrip(&mut events).unwrap();
    assert!(events.fullscreen);
    assert_eq!(events.toplevel_size, (1000, 700));
    assert!(snapshot.lock().unwrap().windows[0].is_fullscreen);
    surface.commit();
    queue.roundtrip(&mut events).unwrap();
    toplevel.unset_fullscreen();
    queue.roundtrip(&mut events).unwrap();
    assert!(!events.fullscreen);
    assert_eq!(
        events.toplevel_size,
        (4, 4),
        "Leaving fullscreen restores the previous size"
    );
    assert!(!snapshot.lock().unwrap().windows[0].is_fullscreen);

    let child = compositor.create_surface(&qh, ());
    let subsurface = subcompositor.get_subsurface(&child, &surface, &qh, ());
    subsurface.set_position(8, 0);
    child.attach(Some(&small), 0, 0);
    child.commit();
    surface.commit();
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(snapshot.lock().unwrap().width, 12);
    subsurface.set_desync();
    child.attach(Some(&wide), 0, 0);
    child.commit();
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(
        snapshot.lock().unwrap().width,
        16,
        "Desynchronized child must update the window bounds"
    );

    surface.attach(None, 0, 0);
    surface.commit();
    queue.roundtrip(&mut events).unwrap();
    let hidden = snapshot.lock().unwrap().clone();
    assert_eq!(hidden.count, 0);
    assert!(hidden.windows.is_empty() && hidden.active.is_none());
    let before = events.configures;
    surface.commit();
    queue.roundtrip(&mut events).unwrap();
    assert!(
        events.configures > before,
        "Remapping requires a new configure"
    );
    surface.attach(Some(&small), 0, 0);
    surface.commit();
    queue.roundtrip(&mut events).unwrap();
    let reopened = snapshot.lock().unwrap().clone();
    assert_eq!(reopened.count, 1);
    assert_ne!(reopened.windows[0].id, first.windows[0].id);
    assert_eq!(reopened.active, Some(reopened.windows[0].id));
    toplevel.set_minimized();
    queue.roundtrip(&mut events).unwrap();
    let minimized = snapshot.lock().unwrap().clone();
    assert_eq!(minimized.count, 0);
    assert!(minimized.windows[0].is_minimized);
    assert!(
        !minimized.idle_inhibited,
        "A minimized application cannot prevent auto-lock"
    );
    inhibitor_b.destroy();

    surface.commit();
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(snapshot.lock().unwrap().count, 0);
    subsurface.destroy();
    child.destroy();
    decoration.destroy();
    toplevel.destroy();
    xdg.destroy();
    surface.destroy();
    queue.roundtrip(&mut events).unwrap();
    assert!(snapshot.lock().unwrap().windows.is_empty());
    done.store(true, Ordering::Relaxed);
    server.join().unwrap();
}

use wayland_client::protocol::{
    wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_seat,
};
wayland_client::delegate_noop!(Events: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(Events: ignore wl_data_device_manager::WlDataDeviceManager);
impl Dispatch<wl_data_source::WlDataSource, ()> for Events {
    fn event(
        _: &mut Self,
        _: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_source::Event::Send { fd, .. } = event {
            use std::io::Write;
            std::fs::File::from(fd)
                .write_all(b"persistent Wayland fixture")
                .unwrap();
        }
    }
}
impl Dispatch<wl_data_offer::WlDataOffer, ()> for Events {
    fn event(
        state: &mut Self,
        _: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            state.offered.push(mime_type);
        }
    }
}
impl Dispatch<wl_data_device::WlDataDevice, ()> for Events {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_device::Event::Selection { id } = event {
            state.selection = id;
        }
    }
    wayland_client::event_created_child!(Events, wl_data_device::WlDataDevice, [0 => (wl_data_offer::WlDataOffer, ())]);
}
#[test]
fn clipboard_preserves_live_formats_and_serves_history_over_wayland() {
    use std::io::Read;
    let (server_socket, client_socket) = UnixStream::pair().unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let stop = done.clone();
    let (commands, requests) = std::sync::mpsc::channel();
    let (replies, responses) = std::sync::mpsc::channel();
    let fixture = tempfile::tempdir().unwrap();
    let config = rix_config::ConfigManager::init_in(fixture.path().join("config")).unwrap();
    let server = std::thread::spawn(move || {
        let mut display = wayland_server::Display::<RixState>::new().unwrap();
        let mut state = RixState::new(&mut display, "clipboard-test".into());
        state
            .seat
            .add_keyboard(Default::default(), 500, 30)
            .unwrap();
        state.configure_keyboard(Default::default()).unwrap();
        let _client = display
            .handle()
            .insert_client(server_socket, Arc::new(ClientState::default()))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut png_reply: Option<
            tokio::sync::oneshot::Receiver<Result<rix_ipc::IpcResponse, String>>,
        > = None;
        while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            display.dispatch_clients(&mut state).unwrap();
            state.tick_clipboard(&config);
            for request in requests.try_iter() {
                if let rix_ipc::IpcRequest::ClipboardSetPng { data } = request {
                    let (sender, receiver) = tokio::sync::oneshot::channel();
                    state.set_png_clipboard(data, sender);
                    png_reply = Some(receiver);
                } else {
                    replies.send(state.clipboard_request(request)).unwrap();
                }
            }
            if let Some(receiver) = png_reply.as_mut() {
                if let Ok(result) = receiver.try_recv() {
                    replies.send(result).unwrap();
                    png_reply = None;
                }
            }
            display.flush_clients().unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
    });
    let connection = Connection::from_socket(client_socket).unwrap();
    let (globals, mut queue) = registry_queue_init::<Events>(&connection).unwrap();
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=4, ()).unwrap();
    let wm: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=1, ()).unwrap();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).unwrap();
    let manager: wl_data_device_manager::WlDataDeviceManager =
        globals.bind(&qh, 3..=3, ()).unwrap();
    let device = manager.get_data_device(&seat, &qh, ());
    let mut events = Events::default();
    let surface = compositor.create_surface(&qh, ());
    let xdg = wm.get_xdg_surface(&surface, &qh, ());
    let _toplevel = xdg.get_toplevel(&qh, ());
    surface.commit();
    queue.roundtrip(&mut events).unwrap();
    let file = tempfile::tempfile().unwrap();
    file.set_len(64).unwrap();
    let pool = shm.create_pool(file.as_fd(), 64, &qh, ());
    let buffer = pool.create_buffer(0, 4, 4, 16, wl_shm::Format::Argb8888, &qh, ());
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    queue.roundtrip(&mut events).unwrap();
    let source = manager.create_data_source(&qh, ());
    source.offer("text/plain;charset=utf-8".into());
    source.offer("text/html".into());
    device.set_selection(Some(&source), 0);
    let mut id = None;
    for _ in 0..100 {
        queue.roundtrip(&mut events).unwrap();
        commands.send(rix_ipc::IpcRequest::GetClipboard).unwrap();
        if let Ok(rix_ipc::IpcResponse::Clipboard { entries, .. }) =
            responses.recv_timeout(Duration::from_secs(2)).unwrap()
        {
            if let Some(entry) = entries.first() {
                assert_eq!(entry.label, "persistent Wayland fixture");
                id = Some(entry.id);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let id = id.expect("clipboard capture did not complete");
    assert!(
        events.offered.iter().any(|mime| mime == "text/html"),
        "Original rich clipboard formats must survive capture"
    );
    events.offered.clear();
    commands
        .send(rix_ipc::IpcRequest::ClipboardSelect(id))
        .unwrap();
    assert_eq!(responses.recv().unwrap().unwrap(), rix_ipc::IpcResponse::Ok);
    queue.roundtrip(&mut events).unwrap();
    assert!(events.offered.iter().any(|mime| mime == "text/plain"));
    let (mut reader, writer) = UnixStream::pair().unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    events
        .selection
        .as_ref()
        .unwrap()
        .receive("text/plain".into(), writer.as_fd());
    drop(writer);
    connection.flush().unwrap();
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"persistent Wayland fixture");
    commands.send(rix_ipc::IpcRequest::ClipboardClear).unwrap();
    assert_eq!(responses.recv().unwrap().unwrap(), rix_ipc::IpcResponse::Ok);
    let image = image::RgbaImage::from_pixel(4, 4, image::Rgba([10, 45, 80, 255]));
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png).unwrap();
    let png = png.into_inner();
    commands
        .send(rix_ipc::IpcRequest::ClipboardSetPng { data: png.clone() })
        .unwrap();
    assert_eq!(
        responses
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap(),
        rix_ipc::IpcResponse::Ok
    );
    events.offered.clear();
    queue.roundtrip(&mut events).unwrap();
    assert!(events.offered.iter().any(|mime| mime == "image/png"));
    let (mut reader, writer) = UnixStream::pair().unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    events
        .selection
        .as_ref()
        .unwrap()
        .receive("image/png".into(), writer.as_fd());
    drop(writer);
    connection.flush().unwrap();
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(
        bytes, png,
        "Clipboard PNG must survive without a capture-client data source"
    );
    done.store(true, Ordering::Relaxed);
    server.join().unwrap();
}

#[test]
fn expired_pending_placement_applies_its_target_and_releases_the_entry() {
    use smithay::utils::{Rectangle, SERIAL_COUNTER};

    let mut desktop = TestDesktop::new();
    let window = desktop.window("Disposable placement target");
    desktop.inspect(|state| {
        let (id, window) = state.windows.entries[0].clone();
        let target = Rectangle::new((120, 140).into(), (80, 60).into());
        state.queue_window_placement(id, target, SERIAL_COUNTER.next_serial(), false);

        state.prune_expired_placements(Duration::from_secs(60));
        assert!(state.windows.pending_placement.contains_key(&id));

        state.prune_expired_placements(Duration::ZERO);
        assert!(!state.windows.pending_placement.contains_key(&id));
        assert_eq!(state.space.element_location(&window), Some(target.loc));
    });
    window.0.commit();
    desktop.roundtrip();
}

#[test]
fn expired_placement_waits_for_the_active_move_grab_to_finish() {
    use smithay::{
        input::pointer::{Focus, GrabStartData},
        utils::{Point, Rectangle, SERIAL_COUNTER},
    };

    let mut desktop = TestDesktop::new();
    let window = desktop.window("Disposable dragged placement target");
    desktop.inspect(|state| {
        let (id, window) = state.windows.entries[0].clone();
        let origin = Point::from((0, 0));
        state.space.map_element(window.clone(), origin, false);
        let pointer = state.seat.add_pointer();
        let start = Point::from((10., 10.));
        pointer.set_grab(
            state,
            crate::grabs::MoveSurfaceGrab {
                start_data: GrabStartData {
                    focus: None,
                    button: 0x110,
                    location: start,
                },
                window: window.clone(),
                initial_window_location: origin,
                snap_output: None,
            },
            SERIAL_COUNTER.next_serial(),
            Focus::Clear,
        );
        let target = Rectangle::new((120, 140).into(), (80, 60).into());
        state.queue_window_placement(id, target, SERIAL_COUNTER.next_serial(), false);

        state.prune_expired_placements(Duration::ZERO);
        assert!(
            state.windows.pending_placement.contains_key(&id),
            "an active move grab delays the expiry"
        );
        assert_eq!(state.space.element_location(&window), Some(origin));

        pointer.unset_grab(state, SERIAL_COUNTER.next_serial(), 1);
        state.prune_expired_placements(Duration::ZERO);
        assert!(!state.windows.pending_placement.contains_key(&id));
        assert_eq!(state.space.element_location(&window), Some(target.loc));
    });
    window.0.commit();
    desktop.roundtrip();
}
