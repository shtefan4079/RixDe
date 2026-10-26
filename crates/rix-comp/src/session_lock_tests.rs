use super::*;
use std::{
    os::unix::net::UnixStream,
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::wl_registry,
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1 as manager, ext_session_lock_v1 as client_lock,
};

#[derive(Default)]
struct Events {
    locked: usize,
    rejected: usize,
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
impl Dispatch<client_lock::ExtSessionLockV1, ()> for Events {
    fn event(
        state: &mut Self,
        _: &client_lock::ExtSessionLockV1,
        event: client_lock::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            client_lock::Event::Locked => state.locked += 1,
            client_lock::Event::Finished => state.rejected += 1,
            _ => {}
        }
    }
}
wayland_client::delegate_noop!(Events: ignore manager::ExtSessionLockManagerV1);

enum Control {
    Present(usize, u64),
    AddOutput,
    TrustSecond,
    Snapshot(mpsc::Sender<(bool, bool)>),
    Stop,
}
struct Harness {
    tx: mpsc::Sender<Control>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Harness {
    fn new() -> (Self, UnixStream, UnixStream) {
        let (a, first) = UnixStream::pair().unwrap();
        let (b, second) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut display = wayland_server::Display::<RixState>::new().unwrap();
            let mut state = RixState::new(&mut display, "lock-test".into());
            state
                .seat
                .add_keyboard(Default::default(), 200, 30)
                .unwrap();
            state.seat.add_pointer();
            let mut outputs: Vec<_> = ["first", "second"]
                .into_iter()
                .enumerate()
                .map(|(index, name)| {
                    let output = Output::new(
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
                            size: (640, 480).into(),
                            refresh: 60_000,
                        }),
                        None,
                        None,
                        Some((index as i32 * 640, 0).into()),
                    );
                    state.space.map_output(&output, (index as i32 * 640, 0));
                    output
                })
                .collect();
            let mut clients = Vec::new();
            for socket in [a, b] {
                clients.push(
                    display
                        .handle()
                        .insert_client(socket, Arc::new(crate::state::ClientState::default()))
                        .unwrap(),
                );
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            'event: while Instant::now() < deadline {
                display.dispatch_clients(&mut state).unwrap();
                for control in rx.try_iter() {
                    match control {
                        Control::Present(index, generation) => {
                            state.lock_frame_presented(&outputs[index], generation)
                        }
                        Control::Snapshot(reply) => {
                            let _ = reply.send((state.lock.active, state.lock.confirmed));
                        }
                        Control::TrustSecond => {
                            state.lock.managed_client = Some(clients[1].clone())
                        }
                        Control::AddOutput => {
                            let output = Output::new(
                                "third".into(),
                                smithay::output::PhysicalProperties {
                                    size: (0, 0).into(),
                                    subpixel: smithay::output::Subpixel::Unknown,
                                    make: "test".into(),
                                    model: "test".into(),
                                },
                            );
                            output.change_current_state(
                                Some(smithay::output::Mode {
                                    size: (640, 480).into(),
                                    refresh: 60_000,
                                }),
                                None,
                                None,
                                Some((1280, 0).into()),
                            );
                            state.space.map_output(&output, (1280, 0));
                            outputs.push(output);
                            state.sync_lock_outputs();
                        }
                        Control::Stop => break 'event,
                    }
                }
                display.flush_clients().unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        (
            Self {
                tx,
                thread: Some(thread),
            },
            first,
            second,
        )
    }
    fn state(&self) -> (bool, bool) {
        let (tx, rx) = mpsc::channel();
        self.tx.send(Control::Snapshot(tx)).unwrap();
        rx.recv_timeout(Duration::from_secs(2)).unwrap()
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.tx.send(Control::Stop);
        let _ = self.thread.take().unwrap().join();
    }
}

#[test]
fn lock_waits_for_all_outputs_and_only_its_owner_can_unlock() {
    let (server, socket, attacker_socket) = Harness::new();
    let connection = Connection::from_socket(socket).unwrap();
    let (globals, mut queue) = registry_queue_init::<Events>(&connection).unwrap();
    let manager: manager::ExtSessionLockManagerV1 =
        globals.bind(&queue.handle(), 1..=1, ()).unwrap();
    let owner = manager.lock(&queue.handle(), ());
    let mut events = Events::default();
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(server.state(), (true, false));
    assert_eq!(events.locked, 0);
    server.tx.send(Control::Present(0, 0)).unwrap();
    server.tx.send(Control::Present(1, 1)).unwrap();
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(server.state(), (true, false));
    server.tx.send(Control::Present(0, 1)).unwrap();
    queue.roundtrip(&mut events).unwrap();
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(events.locked, 1);

    let attacker = Connection::from_socket(attacker_socket).unwrap();
    let (globals, mut attacker_queue) = registry_queue_init::<Events>(&attacker).unwrap();
    let manager: manager::ExtSessionLockManagerV1 =
        globals.bind(&attacker_queue.handle(), 1..=1, ()).unwrap();
    let rejected = manager.lock(&attacker_queue.handle(), ());
    let mut attacker_events = Events::default();
    attacker_queue.roundtrip(&mut attacker_events).unwrap();
    assert_eq!(attacker_events.rejected, 1);
    rejected.unlock_and_destroy();
    assert!(attacker_queue.roundtrip(&mut attacker_events).is_err());
    assert_eq!(
        server.state(),
        (true, true),
        "Rejected locker must never unlock the session"
    );
    owner.unlock_and_destroy();
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(server.state(), (false, false));
}

