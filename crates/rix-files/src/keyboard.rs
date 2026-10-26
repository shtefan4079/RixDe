use slint::{
    platform::WindowEvent,
    winit_030::{
        winit::{
            event::{ElementState, WindowEvent as NativeEvent},
            keyboard::{Key, KeyCode, ModifiersState, PhysicalKey},
        },
        EventResult, WinitWindowAccessor,
    },
};
use std::collections::HashMap;

pub fn control_letter(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let character = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    (('\u{1}'..='\u{1a}').contains(&character)).then(|| char::from(character as u8 + b'a' - 1))
}

fn shortcut_letter(logical: &str, physical: PhysicalKey) -> Option<char> {
    if let Some(letter) = control_letter(logical) {
        return Some(letter);
    }

    if logical.len() == 1 && logical.as_bytes()[0].is_ascii_alphabetic() {
        return None;
    }
    if logical.chars().count() != 1 {
        return None;
    }
    let PhysicalKey::Code(code) = physical else {
        return None;
    };
    Some(match code {
        KeyCode::KeyA => 'a',
        KeyCode::KeyB => 'b',
        KeyCode::KeyC => 'c',
        KeyCode::KeyD => 'd',
        KeyCode::KeyE => 'e',
        KeyCode::KeyF => 'f',
        KeyCode::KeyG => 'g',
        KeyCode::KeyH => 'h',
        KeyCode::KeyI => 'i',
        KeyCode::KeyJ => 'j',
        KeyCode::KeyK => 'k',
        KeyCode::KeyL => 'l',
        KeyCode::KeyM => 'm',
        KeyCode::KeyN => 'n',
        KeyCode::KeyO => 'o',
        KeyCode::KeyP => 'p',
        KeyCode::KeyQ => 'q',
        KeyCode::KeyR => 'r',
        KeyCode::KeyS => 's',
        KeyCode::KeyT => 't',
        KeyCode::KeyU => 'u',
        KeyCode::KeyV => 'v',
        KeyCode::KeyW => 'w',
        KeyCode::KeyX => 'x',
        KeyCode::KeyY => 'y',
        KeyCode::KeyZ => 'z',
        _ => return None,
    })
}

pub fn install(window: &slint::Window) {
    let mut modifiers = ModifiersState::empty();
    let mut pressed = HashMap::new();
    window.on_winit_window_event(move |window, event| {
        match event {
            NativeEvent::ModifiersChanged(value) => modifiers = value.state(),
            NativeEvent::Focused(false) => {
                modifiers = ModifiersState::empty();
                for (_, text) in pressed.drain() {
                    window.dispatch_event(WindowEvent::KeyReleased { text });
                }
            }
            NativeEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } => {
                if event.state == ElementState::Released {
                    if let Some(text) = pressed.remove(&event.physical_key) {
                        window.dispatch_event(WindowEvent::KeyReleased { text });
                        return EventResult::PreventDefault;
                    }
                } else if modifiers.control_key() && !modifiers.super_key() {
                    if let Key::Character(logical) = &event.logical_key {
                        if let Some(letter) = shortcut_letter(logical, event.physical_key) {
                            let text: slint::SharedString = letter.to_string().into();
                            pressed.insert(event.physical_key, text.clone());
                            window.dispatch_event(if event.repeat {
                                WindowEvent::KeyPressRepeated { text }
                            } else {
                                WindowEvent::KeyPressed { text }
                            });
                            return EventResult::PreventDefault;
                        }
                    }
                }
            }
            _ => {}
        }
        EventResult::Propagate
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shortcut_fallback_preserves_latin_layout_and_normal_text() {
        assert_eq!(
            shortcut_letter("α", PhysicalKey::Code(KeyCode::KeyA)),
            Some('a')
        );
        assert_eq!(
            shortcut_letter("Α", PhysicalKey::Code(KeyCode::KeyA)),
            Some('a')
        );
        assert_eq!(
            shortcut_letter("σ", PhysicalKey::Code(KeyCode::KeyS)),
            Some('s')
        );
        assert_eq!(
            shortcut_letter("é", PhysicalKey::Code(KeyCode::KeyS)),
            Some('s')
        );
        assert_eq!(shortcut_letter("a", PhysicalKey::Code(KeyCode::KeyQ)), None);
        assert_eq!(
            shortcut_letter("\u{1}", PhysicalKey::Code(KeyCode::KeyQ)),
            Some('a')
        );
        assert_eq!(
            shortcut_letter("中文", PhysicalKey::Code(KeyCode::KeyA)),
            None
        );
        assert_eq!(control_letter("α"), None);
    }
}
