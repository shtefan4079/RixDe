use crate::{ContextMenuItemData, RixPanelWindow, Theme};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::rc::Rc;

const CLOSE_ICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" width="24" height="24" fill="none"><path d="M6.5 6.5L17.5 17.5M17.5 6.5L6.5 17.5" stroke="#ef4444" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/></svg>"##;

fn sanitize_menu_separators(items: &mut Vec<ContextMenuItemData>) {

    while items.first().is_some_and(|item| item.separator) {
        items.remove(0);
    }

    while items.last().is_some_and(|item| item.separator) {
        items.pop();
    }

    let mut i = 0;
    while i < items.len() {
        if items[i].separator {
            if i + 1 < items.len() && items[i + 1].separator {
                items.remove(i + 1);
                continue;
            }
        }
        i += 1;
    }

    let non_separators = items.iter().filter(|i| !i.separator).count();
    if non_separators <= 1 {
        items.retain(|i| !i.separator);
    }
}

pub fn set_items(window: &RixPanelWindow, mut items: Vec<ContextMenuItemData>) {
    sanitize_menu_separators(&mut items);
    let theme = window.global::<Theme>();
    for item in &mut items {
        if !item.has_icon {
            if item.id == "close" || item.id == "close_all" {
                if let Ok(icon) = slint::Image::load_from_svg_data(CLOSE_ICON_SVG.as_bytes()) {
                    item.icon = icon;
                    item.has_icon = true;
                    continue;
                }
            }
            let name = match item.id.as_str() {
                "favorite" | "unfavorite" => "favorites",
                "pin" | "pin_app" => "window-pin",
                "unpin" | "unpin_app" => "window-unpin",
                "launch" | "new_window" => "document-open",
                "minimize" => "window-minimize",
                "restore" => "window-restore",
                "maximize" => "window-maximize",
                "close" | "close_all" => "window-close",
                "back" => "go-previous",
                "panel-settings" | "shell-settings" | "wallpaper-settings" => "configure",
                "panel-edit" => "document-edit",
                _ => "",
            };
            if let Some(icon) = crate::apps::load_icon_at_size(name, theme.get_icon_size() as u32) {
                item.icon = icon;
                item.has_icon = true;
            }
        }
    }
    update_geometry(window, &mut items);
    window.set_context_selected_index(-1);
    window.set_context_menu_items(ModelRc::from(Rc::new(VecModel::from(items))));
}

fn update_geometry(window: &RixPanelWindow, items: &mut [ContextMenuItemData]) {
    let row_height = window.global::<Theme>().get_menu_row_height();
    let separator_height = window.global::<crate::MenuStyle>().get_separator_height();
    let font_size = window.global::<Theme>().get_font_size_md().max(13.);
    let mut y = 0.;
    let mut max_item_width: f32 = 0.;
    for item in items.iter_mut() {
        item.row_y = y;
        item.row_height = if item.separator {
            separator_height
        } else {
            row_height
        };
        y += item.row_height;
        if !item.separator {
            let label_len = item.label.chars().count() as f32;
            let icon_w = if item.has_icon || item.checkable || item.checked { 28. } else { 0. };
            let arrow_w = if item.submenu { 20. } else { 0. };
            let item_w = 28. + icon_w + (label_len * font_size * 0.62) + arrow_w;
            if item_w > max_item_width {
                max_item_width = item_w;
            }
        }
    }
    let calculated = max_item_width.clamp(130., 280.);
    window.set_context_content_height(y);
    window.set_context_menu_width(calculated);
}

pub fn refresh_geometry(window: &RixPanelWindow) {
    let mut items: Vec<_> = window.get_context_menu_items().iter().collect();
    update_geometry(window, &mut items);
    window.set_context_menu_items(ModelRc::from(Rc::new(VecModel::from(items))));
}

