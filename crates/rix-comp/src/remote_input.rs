use crate::state::{ClientState, RixState};
use rix_protocols::remote_input_server::{
    rix_remote_input_manager_v1::{self as manager, RixRemoteInputManagerV1},
    rix_remote_input_session_v1::{self as session, RixRemoteInputSessionV1},
};
use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, KeyState},
    input::{
        keyboard::xkb,
        pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent},
    },
    utils::{Logical, Point, SERIAL_COUNTER},
};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

const MAX_SESSIONS: usize = 4;
const MAX_HELD_KEYS: usize = 64;
const MAX_EVENTS_PER_SECOND: u32 = 2048;

#[derive(Default)]
struct Ownership {
    physical: HashSet<u32>,
    remote: HashMap<u32, usize>,
}
impl Ownership {
    fn held(&self, code: u32) -> bool {
        self.physical.contains(&code) || self.remote.contains_key(&code)
    }
    fn physical(&mut self, code: u32, pressed: bool) -> bool {
        let before = self.held(code);
        if pressed {
            self.physical.insert(code);
        } else {
            self.physical.remove(&code);
        }
        before != self.held(code)
    }
    fn acquire(&mut self, code: u32) -> bool {
        let before = self.held(code);
        *self.remote.entry(code).or_default() += 1;
        !before
    }
    fn release(&mut self, code: u32) -> bool {
        let Some(count) = self.remote.get_mut(&code) else {
            return false;
        };
        *count -= 1;
        if *count == 0 {
            self.remote.remove(&code);
        }
        !self.held(code)
    }
}

struct Session {
    resource: RixRemoteInputSessionV1,
    keys: HashMap<u32, usize>,
    direct_keys: HashSet<u32>,
    symbols: HashMap<u32, Vec<u32>>,
    buttons: HashSet<u32>,
    rate_started: Instant,
    events: u32,
}

#[derive(Default)]
pub(crate) struct RemoteInputState {
    sessions: HashMap<u64, Session>,
    next_id: u64,
    keys: Ownership,
    buttons: Ownership,
    pub(crate) injecting: bool,
}
pub(crate) struct SessionData {
    id: u64,
}

pub(crate) fn install(display: &DisplayHandle) {
    display.create_global::<RixState, RixRemoteInputManagerV1, _>(1, ());
}
fn permitted(client: &Client) -> bool {
    client.get_data::<ClientState>().is_some_and(|data| {
        data.allow_remote_input && !data.disconnected.load(std::sync::atomic::Ordering::Acquire)
    })
}
pub(crate) fn refresh(state: &mut RixState) {
    let stale: Vec<_> = state
        .remote_input
        .sessions
        .iter()
        .filter(|(_, session)| {
            !session.resource.is_alive()
                || session
                    .resource
                    .client()
                    .is_none_or(|client| !permitted(&client))
                || state.lock.active
                || state.lock.requested
        })
        .map(|(id, _)| *id)
        .collect();
    let reason = if state.lock.active || state.lock.requested {
        session::CloseReason::Locked
    } else {
        session::CloseReason::Revoked
    };
    for id in stale {
        close(state, id, reason);
    }
}
pub(crate) fn revoke_all(state: &mut RixState, reason: session::CloseReason) {
    let ids: Vec<_> = state.remote_input.sessions.keys().copied().collect();
    for id in ids {
        close(state, id, reason);
    }
}
pub(crate) fn cancel_devices(state: &mut RixState) {
    revoke_all(state, session::CloseReason::Revoked);
    state.remote_input.keys.physical.clear();
    state.remote_input.buttons.physical.clear();
}
fn close(state: &mut RixState, id: u64, reason: session::CloseReason) {
    let Some(held) = state.remote_input.sessions.remove(&id) else {
        return;
    };
    let time = state.start_time.elapsed().as_millis() as u32;
    for (code, count) in held.keys {
        for _ in 0..count {
            if state.remote_input.keys.release(code) {
                state.release_remote_key(code.into(), time);
            }
        }
    }
    for button in held.buttons {
        if state.remote_input.buttons.release(button) {
            send_button(state, button, ButtonState::Released, time);
        }
    }
    if held.resource.is_alive() {
        held.resource.closed(reason);
    }
}

