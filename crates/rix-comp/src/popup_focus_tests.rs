use super::*;
use smithay::{
    desktop::{PopupKeyboardGrab, PopupKind, PopupPointerGrab},
    input::pointer::{ButtonEvent, Focus, MotionEvent},
    utils::{Logical, Point, SERIAL_COUNTER},
};
use wayland_client::protocol::{wl_pointer, wl_seat};
use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_confined_pointer_v1, zwp_locked_pointer_v1, zwp_pointer_constraints_v1,
};
use wayland_protocols::xdg::shell::client::{xdg_popup, xdg_positioner};

wayland_client::delegate_noop!(Events: ignore xdg_positioner::XdgPositioner);
wayland_client::delegate_noop!(Events: ignore xdg_popup::XdgPopup);
wayland_client::delegate_noop!(Events: ignore wl_pointer::WlPointer);
wayland_client::delegate_noop!(Events: ignore zwp_pointer_constraints_v1::ZwpPointerConstraintsV1);
wayland_client::delegate_noop!(Events: ignore zwp_locked_pointer_v1::ZwpLockedPointerV1);
wayland_client::delegate_noop!(Events: ignore zwp_confined_pointer_v1::ZwpConfinedPointerV1);

#[test]
fn ipc_activation_takes_focus_before_launcher_popup_is_destroyed() {
    let mut desktop = TestDesktop::new();
    let launcher_parent = desktop.window("Disposable launcher parent");
    let target = desktop.window("Disposable activation target");
    let target_id = desktop.inspect(|state| {
        state.seat.add_pointer();
        state.windows.entries[1].0
    });
    desktop.action(rix_ipc::WindowAction::Minimize(target_id));
    let qh = desktop.queue.handle();
    let seat: wl_seat::WlSeat = desktop.globals.bind(&qh, 1..=7, ()).unwrap();
    let positioner = desktop.wm.create_positioner(&qh, ());
    positioner.set_size(120, 80);
    positioner.set_anchor_rect(0, 0, 16, 16);
    let surface = desktop.compositor.create_surface(&qh, ());
    let xdg = desktop.wm.get_xdg_surface(&surface, &qh, ());
    let popup = xdg.get_popup(Some(&launcher_parent.1), &positioner, &qh, ());
    popup.grab(&seat, 777);
    surface.commit();
    desktop.roundtrip();

    desktop.inspect(move |state| {
        let keyboard = state.seat.get_keyboard().unwrap();
        let pointer = state.seat.get_pointer().unwrap();
        let popup = state
            .xdg_shell_state
            .popup_surfaces()
            .last()
            .unwrap()
            .clone();
        let target = state.windows.entries[1].1.clone();
        let target_surface = target.toplevel().unwrap().wl_surface().clone();
        assert_eq!(keyboard.current_focus(), Some(popup.wl_surface().clone()));
        assert!(keyboard.is_grabbed());
        assert!(pointer.is_grabbed());
        state
            .apply_window_action(rix_ipc::WindowAction::Focus(target_id))
            .unwrap();
        state.apply_pending_pointer_constraint_focus();
        assert_eq!(keyboard.current_focus(), Some(target_surface));
        assert_eq!(state.windows.active, Some(target_id));
        assert!(!state.windows.minimized.contains_key(&target_id));
        assert!(state.space.element_location(&target).is_some());
        assert!(!keyboard.is_grabbed());
        assert!(!pointer.is_grabbed());
        assert!(smithay::utils::IsAlive::alive(&PopupKind::Xdg(popup)));
    });
    desktop.roundtrip();
    popup.destroy();
    xdg.destroy();
    surface.destroy();
    desktop.roundtrip();
    desktop.inspect(move |state| {
        let pointer = state.seat.get_pointer().unwrap();
        pointer.motion(
            state,
            None,
            &MotionEvent {
                location: Point::from((900., 650.)),
                serial: SERIAL_COUNTER.next_serial(),
                time: 120,
            },
        );
        state.apply_pending_pointer_constraint_focus();
        let keyboard = state.seat.get_keyboard().unwrap();
        let target = &state.windows.entries[1].1;
        assert_eq!(
            keyboard.current_focus(),
            Some(target.toplevel().unwrap().wl_surface().clone())
        );
        assert_eq!(state.windows.active, Some(target_id));
    });
    target.0.commit();
    desktop.roundtrip();
}