#[test]
fn locker_disconnect_keeps_session_locked() {
    let (server, socket, _) = Harness::new();
    {
        let connection = Connection::from_socket(socket).unwrap();
        let (globals, mut queue) = registry_queue_init::<Events>(&connection).unwrap();
        let manager: manager::ExtSessionLockManagerV1 =
            globals.bind(&queue.handle(), 1..=1, ()).unwrap();
        let _lock = manager.lock(&queue.handle(), ());
        queue.roundtrip(&mut Events::default()).unwrap();
        assert!(server.state().0);
    }
    std::thread::sleep(Duration::from_millis(10));
    assert!(
        server.state().0,
        "A dead lock client must not reveal desktop content"
    );
}

#[test]
fn monitor_connected_during_lock_also_waits_for_a_closed_frame() {
    let (server, socket, _) = Harness::new();
    let connection = Connection::from_socket(socket).unwrap();
    let (globals, mut queue) = registry_queue_init::<Events>(&connection).unwrap();
    let manager: manager::ExtSessionLockManagerV1 =
        globals.bind(&queue.handle(), 1..=1, ()).unwrap();
    let _lock = manager.lock(&queue.handle(), ());
    let mut events = Events::default();
    queue.roundtrip(&mut events).unwrap();
    server.tx.send(Control::AddOutput).unwrap();
    server.tx.send(Control::Present(0, 1)).unwrap();
    server.tx.send(Control::Present(1, 1)).unwrap();
    assert_eq!(server.state(), (true, false));
    server.tx.send(Control::Present(2, 1)).unwrap();
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(server.state(), (true, true));
    assert_eq!(events.locked, 1);
}

#[test]
fn supervisor_recovery_is_locked_before_outputs_and_missing_locker_cannot_unlock_it() {
    let directory = tempfile::tempdir().unwrap();
    let config = rix_config::ConfigManager::init_in(directory.path().to_owned()).unwrap();
    let mut desktop = config.get_desktop();
    desktop.session.lock_command = vec![directory
        .path()
        .join("missing-locker")
        .to_string_lossy()
        .into_owned()];
    config.save_desktop(desktop).unwrap();
    let mut display = wayland_server::Display::<RixState>::new().unwrap();
    let mut state = RixState::new(&mut display, "supervisor-lock-test".into());
    assert!(!state.lock.active);
    state.restore_supervisor_lock(true);
    assert!(state.lock.active);
    assert!(!state.lock.confirmed);
    assert!(state.space.outputs().next().is_none());
    let generation = state.lock.generation;
    state.restore_supervisor_lock(true);
    assert_eq!(
        state.lock.generation, generation,
        "Restoration must not reset a live lock"
    );
    state.restore_supervisor_lock(false);
    assert!(
        state.lock.active,
        "Supervisor state cannot unlock an existing lock"
    );
    assert!(state
        .apply_window_action(rix_ipc::WindowAction::List)
        .is_err());
    assert!(state.launch_locker(&config).is_err());
    assert!(state.lock.active);
    state.tick_locker(&config);
    assert!(state.lock.retry_at.is_some());
}

#[test]
fn native_lock_remains_closed_when_configured_executable_is_missing() {
    let directory = tempfile::tempdir().unwrap();
    let config = rix_config::ConfigManager::init_in(directory.path().to_owned()).unwrap();
    let mut desktop = config.get_desktop();
    desktop.session.lock_command = vec![directory
        .path()
        .join("missing-locker")
        .to_string_lossy()
        .into_owned()];
    config.save_desktop(desktop).unwrap();
    let mut display = wayland_server::Display::<RixState>::new().unwrap();
    let mut state = RixState::new(&mut display, "missing-locker-test".into());
    state.native_session = true;
    assert!(state.launch_locker(&config).is_err());
    assert!(
        state.lock.active,
        "A missing binary must not expose the desktop after external sleep"
    );
    assert!(!state.lock.confirmed);
    state.tick_locker(&config);
    assert!(
        state.lock.retry_at.is_some(),
        "Recovery must retry without unlocking"
    );
}

#[test]
fn only_private_managed_connection_can_replace_a_dead_locker() {
    let (server, socket, replacement_socket) = Harness::new();
    {
        let connection = Connection::from_socket(socket).unwrap();
        let (globals, mut queue) = registry_queue_init::<Events>(&connection).unwrap();
        let manager: manager::ExtSessionLockManagerV1 =
            globals.bind(&queue.handle(), 1..=1, ()).unwrap();
        let _lock = manager.lock(&queue.handle(), ());
        queue.roundtrip(&mut Events::default()).unwrap();
        server.tx.send(Control::Present(0, 1)).unwrap();
        server.tx.send(Control::Present(1, 1)).unwrap();
        assert_eq!(server.state(), (true, true));
    }
    let connection = Connection::from_socket(replacement_socket).unwrap();
    let (globals, mut queue) = registry_queue_init::<Events>(&connection).unwrap();
    let manager: manager::ExtSessionLockManagerV1 =
        globals.bind(&queue.handle(), 1..=1, ()).unwrap();
    let rejected = manager.lock(&queue.handle(), ());
    let mut events = Events::default();
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(
        events.rejected, 1,
        "An ordinary client cannot take over a dead locker"
    );
    rejected.destroy();
    server.tx.send(Control::TrustSecond).unwrap();
    assert!(server.state().0);
    let accepted = manager.lock(&queue.handle(), ());
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(
        events.locked, 1,
        "Recovery reuses the already presented locked frame"
    );
    accepted.unlock_and_destroy();
    queue.roundtrip(&mut events).unwrap();
    assert_eq!(server.state(), (false, false));
}
