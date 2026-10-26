use crate::state::RixState;
use smithay::wayland::selection::ext_data_control::{DataControlHandler, DataControlState};

impl DataControlHandler for RixState {
    fn data_control_state(&self) -> &DataControlState {
        self.data_control
            .as_ref()
            .expect("data control is only dispatched in desktop sessions")
    }
}
smithay::delegate_ext_data_control!(RixState);

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::fd::AsFd,
        os::unix::net::UnixStream,
        sync::{mpsc, Arc},
        time::{Duration, Instant},
    };
    use wayland_client::{
        globals::{registry_queue_init, GlobalListContents},
        protocol::{wl_registry, wl_seat},
        Connection, Dispatch, QueueHandle,
    };
    use wayland_protocols::ext::{
        data_control::v1::client::{
            ext_data_control_device_v1 as device, ext_data_control_manager_v1 as manager,
            ext_data_control_offer_v1 as offer, ext_data_control_source_v1 as source,
        },
        session_lock::v1::client::{
            ext_session_lock_manager_v1 as lock_manager, ext_session_lock_v1 as lock,
        },
    };

    #[derive(Default)]
    struct Events {
        clipboard: Option<offer::ExtDataControlOfferV1>,
        primary: Option<offer::ExtDataControlOfferV1>,
        sends: usize,
        locked: bool,
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
    wayland_client::delegate_noop!(Events: ignore wl_seat::WlSeat);
    wayland_client::delegate_noop!(Events: ignore manager::ExtDataControlManagerV1);
    wayland_client::delegate_noop!(Events: ignore lock_manager::ExtSessionLockManagerV1);
    wayland_client::delegate_noop!(Events: ignore offer::ExtDataControlOfferV1);
    impl Dispatch<device::ExtDataControlDeviceV1, ()> for Events {
        fn event(
            state: &mut Self,
            _: &device::ExtDataControlDeviceV1,
            event: device::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                device::Event::Selection { id } => state.clipboard = id,
                device::Event::PrimarySelection { id } => state.primary = id,
                _ => {}
            }
        }
        wayland_client::event_created_child!(Events, device::ExtDataControlDeviceV1, [0 => (offer::ExtDataControlOfferV1, ())]);
    }
    impl Dispatch<source::ExtDataControlSourceV1, ()> for Events {
        fn event(
            state: &mut Self,
            _: &source::ExtDataControlSourceV1,
            event: source::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let source::Event::Send { fd, mime_type } = event {
                assert_eq!(mime_type, "text/uri-list");
                state.sends += 1;
                std::fs::File::from(fd)
                    .write_all(b"file:///tmp/native%20file\r\n")
                    .unwrap();
            }
        }
    }
    impl Dispatch<lock::ExtSessionLockV1, ()> for Events {
        fn event(
            state: &mut Self,
            _: &lock::ExtSessionLockV1,
            event: lock::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let lock::Event::Locked = event {
                state.locked = true;
            }
        }
    }

    struct Server {
        stop: mpsc::Sender<()>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Server {
        fn new() -> (Self, UnixStream) {
            let (server, client) = UnixStream::pair().unwrap();
            let (stop, stopped) = mpsc::channel();
            let thread = std::thread::spawn(move || {
                let mut display = wayland_server::Display::<RixState>::new().unwrap();
                let mut state = RixState::new(&mut display, "data-control-test".into());
                state
                    .seat
                    .add_keyboard(Default::default(), 200, 30)
                    .unwrap();
                state.seat.add_pointer();
                let _client = display
                    .handle()
                    .insert_client(server, Arc::new(crate::state::ClientState::default()))
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                while stopped.try_recv().is_err() && Instant::now() < deadline {
                    display.dispatch_clients(&mut state).unwrap();
                    display.flush_clients().unwrap();
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
            (
                Self {
                    stop,
                    thread: Some(thread),
                },
                client,
            )
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.stop.send(());
            self.thread.take().unwrap().join().unwrap();
        }
    }
    fn receive(
        selection: &offer::ExtDataControlOfferV1,
        queue: &mut wayland_client::EventQueue<Events>,
        events: &mut Events,
    ) -> Vec<u8> {
        let (mut reader, writer) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        selection.receive("text/uri-list".into(), writer.as_fd());
        drop(writer);
        queue.roundtrip(events).unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        bytes
    }
    #[test]
    fn native_file_clipboard_exchange_and_retained_offers_obey_session_lock() {
        let (_server, socket) = Server::new();
        let connection = Connection::from_socket(socket).unwrap();
        let (globals, mut queue) = registry_queue_init::<Events>(&connection).unwrap();
        let qh = queue.handle();
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).unwrap();
        let manager: manager::ExtDataControlManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
        let device = manager.get_data_device(&seat, &qh, ());
        let clipboard = manager.create_data_source(&qh, ());
        clipboard.offer("text/uri-list".into());
        let primary = manager.create_data_source(&qh, ());
        primary.offer("text/uri-list".into());
        device.set_selection(Some(&clipboard));
        device.set_primary_selection(Some(&primary));
        let mut events = Events::default();
        queue.roundtrip(&mut events).unwrap();
        let clipboard_offer = events.clipboard.clone().expect("native clipboard offer");
        let primary_offer = events.primary.clone().expect("native primary offer");
        assert_eq!(
            receive(&clipboard_offer, &mut queue, &mut events),
            b"file:///tmp/native%20file\r\n"
        );
        assert_eq!(
            receive(&primary_offer, &mut queue, &mut events),
            b"file:///tmp/native%20file\r\n"
        );
        let manager: lock_manager::ExtSessionLockManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
        let lock = manager.lock(&qh, ());
        queue.roundtrip(&mut events).unwrap();
        assert!(events.locked);
        let sends = events.sends;
        assert!(receive(&clipboard_offer, &mut queue, &mut events).is_empty());
        assert!(receive(&primary_offer, &mut queue, &mut events).is_empty());
        assert_eq!(
            events.sends, sends,
            "retained offer must never forward a read while locked"
        );
        lock.unlock_and_destroy();
        queue.roundtrip(&mut events).unwrap();
        assert_eq!(
            receive(&clipboard_offer, &mut queue, &mut events),
            b"file:///tmp/native%20file\r\n"
        );
        assert_eq!(
            receive(&primary_offer, &mut queue, &mut events),
            b"file:///tmp/native%20file\r\n"
        );
    }
}
