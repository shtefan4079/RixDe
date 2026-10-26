use super::*;
use rix_protocols::remote_input_client::{
    rix_remote_input_manager_v1 as client_manager, rix_remote_input_session_v1 as client_session,
};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use std::{
    os::unix::net::UnixStream,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
};
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::wl_registry,
    Connection, Dispatch as ClientDispatch, QueueHandle,
};

#[derive(Default)]
struct Events {
    closed: Vec<u32>,
    unsupported_keysyms: Vec<u32>,
}
impl ClientDispatch<client_session::RixRemoteInputSessionV1, ()> for Events {
    fn event(
        state: &mut Self,
        _: &client_session::RixRemoteInputSessionV1,
        event: client_session::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            client_session::Event::Closed { reason } => state.closed.push(u32::from(reason)),
            client_session::Event::UnsupportedKeysym { sym } => state.unsupported_keysyms.push(sym),
            _ => {}
        }
    }
}
wayland_client::delegate_noop!(Events: ignore client_manager::RixRemoteInputManagerV1);
impl ClientDispatch<wl_registry::WlRegistry, GlobalListContents> for Events {
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
type Edit = Box<dyn FnOnce(&mut RixState) + Send>;
struct Fixture {
    connection: Connection,
    edits: mpsc::Sender<(Edit, mpsc::Sender<()>)>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    fn new(permitted: bool, greeter: bool) -> Self {
        let (server, client) = UnixStream::pair().unwrap();
        let (edits, pending) = mpsc::channel::<(Edit, mpsc::Sender<()>)>();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut display = wayland_server::Display::<RixState>::new().unwrap();
            let mut state =
                RixState::new_with_mode(&mut display, "remote-input-test".into(), greeter);
            state
                .seat
                .add_keyboard(Default::default(), 500, 30)
                .unwrap();
            state.seat.add_pointer();
            let output = Output::new(
                "remote-input-test".into(),
                PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: Subpixel::Unknown,
                    make: "test".into(),
                    model: "test".into(),
                },
            );
            output.change_current_state(
                Some(Mode {
                    size: (600, 400).into(),
                    refresh: 60000,
                }),
                None,
                None,
                Some((-100, 50).into()),
            );
            state.space.map_output(&output, (-100, 50));
            let _client = display
                .handle()
                .insert_client(
                    server,
                    Arc::new(ClientState {
                        allow_remote_input: permitted,
                        ..Default::default()
                    }),
                )
                .unwrap();
            while !stopping.load(Ordering::Relaxed) {
                display.dispatch_clients(&mut state).unwrap();
                refresh(&mut state);
                for (edit, reply) in pending.try_iter() {
                    edit(&mut state);
                    refresh(&mut state);
                    reply.send(()).unwrap();
                }
                display.flush_clients().unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        Self {
            connection: Connection::from_socket(client).unwrap(),
            edits,
            stop,
            thread: Some(thread),
        }
    }
    fn edit(&self, edit: impl FnOnce(&mut RixState) + Send + 'static) {
        let (reply, done) = mpsc::channel();
        self.edits.send((Box::new(edit), reply)).unwrap();
        done.recv_timeout(Duration::from_secs(3)).unwrap();
    }
    fn snapshot(&self) -> (HashSet<u32>, HashSet<u32>, (f64, f64), usize) {
        let (reply, done) = mpsc::channel();
        self.edit(move |state| {
            let keys = state
                .seat
                .get_keyboard()
                .unwrap()
                .pressed_keys()
                .into_iter()
                .map(u32::from)
                .collect();
            let point = state.seat.get_pointer().unwrap().current_location();
            reply
                .send((
                    keys,
                    state.pressed_buttons.clone(),
                    (point.x, point.y),
                    state.remote_input.sessions.len(),
                ))
                .unwrap();
        });
        done.recv().unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn remote_input_is_hidden_from_ordinary_clients_and_greeters() {
    for (permitted, greeter) in [(false, false), (true, true)] {
        let fixture = Fixture::new(permitted, greeter);
        let (globals, queue) = registry_queue_init::<Events>(&fixture.connection).unwrap();
        assert!(globals
            .bind::<client_manager::RixRemoteInputManagerV1, _, _>(&queue.handle(), 1..=1, ())
            .is_err());
    }
}

#[test]
fn native_remote_motion_keys_and_symmetric_keysym_release_use_the_seat() {
    let fixture = Fixture::new(true, false);
    let (globals, mut queue) = registry_queue_init::<Events>(&fixture.connection).unwrap();
    let qh = queue.handle();
    let manager: client_manager::RixRemoteInputManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
    let session = manager.create_session(&qh, ());
    let mut events = Events::default();
    session.motion_absolute(10000., -10000.);
    session.keycode(30, 1);
    session.button(272, 1);
    queue.roundtrip(&mut events).unwrap();
    let (keys, buttons, position, count) = fixture.snapshot();
    assert!(keys.contains(&38));
    assert!(buttons.contains(&272));
    assert_eq!(position, (499., 50.));
    assert_eq!(count, 1);
    session.keycode(30, 0);
    session.button(272, 0);
    session.keysym(xkb::keysyms::KEY_A, 1);
    queue.roundtrip(&mut events).unwrap();
    let keys = fixture.snapshot().0;
    assert!(
        keys.contains(&38) && keys.contains(&50),
        "Uppercase keysym acquires the current keymap's Shift and A"
    );
    session.keysym(xkb::keysyms::KEY_A, 0);
    session.motion(-10., 15.);
    queue.roundtrip(&mut events).unwrap();
    let (keys, buttons, position, _) = fixture.snapshot();
    assert!(keys.is_empty() && buttons.is_empty());
    assert_eq!(position, (489., 65.));
    assert!(events.closed.is_empty());
}

#[test]
fn remote_destroy_and_lock_release_remote_ownership_without_releasing_local_input() {
    let fixture = Fixture::new(true, false);
    let (globals, mut queue) = registry_queue_init::<Events>(&fixture.connection).unwrap();
    let qh = queue.handle();
    let manager: client_manager::RixRemoteInputManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
    let session = manager.create_session(&qh, ());
    let mut events = Events::default();
    session.keycode(30, 1);
    session.keycode(48, 1);
    session.button(272, 1);
    session.button(273, 1);
    queue.roundtrip(&mut events).unwrap();

    fixture.edit(|state| {
        state.handle_keyboard_key(
            38u32.into(),
            KeyState::Pressed,
            SERIAL_COUNTER.next_serial(),
            1,
            false,
        );
        state.handle_pointer_button(272, ButtonState::Pressed, SERIAL_COUNTER.next_serial(), 1);
    });
    session.destroy();
    queue.roundtrip(&mut events).unwrap();
    let (keys, buttons, _, count) = fixture.snapshot();
    assert_eq!(keys, HashSet::from([38]));
    assert_eq!(buttons, HashSet::from([272]));
    assert_eq!(count, 0);
    let session = manager.create_session(&qh, ());
    session.keycode(48, 1);
    session.button(273, 1);
    queue.roundtrip(&mut events).unwrap();
    fixture.edit(|state| state.lock.active = true);
    queue.roundtrip(&mut events).unwrap();
    assert!(events
        .closed
        .contains(&(session::CloseReason::Locked as u32)));
    let (keys, buttons, _, count) = fixture.snapshot();
    assert_eq!(keys, HashSet::from([38]));
    assert_eq!(buttons, HashSet::from([272]));
    assert_eq!(count, 0);
    session.keycode(16, 1);
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(
        fixture.snapshot().0,
        HashSet::from([38]),
        "Closed input sessions stay inert"
    );
    let denied = manager.create_session(&qh, ());
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(fixture.snapshot().3, 0);
    denied.destroy();
}

#[test]
fn remote_input_sessions_and_event_budgets_are_bounded() {
    let fixture = Fixture::new(true, false);
    let (globals, mut queue) = registry_queue_init::<Events>(&fixture.connection).unwrap();
    let qh = queue.handle();
    let manager: client_manager::RixRemoteInputManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
    let mut sessions = Vec::new();
    for _ in 0..5 {
        sessions.push(manager.create_session(&qh, ()));
    }
    let mut events = Events::default();
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(fixture.snapshot().3, MAX_SESSIONS);
    assert_eq!(events.closed, [session::CloseReason::Limit as u32]);
    fixture.edit(|state| {
        for held in state.remote_input.sessions.values_mut() {
            held.events = MAX_EVENTS_PER_SECOND;
            held.rate_started = Instant::now();
        }
    });
    sessions[0].keycode(30, 1);
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(fixture.snapshot().3, MAX_SESSIONS - 1);
    assert!(fixture.snapshot().0.is_empty());
}

#[test]
fn unsupported_unicode_does_not_close_control_and_disconnect_releases_held_input() {
    let fixture = Fixture::new(true, false);
    let (globals, mut queue) = registry_queue_init::<Events>(&fixture.connection).unwrap();
    let qh = queue.handle();
    let manager: client_manager::RixRemoteInputManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
    let session = manager.create_session(&qh, ());
    let mut events = Events::default();
    let emoji = 0x0101f600;
    session.keysym(emoji, 1);
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(events.unsupported_keysyms, [emoji]);
    assert!(events.closed.is_empty());
    assert_eq!(fixture.snapshot().3, 1);
    session.keycode(30, 1);
    session.button(272, 1);
    queue.roundtrip(&mut events).unwrap();
    assert!(fixture.snapshot().0.contains(&38));
    fixture.edit(|state| {
        let client = state
            .remote_input
            .sessions
            .values()
            .next()
            .unwrap()
            .resource
            .client()
            .unwrap();
        client.kill(
            &state.display_handle,
            wayland_server::backend::protocol::ProtocolError {
                code: 0,
                object_id: 1,
                object_interface: "wl_display".into(),
                message: "Test extension revoked".into(),
            },
        );
    });

    let deadline = Instant::now() + Duration::from_secs(3);
    let (keys, buttons, _, count) = loop {
        let snapshot = fixture.snapshot();
        if snapshot.3 == 0 || Instant::now() >= deadline {
            break snapshot;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    assert!(keys.is_empty() && buttons.is_empty());
    assert_eq!(count, 0);
}

#[test]
fn input_ownership_keeps_overlapping_physical_and_remote_holds_separate() {
    let mut ownership = Ownership::default();
    assert!(ownership.acquire(38));
    assert!(!ownership.acquire(38));
    assert!(!ownership.physical(38, true));
    assert!(!ownership.release(38));
    assert!(!ownership.physical(38, false));
    assert!(ownership.release(38));
    assert!(ownership.physical(38, true));
    assert!(!ownership.acquire(38));
    assert!(!ownership.release(38));
    assert!(ownership.physical(38, false));
}