#[test]
fn popup_dismissal_restores_focus_then_allows_server_titlebar_drag() {
    let mut desktop = TestDesktop::new();
    let files = desktop.window("Disposable Files");
    let terminal = desktop.window("Disposable terminal");
    let qh = desktop.queue.handle();
    let decorations: zxdg_decoration_manager_v1::ZxdgDecorationManagerV1 =
        desktop.globals.bind(&qh, 1..=1, ()).unwrap();
    let decoration = decorations.get_toplevel_decoration(&terminal.2, &qh, ());
    decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
    desktop.roundtrip();
    let shm: wl_shm::WlShm = desktop.globals.bind(&qh, 1..=1, ()).unwrap();
    let file = tempfile::tempfile().unwrap();
    file.set_len(600 * 400 * 4).unwrap();
    let pool = shm.create_pool(file.as_fd(), 600 * 400 * 4, &qh, ());
    let buffer = pool.create_buffer(0, 600, 400, 600 * 4, wl_shm::Format::Argb8888, &qh, ());
    terminal.0.attach(Some(&buffer), 0, 0);
    terminal.0.commit();
    desktop.roundtrip();

    let positioner = desktop.wm.create_positioner(&qh, ());
    positioner.set_size(120, 80);
    positioner.set_anchor_rect(0, 0, 16, 16);
    let popup_surface = desktop.compositor.create_surface(&qh, ());
    let popup_xdg = desktop.wm.get_xdg_surface(&popup_surface, &qh, ());
    let _popup = popup_xdg.get_popup(Some(&files.1), &positioner, &qh, ());

    desktop.roundtrip();

    desktop.inspect_with_timeout(Duration::from_secs(2), |state| {
        let settings = serde_json::from_str(include_str!(
            "../../../assets/rix/themes/fairywren-dark/theme.json"
        ))
        .unwrap();
        state.set_decoration_theme(&settings);
        let pointer = state.seat.add_pointer();
        let keyboard = state.seat.get_keyboard().unwrap();
        let root = state.windows.entries[0]
            .1
            .toplevel()
            .unwrap()
            .wl_surface()
            .clone();
        let popup = state
            .xdg_shell_state
            .popup_surfaces()
            .last()
            .unwrap()
            .clone();
        let serial = SERIAL_COUNTER.next_serial();
        let grab = state
            .popups
            .grab_popup(
                root.clone(),
                PopupKind::Xdg(popup.clone()),
                &state.seat,
                serial,
            )
            .unwrap();
        keyboard.set_grab(state, PopupKeyboardGrab::new(&grab), serial);
        keyboard.set_focus(state, grab.current_grab(), serial);
        pointer.set_grab(state, PopupPointerGrab::new(&grab), serial, Focus::Clear);
        assert_eq!(keyboard.current_focus(), Some(popup.wl_surface().clone()));
        assert!(pointer.is_grabbed());

        pointer.button(
            state,
            &ButtonEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time: 100,
                button: 0x110,
                state: smithay::backend::input::ButtonState::Pressed,
            },
        );
        state.apply_pending_pointer_constraint_focus();
        assert_eq!(keyboard.current_focus(), Some(root));
        assert!(!pointer.is_grabbed());
        pointer.button(
            state,
            &ButtonEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time: 101,
                button: 0x110,
                state: smithay::backend::input::ButtonState::Released,
            },
        );

        let window = state.windows.entries[1].1.clone();
        let location = state.space.element_location(&window).unwrap();
        let body = state.space.element_geometry(&window).unwrap();
        let theme = state.decoration_for_window(&window);
        let titlebar = Point::<f64, Logical>::from((
            body.loc.x as f64 + 100.,
            body.loc.y as f64 - theme.height as f64 / 2.,
        ));
        assert_eq!(
            state.decoration_under(titlebar).unwrap().1,
            crate::decoration::DecorationHit::Move
        );
        super::decoration_pointer(state, titlebar, Some(true));
        state.apply_pending_pointer_constraint_focus();
        assert!(pointer.is_grabbed());
        let moved = titlebar + Point::from((40., 30.));
        pointer.motion(
            state,
            None,
            &MotionEvent {
                location: moved,
                serial: SERIAL_COUNTER.next_serial(),
                time: 110,
            },
        );
        assert_eq!(
            state.space.element_location(&window),
            Some(location + Point::from((40, 30)))
        );
        super::decoration_pointer(state, moved, Some(false));
        assert!(!pointer.is_grabbed());
    });
}

