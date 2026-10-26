use std::{
    collections::HashMap,
    fs::File,
    io::Write,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    path::PathBuf,
    time::{Duration, Instant},
};
use wayland_client::{
    backend::ObjectId,
    protocol::{
        wl_buffer, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer,
        wl_data_source, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

type Actions = wl_data_device_manager::DndAction;
const MIME: &str = "text/uri-list";
struct Offer {
    proxy: wl_data_offer::WlDataOffer,
    uri: bool,
    action: Actions,
}
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    shell: Option<xdg_wm_base::XdgWmBase>,
    manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    device: Option<wl_data_device::WlDataDevice>,
    surface: Option<wl_surface::WlSurface>,
    buffer: Option<wl_buffer::WlBuffer>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    source: Option<wl_data_source::WlDataSource>,
    offers: HashMap<ObjectId, Offer>,
    incoming: Option<Offer>,
    configured: bool,
    source_mode: bool,
    positioned: bool,
    reject: bool,
    accepted: bool,
    selected: Actions,
    uris: Vec<u8>,
    received: Vec<PathBuf>,
    trace: Vec<String>,
    done: bool,
}
impl Dispatch<wl_registry::WlRegistry, ()> for State {
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
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "xdg_wm_base" => state.shell = Some(registry.bind(name, version.min(3), qh, ())),
                "wl_data_device_manager" => {
                    state.manager = Some(registry.bind(name, version.min(3), qh, ()))
                }
                "wl_seat" => state.seat = Some(registry.bind(name, version.min(7), qh, ())),
                _ => {}
            }
        }
    }
}
impl Dispatch<wl_seat::WlSeat, ()> for State {
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
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_pointer::Event::Button {
            serial,
            button: 272,
            state: WEnum::Value(wl_pointer::ButtonState::Pressed),
            ..
        } = event
        {

            if !state.positioned {
                state
                    .toplevel
                    .as_ref()
                    .unwrap()
                    ._move(state.seat.as_ref().unwrap(), serial);
                state.positioned = true;
                return;
            }
            if state.source_mode && state.source.is_none() {
                let source = state.manager.as_ref().unwrap().create_data_source(qh, ());
                source.offer(MIME.into());
                source.set_actions(Actions::Copy | Actions::Move);
                state.device.as_ref().unwrap().start_drag(
                    Some(&source),
                    state.surface.as_ref().unwrap(),
                    None,
                    serial,
                );
                state.source = Some(source);
                state.trace.push("start_drag".into());
            }
        }
    }
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        proxy: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            proxy.pong(serial);
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            proxy.ack_configure(serial);
            let surface = state.surface.as_ref().unwrap();
            surface.attach(state.buffer.as_ref(), 0, 0);
            surface.damage(0, 0, 300, 200);
            surface.commit();
            state.configured = true;
        }
    }
}
impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    wayland_client::event_created_child!(State, wl_data_device::WlDataDevice, [0 => (wl_data_offer::WlDataOffer, ())]);
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        connection: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                state.offers.insert(
                    id.id(),
                    Offer {
                        proxy: id,
                        uri: false,
                        action: Actions::None,
                    },
                );
            }
            wl_data_device::Event::Selection { id } => {
                if let Some(offer) = id.and_then(|id| state.offers.remove(&id.id())) {
                    offer.proxy.destroy();
                }
            }
            wl_data_device::Event::Enter { serial, id, .. } => {
                if let Some(offer) = id.and_then(|id| state.offers.remove(&id.id())) {
                    state.accepted = !state.reject && offer.uri;
                    offer
                        .proxy
                        .accept(serial, state.accepted.then(|| MIME.into()));
                    offer.proxy.set_actions(
                        if state.accepted {
                            state.selected
                        } else {
                            Actions::None
                        },
                        if state.accepted {
                            state.selected
                        } else {
                            Actions::None
                        },
                    );
                    state.trace.push("enter".into());
                    state.incoming = Some(offer);
                }
            }
            wl_data_device::Event::Leave => {
                if let Some(offer) = state.incoming.take() {
                    offer.proxy.destroy();
                }
                state.trace.push("leave".into());
            }
            wl_data_device::Event::Drop => {
                let Some(offer) = state.incoming.take() else {
                    return;
                };
                state.trace.push("drop".into());
                if !state.accepted || offer.action == Actions::None {
                    offer.proxy.destroy();
                    return;
                }
                let mut descriptors = [-1; 2];
                assert_eq!(
                    unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
                    0
                );
                let (reader, writer) = unsafe {
                    (
                        OwnedFd::from_raw_fd(descriptors[0]),
                        OwnedFd::from_raw_fd(descriptors[1]),
                    )
                };
                offer.proxy.receive(MIME.into(), writer.as_fd());
                drop(writer);
                connection.flush().unwrap();
                let bytes =
                    rix_files::clipboard::read_bounded(File::from(reader), Duration::from_secs(5))
                        .unwrap();
                state.received = rix_files::clipboard::decode(&bytes, false).unwrap().0;
                state.trace.push(format!("action:{:?}", offer.action));
                offer.proxy.finish();
                offer.proxy.destroy();
                state.trace.push("finish".into());
                state.done = true;
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_data_offer::WlDataOffer, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let offer = match &mut state.incoming {
            Some(offer) if offer.proxy == *proxy => Some(offer),
            _ => state.offers.get_mut(&proxy.id()),
        };
        if let Some(offer) = offer {
            match event {
                wl_data_offer::Event::Offer { mime_type } if mime_type == MIME => offer.uri = true,
                wl_data_offer::Event::Action {
                    dnd_action: WEnum::Value(action),
                } => offer.action = action,
                _ => {}
            }
        }
    }
}
impl Dispatch<wl_data_source::WlDataSource, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type, fd } if mime_type == MIME => {
                state.trace.push("send:uri-list".into());
                assert!(state.uris.len() < 4096);
                File::from(fd).write_all(&state.uris).unwrap();
            }
            wl_data_source::Event::Action {
                dnd_action: WEnum::Value(action),
            } => state.trace.push(format!("action:{action:?}")),
            wl_data_source::Event::DndFinished => {
                state.trace.push("dnd_finished".into());
                state.done = true;
            }
            wl_data_source::Event::Cancelled => {
                state.trace.push("cancelled".into());
                state.done = true;
            }
            wl_data_source::Event::DndDropPerformed => state.trace.push("drop_performed".into()),
            _ => {}
        }
    }
}
wayland_client::delegate_noop!(State: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: ignore wl_data_device_manager::WlDataDeviceManager);
wayland_client::delegate_noop!(State: ignore xdg_toplevel::XdgToplevel);

