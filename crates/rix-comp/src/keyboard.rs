use crate::state::RixState;
use smithay::input::keyboard::{Layout, XkbConfig};
impl RixState {

    pub(crate) fn cancel_input(&mut self) {
        crate::remote_input::cancel_devices(self);
        use smithay::{
            backend::input::KeyState, input::keyboard::FilterResult, utils::SERIAL_COUNTER,
        };
        self.finish_switcher(false);
        self.cancel_drag();
        let time = self.start_time.elapsed().as_millis() as u32;
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.unset_grab(self);
            for code in keyboard.pressed_keys() {
                keyboard.input::<(), _>(
                    self,
                    code,
                    KeyState::Released,
                    SERIAL_COUNTER.next_serial(),
                    time,
                    |_, _, _| FilterResult::Forward,
                );
            }
            self.keyboard_focus_lost();
            keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
        }
        if let Some(pointer) = self.seat.get_pointer() {
            for button in std::mem::take(&mut self.pressed_buttons) {
                pointer.button(
                    self,
                    &smithay::input::pointer::ButtonEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time,
                        button,
                        state: smithay::backend::input::ButtonState::Released,
                    },
                );
            }
            pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), time);
            pointer.frame(self);
        }
        if let Some(touch) = self.seat.get_touch() {
            touch.cancel(self);
            touch.unset_grab(self);
        }
        self.selection_start = None;
        self.resize_cursor = None;
        self.logo_tap = None;
        self.clear_intercepted_keys();
        self.publish_keyboard_state();
    }

    pub fn configure_keyboard(&mut self, config: rix_config::KeyboardConfig) -> anyhow::Result<()> {
        config.validate()?;
        let keyboard = self
            .seat
            .get_keyboard()
            .ok_or_else(|| anyhow::anyhow!("No keyboard seat"))?;
        let layouts = config
            .layouts
            .iter()
            .map(|l| l.layout.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let variants = config
            .layouts
            .iter()
            .map(|l| l.variant.as_str())
            .collect::<Vec<_>>()
            .join(",");
        keyboard.set_xkb_config(
            self,
            XkbConfig {
                layout: &layouts,
                variant: &variants,
                options: Some(config.switch_option.clone()),
                ..Default::default()
            },
        )?;
        keyboard.change_repeat_info(config.repeat_rate as i32, config.repeat_delay as i32);
        self.keyboard_config = config;
        self.keyboard_group = u32::MAX;
        self.publish_keyboard_state();
        Ok(())
    }
    pub fn keyboard_state(&self) -> rix_ipc::KeyboardState {
        rix_ipc::KeyboardState {
            active: self.keyboard_group,
            layouts: self.keyboard_config.layouts.clone(),
        }
    }
    pub fn publish_keyboard_state(&mut self) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let group = keyboard.with_xkb_state(self, |state| match state.xkb().lock() {
            Ok(state) => state.active_layout().0,
            Err(poisoned) => poisoned.into_inner().active_layout().0,
        });
        if self.keyboard_group != group {
            self.keyboard_group = group;
            if let Some(sender) = &self.ipc_event_sender {
                let _ = sender.send(rix_ipc::IpcEvent::KeyboardChanged(self.keyboard_state()));
            }
        }
    }
    pub fn process_desktop_commands(
        &mut self,
        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
        config: &rix_config::ConfigManager,
    ) {
        if self.greeter_mode {
            for command in self.desktop_rx.try_iter().chain(self.lock_rx.try_iter()) {
                let _ = command.responder.send(Err(
                    "Desktop controls are unavailable on the login display".into(),
                ));
            }
            return;
        }

        for _ in 0..8 {
            let Ok(command) = self.lock_rx.try_recv() else {
                break;
            };
            if !command.responder.is_closed() {
                self.request_lock(config, command.responder);
            }
        }
        self.tick_locker(config);
        if std::mem::take(&mut self.lock.requested) {
            if let Err(error) = self.launch_locker(config) {
                tracing::error!(%error, "Could not lock session");
            }
        }
        self.tick_clipboard(config);
        self.window_snap_settings = config.get_desktop().window_snap;
        let theme = config.get_theme();
        let accent = rix_core::Color::from_hex(&theme.accent_color).unwrap_or_default();
        self.selection_color = [accent.r, accent.g, accent.b, 1.];
        self.animation_duration =
            theme.appearance.metric("window_duration").clamp(0., 1000.) as u32;
        if self.snap_preview.as_ref().is_some_and(|preview| {
            !preview.visible
                && preview.transition_started.elapsed().as_millis()
                    >= self.animation_duration as u128
        }) {
            self.snap_preview = None;
        }
        self.reconcile_switcher();
        self.screencopy
            .publish_status(&self.ipc_event_sender, self.window_capture.active());
        for _ in 0..rix_ipc::COMMANDS_PER_TICK {
            let Ok(command) = self.desktop_rx.try_recv() else {
                break;
            };
            if command.responder.is_closed() {
                continue;
            }
            if matches!(command.action, rix_ipc::DesktopAction::LockSession) {
                self.request_lock(config, command.responder);
                continue;
            }
            if self.lock.active {
                let _ = command.responder.send(Err("Session is locked".into()));
                continue;
            }
            if matches!(command.action, rix_ipc::DesktopAction::LogoutSession) {
                let _ = command.responder.send(Ok(rix_ipc::IpcResponse::Ok));
                self.running
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                break;
            }
            if matches!(command.action, rix_ipc::DesktopAction::Output(_)) && self.native_session {
                if self.output_commands.len() >= rix_ipc::COMMAND_QUEUE_CAPACITY {
                    let _ = command
                        .responder
                        .send(Err("Display controls busy; retry shortly".into()));
                } else {
                    self.output_commands.push_back(command);
                }
                continue;
            }
            if let rix_ipc::DesktopAction::Preview { id, width, height } = command.action {
                let mut queue = std::mem::take(&mut self.previews);
                queue.submit(self, renderer, id, width, height, command.responder);
                self.previews = queue;
                continue;
            }
            if let rix_ipc::DesktopAction::Clipboard(rix_ipc::IpcRequest::ClipboardSetPng {
                data,
            }) = command.action
            {
                self.set_png_clipboard(data, command.responder);
                continue;
            }
            let result = match command.action {
                rix_ipc::DesktopAction::Launcher(open) => {
                    if open == Some(false) {
                        if let Some(sender) = &self.ipc_event_sender {
                            let _ = sender.send(rix_ipc::IpcEvent::LauncherToggled(false));
                        }
                    } else {
                        self.publish_shell(
                            self.shell_output().as_deref(),
                            open.map_or(
                                rix_ipc::ShellEvent::LauncherToggle,
                                rix_ipc::ShellEvent::LauncherSet,
                            ),
                        );
                    }
                    Ok(rix_ipc::IpcResponse::Ok)
                }
                rix_ipc::DesktopAction::PowerAction(action) => {
                    self.publish_shell(
                        self.shell_output().as_deref(),
                        rix_ipc::ShellEvent::PowerAction(action),
                    );
                    Ok(rix_ipc::IpcResponse::Ok)
                }
                rix_ipc::DesktopAction::Output(rix_ipc::IpcRequest::GetOutputs) => {
                    Ok(rix_ipc::IpcResponse::Outputs(
                        crate::output_control::snapshot(self.space.outputs(), false, None),
                    ))
                }
                rix_ipc::DesktopAction::Output(_) => {
                    Err("Physical display changes require a native session".into())
                }
                rix_ipc::DesktopAction::LockSession => unreachable!(),
                rix_ipc::DesktopAction::LogoutSession => unreachable!(),
                rix_ipc::DesktopAction::Clipboard(request) => self.clipboard_request(request),
                rix_ipc::DesktopAction::GetPlugins => {
                    Ok(rix_ipc::IpcResponse::Plugins(self.plugins.statuses()))
                }
                rix_ipc::DesktopAction::GetCaptureStatus => {
                    Ok(rix_ipc::IpcResponse::CaptureStatus(
                        self.screencopy.active() || self.window_capture.active(),
                    ))
                }
                rix_ipc::DesktopAction::StopScreenCapture => {
                    let ids = self.plugins.stop_capture(&self.display_handle);
                    let mut desktop = config.get_desktop();
                    desktop.enabled_plugins.retain(|id| !ids.contains(id));
                    config
                        .save_desktop(desktop)
                        .map(|()| rix_ipc::IpcResponse::Ok)
                        .map_err(|e| e.to_string())
                }
                rix_ipc::DesktopAction::GetSwitcher => {
                    Ok(rix_ipc::IpcResponse::Switcher(self.switcher_snapshot()))
                }
                rix_ipc::DesktopAction::ChooseSwitcher(id) => {
                    self.choose_switcher(id).map(|()| rix_ipc::IpcResponse::Ok)
                }
                rix_ipc::DesktopAction::Preview { .. } => unreachable!(),
                rix_ipc::DesktopAction::Keyboard(group) => {
                    if group
                        .is_some_and(|group| group as usize >= self.keyboard_config.layouts.len())
                    {
                        Err("Invalid keyboard group".into())
                    } else if let Some(keyboard) = self.seat.get_keyboard() {
                        if let Some(group) = group {
                            keyboard
                                .with_xkb_state(self, |mut state| state.set_layout(Layout(group)));
                        }
                        self.publish_keyboard_state();
                        Ok(rix_ipc::IpcResponse::Keyboard(self.keyboard_state()))
                    } else {
                        Err("No keyboard seat".into())
                    }
                }
            };
            let _ = command.responder.send(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hardware_audio_keys_work_for_rotary_steps_and_while_locked() {
        use rix_services::audio::Hotkey;
        use smithay::{backend::input::KeyState, utils::SERIAL_COUNTER};
        for (greeter, locked) in [(false, false), (false, true), (true, false)] {
            let mut display =
                smithay::reexports::wayland_server::Display::<RixState>::new().unwrap();
            let mut state = RixState::new_with_mode(&mut display, "audio-test".into(), greeter);
            state
                .seat
                .add_keyboard(Default::default(), 500, 30)
                .unwrap();
            state.configure_keyboard(Default::default()).unwrap();
            state.lock.active = locked;
            let mut audio = state.audio_hotkey_rx.take().unwrap();

            for (code, expected) in [
                (123u32, Hotkey::VolumeUp),
                (122, Hotkey::VolumeDown),
                (121, Hotkey::ToggleMute),
                (256, Hotkey::ToggleMicrophoneMute),
            ] {
                for _ in 0..2 {
                    for key_state in [KeyState::Pressed, KeyState::Released] {
                        state.handle_keyboard_key(
                            code.into(),
                            key_state,
                            SERIAL_COUNTER.next_serial(),
                            1,
                            false,
                        );
                    }
                    if greeter {
                        assert!(audio.try_recv().is_err());
                    } else {
                        assert_eq!(audio.try_recv().unwrap(), expected);
                    }
                }
            }
        }
    }

    #[test]
    fn logo_tap_requests_launcher_toggle_but_shortcut_does_not() {
        use smithay::{backend::input::KeyState, utils::SERIAL_COUNTER};
        let mut display = smithay::reexports::wayland_server::Display::<RixState>::new().unwrap();
        let mut state = RixState::new(&mut display, "logo-test".into());
        state
            .seat
            .add_keyboard(Default::default(), 500, 30)
            .unwrap();
        let (events, mut receiver) = tokio::sync::broadcast::channel(8);
        state.ipc_event_sender = Some(events);
        let press = |state: &mut RixState, code: u32, key_state: KeyState| {
            state.handle_keyboard_key(
                code.into(),
                key_state,
                SERIAL_COUNTER.next_serial(),
                1,
                false,
            );
        };

        press(&mut state, 133, KeyState::Pressed);
        press(&mut state, 46, KeyState::Pressed);
        press(&mut state, 46, KeyState::Released);
        press(&mut state, 133, KeyState::Released);
        assert!(
            receiver.try_recv().is_err(),
            "Super+L must not open the launcher"
        );
        press(&mut state, 133, KeyState::Pressed);
        press(&mut state, 133, KeyState::Released);
        assert_eq!(
            receiver.try_recv().unwrap(),
            rix_ipc::IpcEvent::LauncherToggleRequested
        );
        press(&mut state, 133, KeyState::Pressed);
        press(&mut state, 133, KeyState::Released);
        assert_eq!(
            receiver.try_recv().unwrap(),
            rix_ipc::IpcEvent::LauncherToggleRequested
        );
    }

    #[test]
    fn configured_groups_and_shortcut_use_actual_seat_state() {
        let mut display = smithay::reexports::wayland_server::Display::<RixState>::new().unwrap();
        let mut state = RixState::new(&mut display, "keyboard-test".into());
        state
            .seat
            .add_keyboard(Default::default(), 500, 30)
            .unwrap();
        state
            .configure_keyboard(rix_config::KeyboardConfig {
                layouts: ["us", "de", "fr"]
                    .map(|code| rix_config::KeyboardLayout {
                        layout: code.into(),
                        variant: String::new(),
                        label: code.to_uppercase(),
                    })
                    .into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(state.keyboard_state().active, 0);
        for (code, key_state) in [
            (64u32, smithay::backend::input::KeyState::Pressed),
            (50u32, smithay::backend::input::KeyState::Pressed),
        ] {
            state.handle_keyboard_key(
                code.into(),
                key_state,
                smithay::utils::SERIAL_COUNTER.next_serial(),
                1,
                false,
            );
        }
        assert_eq!(
            state.keyboard_state().active,
            1,
            "Alt+Shift must switch the real XKB group"
        );
        assert_eq!(state.keyboard_state().layouts[1].layout, "de");
    }
}