#[test]
fn popup_grab_survives_pointer_device_removal_and_activation_dismisses_it() {
    let mut desktop = TestDesktop::new();
    let parent = desktop.window("Disposable popup owner");
    let other = desktop.window("Disposable activation target");
    let qh = desktop.queue.handle();
    let positioner = desktop.wm.create_positioner(&qh, ());
    positioner.set_size(120, 80);
    positioner.set_anchor_rect(0, 0, 16, 16);
    let popup_surface = desktop.compositor.create_surface(&qh, ());
    let popup_xdg = desktop.wm.get_xdg_surface(&popup_surface, &qh, ());
    let _popup = popup_xdg.get_popup(Some(&parent.1), &positioner, &qh, ());
    desktop.roundtrip();

    desktop.inspect(|state| {
        let pointer = state.seat.add_pointer();
        let keyboard = state.seat.get_keyboard().unwrap();
        let root = state.windows.entries[0]
            .1
            .toplevel()
            .unwrap()
            .wl_surface()
            .clone();
        let popup = state
            .xdg_shell_state
            .popup_surfaces()
            .last()
            .unwrap()
            .clone();
        let serial = SERIAL_COUNTER.next_serial();
        let grab = state
            .popups
            .grab_popup(root, PopupKind::Xdg(popup.clone()), &state.seat, serial)
            .unwrap();
        keyboard.set_grab(state, PopupKeyboardGrab::new(&grab), serial);
        keyboard.set_focus(state, grab.current_grab(), serial);
        pointer.set_grab(state, PopupPointerGrab::new(&grab), serial, Focus::Clear);
        state.active_popup_grab = Some(grab);

        pointer.unset_grab(state, SERIAL_COUNTER.next_serial(), 100);
        assert!(!pointer.is_grabbed());
        assert!(
            state.popups.find_popup(popup.wl_surface()).is_some(),
            "popup tree intact"
        );

        state.prune_popup_grab();
        assert!(
            state.active_popup_grab.is_some(),
            "device removal must not be mistaken for a popup dismissal"
        );

        let (other_id, _) = state.windows.entries[1].clone();
        state
            .apply_window_action(rix_ipc::WindowAction::Focus(other_id))
            .unwrap();
        assert!(
            state.active_popup_grab.is_none(),
            "activation releases the surviving handle"
        );
        assert!(
            state.popups.find_popup(popup.wl_surface()).is_none(),
            "activation sent the popup its dismissal"
        );
    });
    desktop.roundtrip();
    other.0.commit();
    desktop.roundtrip();
}