impl RemoteInputState {
    pub(crate) fn physical_key(&mut self, code: u32, state: KeyState) -> bool {
        self.keys.physical(code, state == KeyState::Pressed)
    }
}
impl RixState {

    pub(crate) fn handle_pointer_button(
        &mut self,
        button: u32,
        state: ButtonState,
        serial: smithay::utils::Serial,
        time: u32,
    ) {
        if !self
            .remote_input
            .buttons
            .physical(button, state == ButtonState::Pressed)
        {
            return;
        }
        pointer_button(self, button, state, serial, time);
    }
}
fn pointer_button(
    state: &mut RixState,
    button: u32,
    pressed: ButtonState,
    serial: smithay::utils::Serial,
    time: u32,
) {
    if let Some(pointer) = state.seat.get_pointer() {
        pointer.button(
            state,
            &ButtonEvent {
                serial,
                time,
                button,
                state: pressed,
            },
        );
        if pressed == ButtonState::Pressed {
            state.pointer_pressed(button, serial, time);
        } else {
            state.pointer_released(button);
        }
        pointer.frame(state);
    }
}
fn send_button(state: &mut RixState, button: u32, pressed: ButtonState, time: u32) {
    pointer_button(state, button, pressed, SERIAL_COUNTER.next_serial(), time);
}
fn send_key(state: &mut RixState, code: u32, pressed: KeyState) {
    state.remote_input.injecting = true;
    state.handle_keyboard_key(
        code.into(),
        pressed,
        SERIAL_COUNTER.next_serial(),
        state.start_time.elapsed().as_millis() as u32,
        true,
    );
    state.remote_input.injecting = false;
}
fn acquire_key(state: &mut RixState, id: u64, code: u32) {
    let Some(held) = state.remote_input.sessions.get_mut(&id) else {
        return;
    };
    *held.keys.entry(code).or_default() += 1;
    if state.remote_input.keys.acquire(code) {
        send_key(state, code, KeyState::Pressed);
    }
}
fn release_key(state: &mut RixState, id: u64, code: u32) {
    let Some(held) = state.remote_input.sessions.get_mut(&id) else {
        return;
    };
    let Some(count) = held.keys.get_mut(&code) else {
        return;
    };
    *count -= 1;
    if *count == 0 {
        held.keys.remove(&code);
    }
    if state.remote_input.keys.release(code) {
        send_key(state, code, KeyState::Released);
    }
}

fn keysym_plan(state: &mut RixState, symbol: u32) -> Option<Vec<u32>> {
    let keyboard = state.seat.get_keyboard()?;
    keyboard.with_xkb_state(state, |context| {
        let xkb = context.xkb().lock().unwrap();

        let (keymap, current) = unsafe { (xkb.keymap(), xkb.state()) };
        let layout = xkb.active_layout().0;
        let mut modifiers = Vec::new();
        for modifier in [
            xkb::keysyms::KEY_Shift_L,
            xkb::keysyms::KEY_ISO_Level3_Shift,
        ] {
            let mut found = None;
            keymap.key_for_each(|map, code| {
                if found.is_none()
                    && map
                        .key_get_syms_by_level(code, layout, 0)
                        .iter()
                        .any(|sym| u32::from(*sym) == modifier)
                {
                    found = Some(code);
                }
            });
            modifiers.push(found);
        }
        for mask in 0..4 {
            let mut probe = xkb::State::new(keymap);
            probe.update_mask(
                current.serialize_mods(xkb::STATE_MODS_DEPRESSED),
                current.serialize_mods(xkb::STATE_MODS_LATCHED),
                current.serialize_mods(xkb::STATE_MODS_LOCKED),
                0,
                0,
                layout,
            );
            let mut extra = Vec::new();
            let mut supported = true;
            for (bit, code) in modifiers.iter().enumerate() {
                if mask & (1 << bit) != 0 {
                    if let Some(code) = code {
                        probe.update_key(*code, xkb::KeyDirection::Down);
                        extra.push(u32::from(*code));
                    } else {
                        supported = false;
                    }
                }
            }
            if !supported {
                continue;
            }
            let mut found = None;
            keymap.key_for_each(|_, code| {
                if found.is_none()
                    && u32::from(code) <= 775
                    && u32::from(probe.key_get_one_sym(code)) == symbol
                {
                    found = Some(u32::from(code));
                }
            });
            if let Some(code) = found {
                extra.push(code);
                return Some(extra);
            }
        }
        None
    })
}