pub fn close(window: &RixPanelWindow) {
    window.set_desktop_context(false);
    window.set_is_context_menu_open(false);
    window.invoke_tray_menu_closed();
    if window.get_is_app_menu_open() {
        window.set_menu_focus_generation(window.get_menu_focus_generation().wrapping_add(1));
    }
}
pub fn dismiss_all(window: &RixPanelWindow) {
    window.set_preview_visible(false);
    window.set_system_popup("".into());
    crate::launcher::set_open(window, false);
    window.set_session_dialog_visible(false);
    close(window);
}
pub fn dismiss_transient(window: &RixPanelWindow) {
    window.set_preview_visible(false);
    window.set_system_popup("".into());
    crate::launcher::dismiss_transient(window);
    window.set_session_dialog_visible(false);
    close(window);
}

pub fn install(window: &RixPanelWindow) {
    let weak = window.as_weak();
    window.on_panel_context_menu(move |x, y| {
        let Some(window) = weak.upgrade() else { return };
        let bottom = window.window().size().height as f32 / window.window().scale_factor() - y;
        dismiss_all(&window);
        window.set_context_is_tray(false);
        window.set_context_can_go_back(false);
        window.set_context_prefer_below(window.get_panel_at_top());
        window.set_context_menu_x(x);
        window.set_context_menu_top(y);
        window.set_context_menu_bottom(bottom);
        window.set_context_menu_target_id(0);
        window.set_context_menu_target_exec("".into());
        set_items(
            &window,
            vec![
                ContextMenuItemData {
                    id: if window.get_is_edit_mode() {
                        "panel-edit"
                    } else {
                        "panel-settings"
                    }
                    .into(),
                    label: crate::i18n::tr(if window.get_is_edit_mode() {
                        "Finish editing"
                    } else {
                        "Configure panel…"
                    }),
                    ..Default::default()
                },
                ContextMenuItemData {
                    id: "shell-settings".into(),
                    label: crate::i18n::tr("Settings…"),
                    ..Default::default()
                },
            ],
        );
        window.set_is_context_menu_open(true);
    });
    let weak = window.as_weak();
    window.on_context_go_back(move || {
        if let Some(window) = weak.upgrade() {
            if window.get_context_is_tray() && window.get_context_can_go_back() {
                window.invoke_tray_menu_selected("back".into());
            }
        }
    });
    let weak = window.as_weak();
    window.on_context_enter_submenu(move || {
        if let Some(window) = weak.upgrade() {
            let index = window.get_context_selected_index();
            if index >= 0
                && window
                    .get_context_menu_items()
                    .row_data(index as usize)
                    .is_some_and(|row| row.submenu && !row.disabled)
            {
                window.invoke_activate_context_selection();
            }
        }
    });
    let weak = window.as_weak();
    window.on_context_select_edge(move |end| {
        if let Some(window) = weak.upgrade() {
            window.set_context_selected_index(-1);
            window.invoke_navigate_context_menu(if end { -1 } else { 1 });
        }
    });
    let weak = window.as_weak();
    window.on_navigate_context_menu(move |delta| {
        let Some(window) = weak.upgrade() else { return };
        let items = window.get_context_menu_items();
        let count = items.row_count() as i32;
        let mut index = window.get_context_selected_index();
        if index < 0 && delta < 0 {
            index = 0;
        }
        for _ in 0..count {
            index = (index + delta.signum()).rem_euclid(count);
            if items
                .row_data(index as usize)
                .is_some_and(|e| !e.disabled && !e.separator)
            {
                window.set_context_selected_index(index);
                break;
            }
        }
    });
    let weak = window.as_weak();
    window.on_activate_context_selection(move || {
        let Some(window) = weak.upgrade() else { return };
        let index = window.get_context_selected_index();
        if index < 0 {
            return;
        }
        if let Some(entry) = window
            .get_context_menu_items()
            .row_data(index as usize)
            .filter(|e| !e.disabled && !e.separator)
        {
            if window.get_context_is_tray() {
                window.invoke_tray_menu_selected(entry.id);
            } else {
                window.invoke_context_menu_selected(
                    entry.id,
                    window.get_context_menu_target_id(),
                    window.get_context_menu_target_exec(),
                );
            }
        }
    });
}

