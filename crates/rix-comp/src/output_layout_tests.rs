use crate::state::{ClientState, RixState};
use std::{
    os::{fd::AsFd, unix::net::UnixStream},
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_buffer, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool, wl_surface},
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

#[derive(Default)]
struct Events {
    configures: Vec<(u32, u32)>,
    closed: bool,
    output_events: Vec<&'static str>,
    output_names: Vec<String>,
    window_closes: usize,
    window_surface: Option<wl_surface::WlSurface>,
}
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Events {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_registry::Event::GlobalRemove { .. }) {
            state.output_events.push("global_removed");
        }
    }
}
wayland_client::delegate_noop!(Events: ignore wl_compositor::WlCompositor);
impl Dispatch<wl_output::WlOutput, ()> for Events {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.output_names.push(name);
        }
    }
}
wayland_client::delegate_noop!(Events: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(Events: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(Events: ignore wl_shm_pool::WlShmPool);
impl Dispatch<wl_surface::WlSurface, ()> for Events {
    fn event(
        state: &mut Self,
        surface: &wl_surface::WlSurface,
        event: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let window = state.window_surface.as_ref() == Some(surface);
        match event {
            wl_surface::Event::Enter { .. } => state.output_events.push(if window {
                "window_enter"
            } else {
                "layer_enter"
            }),
            wl_surface::Event::Leave { .. } => state.output_events.push(if window {
                "window_leave"
            } else {
                "layer_leave"
            }),
            _ => {}
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
            state.window_closes += 1;
        }
    }
}
wayland_client::delegate_noop!(Events: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for Events {
    fn event(
        state: &mut Self,
        surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            surface.ack_configure(serial);
            state.configures.push((width, height));
        } else if let zwlr_layer_surface_v1::Event::Closed = event {
            state.closed = true;
        }
    }
}

#[test]
fn panel_waiting_for_output_survives_unplug_and_reconnect() {
    let (server_socket, client_socket) = UnixStream::pair().unwrap();
    #[derive(Clone, Copy)]
    enum Change {
        Connect(i32, i32),
        Disconnect,
        Inspect,
        Minimize(u64),
    }
    #[derive(Debug)]
    struct Snapshot {
        pending: usize,
        layers: usize,
        windows: Vec<u64>,
        mapped: usize,
        minimized: usize,
    }
    let (control, receiver) = mpsc::channel::<(Change, mpsc::Sender<Snapshot>)>();
    let (stop, stopped) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut display = wayland_server::Display::<RixState>::new().unwrap();
        let mut state = RixState::new(&mut display, "output-test".into());
        let mut connected_output = None;
        let _client = display
            .handle()
            .insert_client(server_socket, Arc::new(ClientState::default()))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && stopped.try_recv().is_err() {
            display.dispatch_clients(&mut state).unwrap();
            if let Ok((change, reply)) = receiver.try_recv() {
                match change {
                    Change::Disconnect => {
                        let (old_output, global) = connected_output.take().unwrap();
                        state.detach_output(&old_output);
                        display.handle().disable_global::<RixState>(global);
                        state.reflow_output_layout();
                    }
                    Change::Connect(width, height) => {
                        assert!(connected_output.is_none());
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
                                size: (width, height).into(),
                                refresh: 60_000,
                            }),
                            None,
                            None,
                            Some((0, 0).into()),
                        );
                        let global = output.create_global::<RixState>(&display.handle());
                        state.space.map_output(&output, (0, 0));
                        state.reflow_output_layout();
                        connected_output = Some((output, global));
                    }
                    Change::Minimize(id) => state
                        .apply_window_action(rix_ipc::WindowAction::Minimize(id))
                        .unwrap(),
                    Change::Inspect => {}
                }
                reply
                    .send(Snapshot {
                        pending: state.pending_layers.len(),
                        layers: state
                            .space
                            .outputs()
                            .map(|o| smithay::desktop::layer_map_for_output(o).layers().count())
                            .sum(),
                        windows: state.windows.entries.iter().map(|(id, _)| *id).collect(),
                        mapped: state.space.elements().count(),
                        minimized: state.windows.minimized.len(),
                    })
                    .unwrap();
            }
            display.flush_clients().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let connection = Connection::from_socket(client_socket).unwrap();
    let (globals, mut queue) = registry_queue_init::<Events>(&connection).unwrap();
    let handle = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&handle, 4..=4, ()).unwrap();
    let shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = globals.bind(&handle, 1..=4, ()).unwrap();
    let surface = compositor.create_surface(&handle, ());
    let panel = shell.get_layer_surface(
        &surface,
        None,
        zwlr_layer_shell_v1::Layer::Top,
        "test-panel".into(),
        &handle,
        (),
    );
    panel.set_size(0, 40);
    panel.set_anchor(
        zwlr_layer_surface_v1::Anchor::Bottom
            | zwlr_layer_surface_v1::Anchor::Left
            | zwlr_layer_surface_v1::Anchor::Right,
    );
    surface.commit();
    let mut events = Events::default();
    queue.roundtrip(&mut events).unwrap();
    assert!(events.configures.is_empty());
    let change = |change| {
        let (reply, result) = mpsc::channel();
        control.send((change, reply)).unwrap();
        result.recv_timeout(Duration::from_secs(2)).unwrap()
    };
    let first = change(Change::Connect(1280, 720));
    assert_eq!((first.pending, first.layers), (0, 1));
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(events.configures.last(), Some(&(1280, 40)));
    let output: wl_output::WlOutput = globals.bind(&handle, 4..=4, ()).unwrap();

    let bound_surface = compositor.create_surface(&handle, ());
    let bound_panel = shell.get_layer_surface(
        &bound_surface,
        Some(&output),
        zwlr_layer_shell_v1::Layer::Top,
        "explicit-output-panel".into(),
        &handle,
        (),
    );
    bound_panel.set_size(0, 32);
    bound_panel.set_anchor(
        zwlr_layer_surface_v1::Anchor::Top
            | zwlr_layer_surface_v1::Anchor::Left
            | zwlr_layer_surface_v1::Anchor::Right,
    );
    bound_surface.commit();
    queue.roundtrip(&mut events).unwrap();

    let base: xdg_wm_base::XdgWmBase = globals.bind(&handle, 1..=1, ()).unwrap();
    let shm: wl_shm::WlShm = globals.bind(&handle, 1..=1, ()).unwrap();
    let file = tempfile::tempfile().unwrap();
    file.set_len(4096).unwrap();
    let pool = shm.create_pool(file.as_fd(), 4096, &handle, ());
    let buffer = pool.create_buffer(0, 16, 16, 64, wl_shm::Format::Argb8888, &handle, ());
    let window = compositor.create_surface(&handle, ());
    events.window_surface = Some(window.clone());
    let xdg = base.get_xdg_surface(&window, &handle, ());
    let _top = xdg.get_toplevel(&handle, ());
    window.commit();
    queue.roundtrip(&mut events).unwrap();
    window.attach(Some(&buffer), 0, 0);
    window.commit();
    queue.roundtrip(&mut events).unwrap();
    let before = change(Change::Inspect);
    assert_eq!((before.windows.len(), before.mapped), (1, 1));
    assert!(events.output_events.contains(&"window_enter"));
    events.output_events.clear();

    let detached = change(Change::Disconnect);
    assert_eq!((detached.pending, detached.layers), (2, 0));
    assert_eq!(detached.windows, before.windows);
    assert_eq!(detached.mapped, 1);
    queue.roundtrip(&mut events).unwrap();
    let leave = events
        .output_events
        .iter()
        .position(|event| *event == "window_leave")
        .unwrap();
    let removed = events
        .output_events
        .iter()
        .position(|event| *event == "global_removed")
        .unwrap();
    assert!(
        leave < removed,
        "Output was removed before its surface leaves: {:?}",
        events.output_events
    );
    assert!(!events.closed);
    assert_eq!(events.window_closes, 0);

    window.commit();
    surface.commit();
    queue.roundtrip(&mut events).unwrap();
    let minimized = change(Change::Minimize(before.windows[0]));
    assert_eq!((minimized.mapped, minimized.minimized), (0, 1));
    assert_eq!(minimized.windows, before.windows);
    let returned = change(Change::Connect(1024, 768));
    assert_eq!((returned.pending, returned.layers), (0, 2));
    assert_eq!((returned.mapped, returned.minimized), (0, 1));
    assert_eq!(returned.windows, before.windows);
    queue.roundtrip(&mut events).unwrap();
    assert!(!events.closed);
    assert_eq!(events.window_closes, 0);
    assert!(events.configures.contains(&(1024, 40)));
    assert!(events.configures.contains(&(1024, 32)));
    output.release();
    stop.send(()).unwrap();
    server.join().unwrap();
}