#[test]
#[ignore = "only scripts/tests/files_dnd_wayland.py may run this private compositor peer"]
fn standard_wayland_file_drag_peer() {
    assert_eq!(
        std::env::var("RIX_FILES_ISOLATED_WAYLAND").as_deref(),
        Ok("1")
    );
    let root = PathBuf::from(std::env::var_os("RIX_FILES_PRIVATE_FIXTURE").unwrap());
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    assert_eq!(home, root.join("home"));
    assert_eq!(runtime, root.join("runtime"));
    let status_path = PathBuf::from(std::env::var_os("RIX_DND_PEER_STATUS").unwrap());
    assert!(status_path.starts_with(&root));
    let paths: Vec<PathBuf> =
        serde_json::from_str(&std::env::var("RIX_DND_PEER_PATHS").unwrap_or("[]".into())).unwrap();
    assert!(paths.iter().all(|path| path.starts_with(&home)));
    let source_mode = std::env::var("RIX_DND_PEER_MODE").as_deref() == Ok("source");
    let connection = Connection::connect_to_env().unwrap();
    let mut queue = connection.new_event_queue::<State>();
    let qh = queue.handle();
    let _registry = connection.display().get_registry(&qh, ());
    let mut state = State {
        compositor: None,
        shm: None,
        shell: None,
        manager: None,
        seat: None,
        pointer: None,
        device: None,
        surface: None,
        buffer: None,
        toplevel: None,
        source: None,
        offers: HashMap::new(),
        incoming: None,
        configured: false,
        source_mode,
        positioned: false,
        reject: std::env::var("RIX_DND_PEER_REJECT").as_deref() == Ok("1"),
        accepted: false,
        selected: if std::env::var("RIX_DND_PEER_ACTION").as_deref() == Ok("move") {
            Actions::Move
        } else {
            Actions::Copy
        },
        uris: if source_mode {
            rix_files::clipboard::encode(&paths, false).unwrap().1
        } else {
            vec![]
        },
        received: vec![],
        trace: vec![],
        done: false,
    };
    queue.roundtrip(&mut state).unwrap();
    assert_eq!(state.manager.as_ref().unwrap().version(), 3);
    state.device = Some(state.manager.as_ref().unwrap().get_data_device(
        state.seat.as_ref().unwrap(),
        &qh,
        (),
    ));
    let surface = state.compositor.as_ref().unwrap().create_surface(&qh, ());
    let shell_surface = state
        .shell
        .as_ref()
        .unwrap()
        .get_xdg_surface(&surface, &qh, ());
    let top = shell_surface.get_toplevel(&qh, ());
    top.set_title("Private standard file DnD peer".into());
    top.set_app_id("org.rix.test.FileDnD".into());
    top.set_min_size(300, 200);
    top.set_max_size(300, 200);
    state.toplevel = Some(top.clone());
    shell_surface.set_window_geometry(0, 0, 300, 200);
    let mut buffer_file = tempfile::tempfile().unwrap();
    buffer_file.write_all(&vec![0x66; 300 * 200 * 4]).unwrap();
    let pool = state
        .shm
        .as_ref()
        .unwrap()
        .create_pool(buffer_file.as_fd(), 300 * 200 * 4, &qh, ());
    state.buffer = Some(pool.create_buffer(0, 300, 200, 1200, wl_shm::Format::Xrgb8888, &qh, ()));
    pool.destroy();
    state.surface = Some(surface);
    state.surface.as_ref().unwrap().commit();
    let deadline = Instant::now() + Duration::from_secs(50);
    loop {
        queue.dispatch_pending(&mut state).unwrap();
        connection.flush().unwrap();
        let status = serde_json::json!({"ready":state.configured,"done":state.done,"trace":state.trace,"received":state.received});
        let pending = status_path.with_extension("pending");
        std::fs::write(&pending, serde_json::to_vec(&status).unwrap()).unwrap();
        std::fs::rename(pending, &status_path).unwrap();
        if state.done {
            break;
        }
        assert!(Instant::now() < deadline, "Peer protocol timeout: {status}");
        if let Some(read) = queue.prepare_read() {
            let mut poll = libc::pollfd {
                fd: connection.backend().poll_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll, 1, 50) };
            if ready > 0 {
                read.read().unwrap();
            }
        }
    }
}