fn motion(state: &mut RixState, x: f64, y: f64, absolute: bool) {
    if !x.is_finite() || !y.is_finite() {
        return;
    }
    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };
    let current = pointer.current_location();
    let time = state.start_time.elapsed();
    let delta: Point<f64, Logical> = (x.clamp(-10000., 10000.), y.clamp(-10000., 10000.)).into();
    if state.pointer_is_locked() {
        if !absolute {
            pointer.relative_motion(
                state,
                state.surface_under(current),
                &RelativeMotionEvent {
                    delta,
                    delta_unaccel: delta,
                    utime: time.as_micros() as u64,
                },
            );
        }
    } else if let Some((min_x, min_y, max_x, max_y)) = state.desktop_bounds() {
        let position = if absolute {
            Point::from((x, y))
        } else {
            current + delta
        };
        let position = state.constrain_pointer_location(Point::from((
            position.x.clamp(min_x, max_x - 1.),
            position.y.clamp(min_y, max_y - 1.),
        )));
        let focus = state.surface_under(position);
        if !absolute {
            pointer.relative_motion(
                state,
                focus.clone(),
                &RelativeMotionEvent {
                    delta,
                    delta_unaccel: delta,
                    utime: time.as_micros() as u64,
                },
            );
        }
        pointer.motion(
            state,
            focus,
            &MotionEvent {
                location: position,
                serial: SERIAL_COUNTER.next_serial(),
                time: time.as_millis() as u32,
            },
        );
        state.update_pointer_constraint();
    }
    pointer.frame(state);
}