#[test]
fn explicit_output_panel_stays_on_its_monitor_through_hotplug() {
    let (server_socket, client_socket) = UnixStream::pair().unwrap();
    #[derive(Clone, Copy)]
    enum Change {
        Disconnect(&'static str),
        Reconnect(&'static str),
        Inspect,
    }
    #[derive(Debug)]
    struct Snapshot {
        pending: usize,
        left: usize,
        right: usize,
    }
    let (control, receiver) = mpsc::channel::<(Change, mpsc::Sender<Snapshot>)>();
    let (stop, stopped) = mpsc::channel();
    let server = std::thread::spawn(move || {
        fn monitor(name: &str, x: i32) -> smithay::output::Output {
            let output = smithay::output::Output::new(
                name.into(),
                smithay::output::PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: smithay::output::Subpixel::Unknown,
                    make: "test".into(),
                    model: "test".into(),
                },
            );
            output.change_current_state(
                Some(smithay::output::Mode {
                    size: (800, 600).into(),
                    refresh: 60_000,
                }),
                None,
                None,
                Some((x, 0).into()),
            );
            output
        }
        let mut display = wayland_server::Display::<RixState>::new().unwrap();
        let mut state = RixState::new(&mut display, "output-binding-test".into());
        let _client = display
            .handle()
            .insert_client(server_socket, Arc::new(ClientState::default()))
            .unwrap();
        let mut connected: std::collections::HashMap<&'static str, _> =
            std::collections::HashMap::new();
        for (name, x) in [("left", 0), ("right", 800)] {
            let output = monitor(name, x);
            let global = output.create_global::<RixState>(&display.handle());
            state.space.map_output(&output, (x, 0));
            connected.insert(name, (output, global));
        }
        state.reflow_output_layout();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && stopped.try_recv().is_err() {
            display.dispatch_clients(&mut state).unwrap();
            if let Ok((change, reply)) = receiver.try_recv() {
                match change {
                    Change::Disconnect(name) => {
                        let (output, global) = connected.remove(name).unwrap();
                        state.detach_output(&output);
                        display.handle().disable_global::<RixState>(global);
                        state.reflow_output_layout();
                    }
                    Change::Reconnect(name) => {
                        assert!(!connected.contains_key(name));
                        let x = if name == "left" { 0 } else { 800 };
                        let output = monitor(name, x);
                        let global = output.create_global::<RixState>(&display.handle());
                        state.space.map_output(&output, (x, 0));
                        state.reflow_output_layout();
                        connected.insert(name, (output, global));
                    }
                    Change::Inspect => {}
                }
                let layers_on = |name: &str| {
                    state
                        .space
                        .outputs()
                        .filter(|output| output.name() == name)
                        .map(|output| {
                            smithay::desktop::layer_map_for_output(output)
                                .layers()
                                .count()
                        })
                        .sum::<usize>()
                };
                reply
                    .send(Snapshot {
                        pending: state.pending_layers.len(),
                        left: layers_on("left"),
                        right: layers_on("right"),
                    })
                    .unwrap();
            }
            display.flush_clients().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let change = {
        let control = control;
        move |change| {
            let (reply, result) = mpsc::channel();
            control.send((change, reply)).unwrap();
            result.recv_timeout(Duration::from_secs(2)).unwrap()
        }
    };
    let connection = Connection::from_socket(client_socket).unwrap();
    let (globals, mut queue) = registry_queue_init::<Events>(&connection).unwrap();
    let handle = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&handle, 4..=4, ()).unwrap();
    let shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = globals.bind(&handle, 1..=4, ()).unwrap();
    let mut events = Events::default();

    let output_globals: Vec<_> = globals
        .contents()
        .clone_list()
        .into_iter()
        .filter(|global| global.interface == "wl_output" && global.version >= 4)
        .collect();
    assert_eq!(output_globals.len(), 2, "the fixture exposes both monitors");
    let left: wl_output::WlOutput = globals
        .registry()
        .bind(output_globals[0].name, 4, &handle, ());
    let right: wl_output::WlOutput =
        globals
            .registry()
            .bind(output_globals[1].name, 4, &handle, ());
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(events.output_names, ["left", "right"]);

    let anchored =
        |surface: &wl_surface::WlSurface, output: Option<&wl_output::WlOutput>, height: u32| {
            let panel = shell.get_layer_surface(
                surface,
                output,
                zwlr_layer_shell_v1::Layer::Top,
                "test-panel".into(),
                &handle,
                (),
            );
            panel.set_size(0, height);
            panel.set_anchor(
                zwlr_layer_surface_v1::Anchor::Top
                    | zwlr_layer_surface_v1::Anchor::Left
                    | zwlr_layer_surface_v1::Anchor::Right,
            );
        };
    let right_surface = compositor.create_surface(&handle, ());
    anchored(&right_surface, Some(&right), 40);
    right_surface.commit();
    queue.roundtrip(&mut events).unwrap();
    let free_surface = compositor.create_surface(&handle, ());
    anchored(&free_surface, None, 32);
    free_surface.commit();
    queue.roundtrip(&mut events).unwrap();

    let initial = change(Change::Inspect);
    assert_eq!((initial.pending, initial.left, initial.right), (0, 1, 1));
    assert!(events.configures.contains(&(800, 40)));
    assert!(events.configures.contains(&(800, 32)));
    assert!(!events.closed);

    let detached = change(Change::Disconnect("right"));
    assert_eq!(
        (detached.pending, detached.left, detached.right),
        (1, 1, 0),
        "a panel for an absent monitor must not slide onto another output"
    );
    queue.roundtrip(&mut events).unwrap();
    assert!(!events.closed);

    let returned = change(Change::Reconnect("right"));
    assert_eq!((returned.pending, returned.left, returned.right), (0, 1, 1));
    queue.roundtrip(&mut events).unwrap();
    assert!(!events.closed);
    assert!(events.configures.contains(&(800, 40)));
    assert!(events.configures.contains(&(800, 32)));

    left.release();
    right.release();
    stop.send(()).unwrap();
    server.join().unwrap();
}