pub fn open_desktop(window: &RixPanelWindow, x: f32, y: f32) {
    dismiss_all(window);
    window.set_is_edit_mode(false);
    window.set_desktop_context(true);
    window.set_context_is_tray(false);
    window.set_context_can_go_back(false);
    window.set_context_prefer_below(true);
    window.set_context_menu_x(x);
    window.set_context_menu_top(y);
    window.set_context_menu_bottom(window.get_available_height() as f32 - y);
    window.set_context_menu_target_id(0);
    window.set_context_menu_target_exec("".into());
    set_items(
        window,
        vec![
            ContextMenuItemData {
                id: "wallpaper-settings".into(),
                label: crate::i18n::tr("Set wallpaper…"),
                ..Default::default()
            },
            ContextMenuItemData {
                id: "shell-settings".into(),
                label: crate::i18n::tr("Settings…"),
                ..Default::default()
            },
            ContextMenuItemData {
                id: "panel-settings".into(),
                label: crate::i18n::tr("Configure panel…"),
                ..Default::default()
            },
        ],
    );
    window.set_is_context_menu_open(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_open_submenu_updates_theme_geometry_without_resetting_navigation() {
        i_slint_backend_testing::init_no_event_loop();
        let window = RixPanelWindow::new().unwrap();
        let mut settings = rix_config::ThemeSettings::default();
        settings
            .appearance
            .metrics
            .insert("menu_row_height".into(), 34.);
        settings
            .appearance
            .metrics
            .insert("default_spacing".into(), 8.);
        crate::appearance::apply_shell_theme(&window, &settings);
        set_items(
            &window,
            vec![
                ContextMenuItemData {
                    id: "back".into(),
                    label: "Back".into(),
                    ..Default::default()
                },
                ContextMenuItemData {
                    separator: true,
                    ..Default::default()
                },
                ContextMenuItemData {
                    id: "42".into(),
                    label: "Nested action".into(),
                    submenu: true,
                    ..Default::default()
                },
            ],
        );
        window.set_context_is_tray(true);
        window.set_context_can_go_back(true);
        window.set_context_menu_target_id(42);
        window.set_context_menu_x(120.);
        window.set_context_selected_index(2);
        window.set_is_context_menu_open(true);
        window.show().unwrap();
        settings
            .appearance
            .metrics
            .insert("menu_row_height".into(), 48.);
        settings
            .appearance
            .metrics
            .insert("default_spacing".into(), 11.);
        crate::appearance::apply_shell_theme(&window, &settings);
        let rows: Vec<_> = window.get_context_menu_items().iter().collect();
        assert_eq!((rows[0].row_y, rows[0].row_height), (0., 48.));
        assert_eq!((rows[1].row_y, rows[1].row_height), (48., 12.));
        assert_eq!((rows[2].row_y, rows[2].row_height), (60., 48.));
        assert_eq!(window.get_context_content_height(), 108.);
        assert_eq!(rows[2].id.as_str(), "42");
        assert!(rows[2].submenu);
        assert!(window.get_is_context_menu_open());
        assert!(window.get_context_is_tray() && window.get_context_can_go_back());
        assert_eq!(window.get_context_selected_index(), 2);
        assert_eq!(window.get_context_menu_target_id(), 42);
        assert_eq!(window.get_context_menu_x(), 120.);
    }

    #[test]
    fn separators_use_shared_spacing_and_keep_row_geometry_contiguous() {
        i_slint_backend_testing::init_no_event_loop();
        let window = RixPanelWindow::new().unwrap();
        let theme = window.global::<Theme>();
        theme.set_default_spacing(7.5);
        theme.set_spacing_xs(13.);
        theme.set_menu_row_height(41.);
        set_items(
            &window,
            vec![
                ContextMenuItemData {
                    label: "First".into(),
                    ..Default::default()
                },
                ContextMenuItemData {
                    separator: true,
                    ..Default::default()
                },
                ContextMenuItemData {
                    label: "Disabled".into(),
                    disabled: true,
                    ..Default::default()
                },
            ],
        );
        let rows: Vec<_> = window.get_context_menu_items().iter().collect();
        assert_eq!((rows[0].row_y, rows[0].row_height), (0., 41.));
        assert_eq!((rows[1].row_y, rows[1].row_height), (41., 8.5));
        assert_eq!((rows[2].row_y, rows[2].row_height), (49.5, 41.));
        assert_eq!(window.get_context_content_height(), 90.5);
    }
}