impl GlobalDispatch<RixRemoteInputManagerV1, ()> for RixState {
    fn can_view(client: Client, _: &()) -> bool {
        permitted(&client)
    }
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<RixRemoteInputManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}
impl Dispatch<RixRemoteInputManagerV1, ()> for RixState {
    fn request(
        state: &mut Self,
        client: &Client,
        _: &RixRemoteInputManagerV1,
        request: manager::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        let manager::Request::CreateSession { session: resource } = request else {
            return;
        };
        state.remote_input.next_id = state.remote_input.next_id.wrapping_add(1);
        let id = state.remote_input.next_id;
        let resource = init.init(resource, SessionData { id });
        let reason = if state.greeter_mode || !permitted(client) {
            Some(session::CloseReason::Revoked)
        } else if state.lock.active || state.lock.requested {
            Some(session::CloseReason::Locked)
        } else if state.remote_input.sessions.len() >= MAX_SESSIONS {
            Some(session::CloseReason::Limit)
        } else {
            None
        };
        if let Some(reason) = reason {
            resource.closed(reason);
            return;
        }
        state.remote_input.sessions.insert(
            id,
            Session {
                resource,
                keys: Default::default(),
                direct_keys: Default::default(),
                symbols: Default::default(),
                buttons: Default::default(),
                rate_started: Instant::now(),
                events: 0,
            },
        );
    }
}
impl Dispatch<RixRemoteInputSessionV1, SessionData> for RixState {
    fn request(
        state: &mut Self,
        client: &Client,
        _: &RixRemoteInputSessionV1,
        request: session::Request,
        data: &SessionData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if matches!(request, session::Request::Destroy) {
            close(state, data.id, session::CloseReason::Revoked);
            return;
        }
        if state.lock.active || state.lock.requested || state.greeter_mode || !permitted(client) {
            close(state, data.id, session::CloseReason::Locked);
            return;
        }
        let Some(held) = state.remote_input.sessions.get_mut(&data.id) else {
            return;
        };
        if held.rate_started.elapsed() >= Duration::from_secs(1) {
            held.rate_started = Instant::now();
            held.events = 0;
        }
        held.events += 1;
        if held.events > MAX_EVENTS_PER_SECOND {
            close(state, data.id, session::CloseReason::Limit);
            return;
        }
        let time = state.start_time.elapsed().as_millis() as u32;
        state.idle.activity();
        match request {
            session::Request::Keycode {
                code,
                state: pressed,
            } => {
                if !(1..=767).contains(&code) || pressed > 1 {
                    close(state, data.id, session::CloseReason::Unsupported);
                    return;
                }
                let code = code + 8;
                let held = state.remote_input.sessions.get_mut(&data.id).unwrap();
                if pressed == 1 {
                    if held.keys.len() >= MAX_HELD_KEYS {
                        close(state, data.id, session::CloseReason::Limit);
                        return;
                    }
                    if held.direct_keys.insert(code) {
                        acquire_key(state, data.id, code);
                    }
                } else if held.direct_keys.remove(&code) {
                    release_key(state, data.id, code);
                }
            }
            session::Request::Keysym {
                sym,
                state: pressed,
            } => {
                if pressed > 1 || sym == 0 {
                    close(state, data.id, session::CloseReason::Unsupported);
                    return;
                }
                if pressed == 0 {
                    if let Some(plan) = state
                        .remote_input
                        .sessions
                        .get_mut(&data.id)
                        .unwrap()
                        .symbols
                        .remove(&sym)
                    {
                        for code in plan.into_iter().rev() {
                            release_key(state, data.id, code);
                        }
                    }
                } else if !state.remote_input.sessions[&data.id]
                    .symbols
                    .contains_key(&sym)
                {
                    if state.remote_input.sessions[&data.id].keys.len() + 3 > MAX_HELD_KEYS {
                        close(state, data.id, session::CloseReason::Limit);
                        return;
                    }
                    let Some(plan) = keysym_plan(state, sym) else {
                        state.remote_input.sessions[&data.id]
                            .resource
                            .unsupported_keysym(sym);
                        return;
                    };
                    state
                        .remote_input
                        .sessions
                        .get_mut(&data.id)
                        .unwrap()
                        .symbols
                        .insert(sym, plan.clone());
                    for code in plan {
                        acquire_key(state, data.id, code);
                    }
                }
            }
            session::Request::Motion { dx, dy } => motion(state, dx, dy, false),
            session::Request::MotionAbsolute { x, y } => motion(state, x, y, true),
            session::Request::Button {
                button,
                state: pressed,
            } => {
                if !(0x110..=0x117).contains(&button) || pressed > 1 {
                    close(state, data.id, session::CloseReason::Unsupported);
                    return;
                }
                let held = state.remote_input.sessions.get_mut(&data.id).unwrap();
                if pressed == 1 {
                    if held.buttons.insert(button) && state.remote_input.buttons.acquire(button) {
                        send_button(state, button, ButtonState::Pressed, time);
                    }
                } else if held.buttons.remove(&button) && state.remote_input.buttons.release(button)
                {
                    send_button(state, button, ButtonState::Released, time);
                }
            }
            session::Request::Axis { dx, dy } => {
                if dx.is_finite() && dy.is_finite() {
                    if let Some(pointer) = state.seat.get_pointer() {
                        pointer.axis(
                            state,
                            AxisFrame::new(time)
                                .source(AxisSource::Continuous)
                                .value(Axis::Horizontal, dx.clamp(-1000., 1000.))
                                .value(Axis::Vertical, dy.clamp(-1000., 1000.)),
                        );
                        pointer.frame(state);
                    }
                }
            }
            session::Request::AxisDiscrete { axis, steps } => {
                if axis > 1 {
                    close(state, data.id, session::CloseReason::Unsupported);
                    return;
                }
                if let Some(pointer) = state.seat.get_pointer() {
                    let axis = if axis == 0 {
                        Axis::Vertical
                    } else {
                        Axis::Horizontal
                    };
                    let steps = steps.clamp(-100, 100);
                    pointer.axis(
                        state,
                        AxisFrame::new(time)
                            .source(AxisSource::Wheel)
                            .value(axis, f64::from(steps) * 15.)
                            .v120(axis, steps * 120),
                    );
                    pointer.frame(state);
                }
            }
            _ => {}
        }
    }
    fn destroyed(
        state: &mut Self,
        _: wayland_server::backend::ClientId,
        _: &RixRemoteInputSessionV1,
        data: &SessionData,
    ) {
        close(state, data.id, session::CloseReason::Revoked);
    }
}

#[cfg(test)]
#[path = "remote_input_tests.rs"]
mod tests;