#[test]
fn keyboard_grab_release_alone_keeps_the_popup_handle() {
    let mut desktop = TestDesktop::new();
    let parent = desktop.window("Disposable popup owner");
    let other = desktop.window("Disposable activation target");
    let qh = desktop.queue.handle();
    let positioner = desktop.wm.create_positioner(&qh, ());
    positioner.set_size(120, 80);
    positioner.set_anchor_rect(0, 0, 16, 16);
    let popup_surface = desktop.compositor.create_surface(&qh, ());
    let popup_xdg = desktop.wm.get_xdg_surface(&popup_surface, &qh, ());
    let _popup = popup_xdg.get_popup(Some(&parent.1), &positioner, &qh, ());
    desktop.roundtrip();

    desktop.inspect(|state| {
        let pointer = state.seat.add_pointer();
        let keyboard = state.seat.get_keyboard().unwrap();
        let root = state.windows.entries[0]
            .1
            .toplevel()
            .unwrap()
            .wl_surface()
            .clone();
        let popup = state
            .xdg_shell_state
            .popup_surfaces()
            .last()
            .unwrap()
            .clone();
        let serial = SERIAL_COUNTER.next_serial();
        let grab = state
            .popups
            .grab_popup(root, PopupKind::Xdg(popup.clone()), &state.seat, serial)
            .unwrap();
        keyboard.set_grab(state, PopupKeyboardGrab::new(&grab), serial);
        keyboard.set_focus(state, grab.current_grab(), serial);
        pointer.set_grab(state, PopupPointerGrab::new(&grab), serial, Focus::Clear);
        state.active_popup_grab = Some(grab);

        keyboard.unset_grab(state);
        assert!(!keyboard.is_grabbed() && pointer.is_grabbed());

        state.prune_popup_grab();
        assert!(
            state.active_popup_grab.is_some(),
            "a live pointer grab keeps the popup handle"
        );

        let (other_id, _) = state.windows.entries[1].clone();
        state
            .apply_window_action(rix_ipc::WindowAction::Focus(other_id))
            .unwrap();
        assert!(
            state.active_popup_grab.is_none(),
            "the full dismissal drops the handle"
        );
        assert!(
            !pointer.is_grabbed(),
            "activation must release the live pointer grab through the handle"
        );
        assert!(state.popups.find_popup(popup.wl_surface()).is_none());
    });
    desktop.roundtrip();
    other.0.commit();
    desktop.roundtrip();
}

#[test]
fn stale_popup_grab_is_pruned_after_smithay_dismisses_it_on_click_outside() {
    let mut desktop = TestDesktop::new();
    let parent = desktop.window("Disposable popup owner");
    let qh = desktop.queue.handle();
    let positioner = desktop.wm.create_positioner(&qh, ());
    positioner.set_size(120, 80);
    positioner.set_anchor_rect(0, 0, 16, 16);
    let popup_surface = desktop.compositor.create_surface(&qh, ());
    let popup_xdg = desktop.wm.get_xdg_surface(&popup_surface, &qh, ());
    let _popup = popup_xdg.get_popup(Some(&parent.1), &positioner, &qh, ());
    desktop.roundtrip();

    desktop.inspect(|state| {
        let pointer = state.seat.add_pointer();
        let keyboard = state.seat.get_keyboard().unwrap();
        let root = state.windows.entries[0]
            .1
            .toplevel()
            .unwrap()
            .wl_surface()
            .clone();
        let popup = state
            .xdg_shell_state
            .popup_surfaces()
            .last()
            .unwrap()
            .clone();
        let serial = SERIAL_COUNTER.next_serial();
        let grab = state
            .popups
            .grab_popup(root, PopupKind::Xdg(popup), &state.seat, serial)
            .unwrap();
        keyboard.set_grab(state, PopupKeyboardGrab::new(&grab), serial);
        keyboard.set_focus(state, grab.current_grab(), serial);
        pointer.set_grab(state, PopupPointerGrab::new(&grab), serial, Focus::Clear);
        state.active_popup_grab = Some(grab);

        state.prune_popup_grab();
        assert!(
            state.active_popup_grab.is_some(),
            "a live popup grab must survive pruning"
        );
        assert!(pointer.is_grabbed() && keyboard.is_grabbed());

        pointer.button(
            state,
            &ButtonEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time: 100,
                button: 0x110,
                state: smithay::backend::input::ButtonState::Pressed,
            },
        );
        assert!(!pointer.is_grabbed(), "Smithay releases the pointer grab");
        assert!(!keyboard.is_grabbed(), "Smithay releases the keyboard grab");
        assert!(
            state.active_popup_grab.is_some(),
            "the compositor handle is left behind"
        );

        state.prune_popup_grab();
        assert!(
            state.active_popup_grab.is_none(),
            "maintenance must drop the stale grab"
        );
        pointer.button(
            state,
            &ButtonEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time: 101,
                button: 0x110,
                state: smithay::backend::input::ButtonState::Released,
            },
        );
    });
}

