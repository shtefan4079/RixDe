use super::*;
use smithay::{
    backend::input::{ButtonState, KeyState},
    input::pointer::MotionEvent,
    utils::{Logical, Point, SERIAL_COUNTER},
};
use wayland_client::{
    protocol::{wl_pointer, wl_seat},
    WEnum,
};

#[derive(Default)]
struct PointerEvents {
    buttons: Vec<(u32, bool)>,
    motions: usize,
}

impl Dispatch<wl_pointer::WlPointer, Arc<Mutex<PointerEvents>>> for Events {
    fn event(
        _: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        events: &Arc<Mutex<PointerEvents>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut events = events.lock().unwrap();
        match event {
            wl_pointer::Event::Button {
                serial,
                button: 0x110,
                state: WEnum::Value(state),
                ..
            } => {
                events
                    .buttons
                    .push((serial, state == wl_pointer::ButtonState::Pressed));
            }
            wl_pointer::Event::Motion { .. } => events.motions += 1,
            _ => {}
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum FullscreenPhase {
    Windowed,
    Entering,
    Committed,
    Leaving,
}

fn prepare(
    phase: FullscreenPhase,
) -> (
    TestDesktop,
    xdg_toplevel::XdgToplevel,
    wl_seat::WlSeat,
    wl_pointer::WlPointer,
    Arc<Mutex<PointerEvents>>,
) {
    let mut desktop = TestDesktop::new();
    let window = desktop.window("Disposable fullscreen input fixture");
    let normal_buffer = desktop.sized_buffer(320, 200);
    window.0.attach(Some(&normal_buffer), 0, 0);
    window.0.commit();
    desktop.roundtrip();
    desktop.inspect(|state| {
        state.seat.add_pointer();
        let window = state.windows.entries[0].1.clone();
        state.space.map_element(window, (100, 150), false);
        state.space.refresh();
    });
    if !matches!(phase, FullscreenPhase::Windowed) {
        window.2.set_fullscreen(None);
        desktop.roundtrip();
        if matches!(phase, FullscreenPhase::Committed | FullscreenPhase::Leaving) {
            let fullscreen_buffer = desktop.sized_buffer(1000, 700);
            window.0.attach(Some(&fullscreen_buffer), 0, 0);
            window.0.commit();
            desktop.roundtrip();
        }
        if matches!(phase, FullscreenPhase::Leaving) {
            window.2.unset_fullscreen();
            desktop.roundtrip();
        }
    }
    desktop.inspect(move |state| {
        let toplevel = state.windows.entries[0].1.toplevel().unwrap();
        let current = toplevel
            .current_state()
            .states
            .contains(wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen);
        let pending = toplevel.with_pending_state(|state| {
            state
                .states
                .contains(wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen)
        });
        assert_eq!(
            (current, pending),
            match phase {
                FullscreenPhase::Windowed => (false, false),
                FullscreenPhase::Entering => (false, true),
                FullscreenPhase::Committed => (true, true),
                FullscreenPhase::Leaving => (true, false),
            }
        );
    });
    let qh = desktop.queue.handle();
    let seat: wl_seat::WlSeat = desktop.globals.bind(&qh, 1..=7, ()).unwrap();
    let events = Arc::new(Mutex::new(PointerEvents::default()));
    let pointer = seat.get_pointer(&qh, events.clone());
    desktop.roundtrip();
    (desktop, window.2, seat, pointer, events)
}

fn motion(state: &mut RixState, position: Point<f64, Logical>) {
    let pointer = state.seat.get_pointer().unwrap();
    pointer.motion(
        state,
        state.surface_under(position),
        &MotionEvent {
            location: position,
            serial: SERIAL_COUNTER.next_serial(),
            time: 10,
        },
    );
    pointer.frame(state);
}

fn button(state: &mut RixState, pressed: bool) {
    state.handle_pointer_button(
        0x110,
        if pressed {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        },
        SERIAL_COUNTER.next_serial(),
        11,
    );
}

fn has_move_grab(state: &RixState) -> bool {
    state
        .seat
        .get_pointer()
        .unwrap()
        .with_grab(|_, grab| {
            grab.downcast_ref::<crate::grabs::MoveSurfaceGrab>()
                .is_some()
        })
        .unwrap_or(false)
}

#[test]
fn fullscreen_modifier_clicks_preserve_geometry_and_reach_the_client() {
    for code in [64u32, 133] {
        for phase in [
            FullscreenPhase::Entering,
            FullscreenPhase::Committed,
            FullscreenPhase::Leaving,
        ] {
            let (mut desktop, _toplevel, _seat, _pointer, events) = prepare(phase);
            desktop.inspect(move |state| {
                state.handle_keyboard_key(
                    code.into(),
                    KeyState::Pressed,
                    SERIAL_COUNTER.next_serial(),
                    1,
                    false,
                );
                let window = state.windows.entries[0].1.clone();
                let location = state.space.element_location(&window).unwrap();
                let geometry = state.space.element_geometry(&window);
                let start = location.to_f64() + Point::from((40., 40.));
                motion(state, start);
                button(state, true);
                assert!(!has_move_grab(state), "{phase:?}, modifier {code}");
                motion(state, start + Point::from((70., 30.)));
                button(state, false);
                assert_eq!(state.space.element_location(&window), Some(location));
                assert_eq!(state.space.element_geometry(&window), geometry);
                assert!(crate::window_manager::is_fullscreen(&window));
                state.handle_keyboard_key(
                    code.into(),
                    KeyState::Released,
                    SERIAL_COUNTER.next_serial(),
                    12,
                    false,
                );
            });
            desktop.roundtrip();
            let events = events.lock().unwrap();
            assert_eq!(
                events
                    .buttons
                    .iter()
                    .map(|(_, pressed)| *pressed)
                    .collect::<Vec<_>>(),
                [true, false],
                "{phase:?}, modifier {code}"
            );
            assert!(
                events.motions >= 1,
                "fullscreen client must keep receiving motion"
            );
        }
    }
}

#[test]
fn windowed_modifier_drag_still_works_and_stops_on_fullscreen_entry() {
    for code in [64u32, 133] {
        let (mut desktop, toplevel, _seat, _pointer, _events) = prepare(FullscreenPhase::Windowed);
        desktop.inspect(move |state| {
            state.handle_keyboard_key(
                code.into(),
                KeyState::Pressed,
                SERIAL_COUNTER.next_serial(),
                1,
                false,
            );
            motion(state, (140., 190.).into());
            button(state, true);
            assert!(has_move_grab(state));
            motion(state, (210., 220.).into());
            assert_eq!(
                state.space.element_location(&state.windows.entries[0].1),
                Some((170, 180).into())
            );
        });
        toplevel.set_fullscreen(None);
        desktop.roundtrip();
        desktop.inspect(move |state| {
            let window = state.windows.entries[0].1.clone();
            let location = state.space.element_location(&window);
            assert_eq!(location, Some((0, 0).into()));
            motion(state, (250., 250.).into());
            assert!(!has_move_grab(state));
            assert_eq!(state.space.element_location(&window), location);
            button(state, false);
            state.handle_keyboard_key(
                code.into(),
                KeyState::Released,
                SERIAL_COUNTER.next_serial(),
                12,
                false,
            );
        });
    }
}

#[test]
fn fullscreen_client_move_and_resize_requests_cannot_change_geometry() {
    for phase in [
        FullscreenPhase::Entering,
        FullscreenPhase::Committed,
        FullscreenPhase::Leaving,
    ] {
        for resize in [false, true] {
            let (mut desktop, toplevel, seat, _pointer, events) = prepare(phase);
            let (location, geometry, start) = desktop.inspect(|state| {
                let window = &state.windows.entries[0].1;
                let location = state.space.element_location(window).unwrap();
                let geometry = state.space.element_geometry(window);
                let start = location.to_f64() + Point::from((40., 40.));
                motion(state, start);
                button(state, true);
                (location, geometry, start)
            });
            desktop.roundtrip();
            let serial = events.lock().unwrap().buttons[0].0;
            if resize {
                toplevel.resize(&seat, serial, xdg_toplevel::ResizeEdge::BottomRight);
            } else {
                toplevel._move(&seat, serial);
            }
            desktop.roundtrip();
            desktop.inspect(move |state| {
                let pointer = state.seat.get_pointer().unwrap();
                assert!(!has_move_grab(state));
                assert!(!pointer
                    .with_grab(|_, grab| grab
                        .downcast_ref::<crate::grabs::ResizeSurfaceGrab>()
                        .is_some())
                    .unwrap_or(false));
                motion(state, start + Point::from((70., 30.)));
                button(state, false);
                let window = &state.windows.entries[0].1;
                assert_eq!(state.space.element_location(window), Some(location));
                assert_eq!(state.space.element_geometry(window), geometry);
                assert!(crate::window_manager::is_fullscreen(window));
            });
        }
    }
}

#[test]
fn fullscreen_entry_cancels_active_client_resize_and_its_configure_state() {
    use wayland_protocols::xdg::shell::server::xdg_toplevel::State;

    let (mut desktop, toplevel, seat, _pointer, events) = prepare(FullscreenPhase::Windowed);
    desktop.inspect(|state| {
        motion(state, (140., 190.).into());
        button(state, true);
    });
    desktop.roundtrip();
    let serial = events.lock().unwrap().buttons[0].0;
    toplevel.resize(&seat, serial, xdg_toplevel::ResizeEdge::BottomRight);
    desktop.roundtrip();
    desktop.inspect(|state| {
        assert!(state
            .seat
            .get_pointer()
            .unwrap()
            .with_grab(|_, grab| {
                grab.downcast_ref::<crate::grabs::ResizeSurfaceGrab>()
                    .is_some()
            })
            .unwrap_or(false));
        motion(state, (180., 230.).into());
        assert!(state.windows.entries[0]
            .1
            .toplevel()
            .unwrap()
            .with_pending_state(|pending| { pending.states.contains(State::Resizing) }));
    });
    toplevel.set_fullscreen(None);
    desktop.roundtrip();
    desktop.inspect(|state| {
        let window = state.windows.entries[0].1.clone();
        let toplevel = window.toplevel().unwrap();
        let location = state.space.element_location(&window);
        motion(state, (250., 250.).into());
        assert!(!state
            .seat
            .get_pointer()
            .unwrap()
            .with_grab(|_, grab| {
                grab.downcast_ref::<crate::grabs::ResizeSurfaceGrab>()
                    .is_some()
            })
            .unwrap_or(false));
        toplevel.with_pending_state(|pending| {
            assert!(pending.states.contains(State::Fullscreen));
            assert!(!pending.states.contains(State::Resizing));
            assert_eq!(pending.size, Some((1000, 700).into()));
        });
        assert_eq!(state.space.element_location(&window), location);
        assert!(state.windows.resize_placement.is_empty());
        button(state, false);
    });
}
