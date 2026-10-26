use super::*;

fn rows(states: &[(bool, bool)]) -> ModelRc<MenuItem> {
    model(
        states
            .iter()
            .map(|&(enabled, separator)| MenuItem {
                enabled,
                separator,
                ..Default::default()
            })
            .collect(),
    )
}

#[test]
fn empty_and_disabled_menus_have_no_keyboard_target() {
    for menu in [
        rows(&[]),
        rows(&[(false, false), (true, true), (false, true)]),
    ] {
        for direction in [-1, 0, 1, 2] {
            assert_eq!(next_menu_index(menu.clone(), -1, direction), -1);
            assert_eq!(next_menu_index(menu.clone(), 0, direction), -1);
        }
    }
}

#[test]
fn menu_navigation_skips_any_number_of_disabled_rows_and_separators() {
    let mut states = vec![(false, false); 128];
    states[125] = (true, false);
    states[126] = (true, true);
    let menu = rows(&states);
    assert_eq!(next_menu_index(menu.clone(), 0, 1), 125);
    assert_eq!(next_menu_index(menu.clone(), 124, -1), 125);
    assert_eq!(next_menu_index(menu, 125, 1), 125);
}

#[test]
fn menu_navigation_wraps_and_starts_at_the_correct_edge() {
    let menu = rows(&[
        (false, false),
        (true, false),
        (true, true),
        (false, false),
        (true, false),
        (false, false),
    ]);
    assert_eq!(next_menu_index(menu.clone(), -1, 1), 1);
    assert_eq!(next_menu_index(menu.clone(), -1, -1), 4);
    assert_eq!(next_menu_index(menu.clone(), 4, 1), 1);
    assert_eq!(next_menu_index(menu.clone(), 1, -1), 4);
    assert_eq!(next_menu_index(menu.clone(), 3, 0), 1);
    assert_eq!(next_menu_index(menu.clone(), 3, 2), 4);
    assert_eq!(next_menu_index(menu.clone(), i32::MAX, 1), 1);
    assert_eq!(next_menu_index(menu, i32::MIN, -1), 4);
}