fn active_constraint(state: &RixState) -> bool {
    let pointer = state.seat.get_pointer().unwrap();
    let surface = pointer.current_focus().unwrap();
    smithay::wayland::pointer_constraints::with_pointer_constraint(
        &surface,
        &pointer,
        |constraint| constraint.is_some_and(|constraint| constraint.is_active()),
    )
}

#[test]
fn native_constraints_release_on_focus_none_and_unmapped_window() {
    for confined in [false, true] {
        let mut desktop = TestDesktop::new();
        let terminal = desktop.window("Disposable constrained terminal");
        desktop.inspect(|state| {
            let pointer = state.seat.add_pointer();
            let window = state.windows.entries[0].1.clone();
            let body = state.space.element_geometry(&window).unwrap();
            let position = body.loc.to_f64() + Point::<f64, Logical>::from((4., 4.));
            pointer.motion(
                state,
                state.surface_under(position),
                &MotionEvent {
                    location: position,
                    serial: SERIAL_COUNTER.next_serial(),
                    time: 1,
                },
            );
            state.focus_window(&window);
            state.apply_pending_pointer_constraint_focus();
        });
        let qh = desktop.queue.handle();
        let seat: wl_seat::WlSeat = desktop.globals.bind(&qh, 1..=7, ()).unwrap();
        let pointer = seat.get_pointer(&qh, ());
        let manager: zwp_pointer_constraints_v1::ZwpPointerConstraintsV1 =
            desktop.globals.bind(&qh, 1..=1, ()).unwrap();
        let _locked = (!confined).then(|| {
            manager.lock_pointer(
                &terminal.0,
                &pointer,
                None,
                zwp_pointer_constraints_v1::Lifetime::Persistent,
                &qh,
                (),
            )
        });
        let _confined = confined.then(|| {
            manager.confine_pointer(
                &terminal.0,
                &pointer,
                None,
                zwp_pointer_constraints_v1::Lifetime::Persistent,
                &qh,
                (),
            )
        });
        desktop.roundtrip();
        desktop.inspect(|state| {
            assert!(
                active_constraint(state),
                "Native constraint must be active before focus loss"
            );
            let keyboard = state.seat.get_keyboard().unwrap();
            let window = state.windows.entries[0].1.clone();
            let surface = window.toplevel().unwrap().wl_surface().clone();

            keyboard.set_focus(state, None, SERIAL_COUNTER.next_serial());
            keyboard.set_focus(state, Some(surface.clone()), SERIAL_COUNTER.next_serial());
            keyboard.set_focus(state, None, SERIAL_COUNTER.next_serial());
            state.apply_pending_pointer_constraint_focus();
            assert!(
                !active_constraint(state),
                "Queued positive focus must not override a later None"
            );

            keyboard.set_focus(state, Some(surface.clone()), SERIAL_COUNTER.next_serial());
            state.apply_pending_pointer_constraint_focus();
            assert!(active_constraint(state));

            state.space.unmap_elem(&window);
            state.space.refresh();
            keyboard.set_focus(state, None, SERIAL_COUNTER.next_serial());
            state.apply_pending_pointer_constraint_focus();
            assert!(
                !active_constraint(state),
                "Unmapped pointer owner must release capture on focus loss"
            );
        });
    }
}
