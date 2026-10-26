use smithay_client_toolkit::seat::keyboard::{KeyEvent, Modifiers};

pub(super) fn shortcut_letter(event: &KeyEvent, modifiers: Modifiers) -> Option<char> {
    if !modifiers.ctrl || modifiers.alt || modifiers.logo {
        return None;
    }
    if let Some(letter) = event.keysym.key_char().filter(char::is_ascii_alphabetic) {
        return Some(letter.to_ascii_lowercase());
    }
    if let Some(text) = &event.utf8 {
        let mut chars = text.chars();
        if let Some(character) = chars.next().filter(|_| chars.next().is_none()) {
            if ('\u{1}'..='\u{1a}').contains(&character) {
                return Some(char::from(character as u8 + b'a' - 1));
            }
        }
    }

    event.keysym.key_char().filter(|c| c.is_alphabetic())?;
    Some(match event.raw_code {
        16 => 'q',
        17 => 'w',
        18 => 'e',
        19 => 'r',
        20 => 't',
        21 => 'y',
        22 => 'u',
        23 => 'i',
        24 => 'o',
        25 => 'p',
        30 => 'a',
        31 => 's',
        32 => 'd',
        33 => 'f',
        34 => 'g',
        35 => 'h',
        36 => 'j',
        37 => 'k',
        38 => 'l',
        44 => 'z',
        45 => 'x',
        46 => 'c',
        47 => 'v',
        48 => 'b',
        49 => 'n',
        50 => 'm',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use xkbcommon::xkb;
    #[test]
    fn real_xkb_control_shortcuts_normalize_before_slint() {
        for layout in ["us", "gr"] {
            let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
            let keymap = xkb::Keymap::new_from_names(
                &context,
                "",
                "",
                layout,
                "",
                None,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .unwrap();
            let mut state = xkb::State::new(&keymap);
            state.update_key(xkb::Keycode::new(29 + 8), xkb::KeyDirection::Down);
            for (raw_code, letter) in [(30, 'a'), (46, 'c'), (45, 'x'), (47, 'v'), (44, 'z')] {
                let keycode = xkb::Keycode::new(raw_code + 8);
                let event = KeyEvent {
                    time: 0,
                    raw_code,
                    keysym: state.key_get_one_sym(keycode),
                    utf8: Some(state.key_get_utf8(keycode)),
                };
                assert_eq!(
                    shortcut_letter(
                        &event,
                        Modifiers {
                            ctrl: true,
                            ..Modifiers::default()
                        }
                    ),
                    Some(letter),
                    "{layout} Ctrl+{letter}"
                );
                assert_eq!(shortcut_letter(&event, Modifiers::default()), None);
                assert_eq!(
                    shortcut_letter(
                        &event,
                        Modifiers {
                            ctrl: true,
                            alt: true,
                            ..Modifiers::default()
                        }
                    ),
                    None
                );
            }
        }
    }
    #[test]
    fn logical_latin_layout_and_control_characters_keep_their_own_letter() {
        let modifiers = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        let event = KeyEvent {
            time: 0,
            raw_code: 16,
            keysym: smithay_client_toolkit::seat::keyboard::Keysym::a,
            utf8: Some("\u{1}".into()),
        };
        assert_eq!(shortcut_letter(&event, modifiers), Some('a'));
        assert_eq!(
            shortcut_letter(
                &event,
                Modifiers {
                    shift: true,
                    ..modifiers
                }
            ),
            Some('a')
        );
    }
}
