use crate::{native_popup, FilePicker, FilesWindow, MenuHost, SharedMenuItem, Theme};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

pub fn deliver(window: &FilesWindow, action: String, unavailable: bool) -> bool {
    let host = window.global::<MenuHost>();
    let owner = host.get_active_owner();
    if owner <= 0 {
        return false;
    }
    host.set_active_owner(0);
    host.set_event_owner(owner);
    host.set_event_action(action.into());
    host.set_event_unavailable(unavailable);
    host.set_event_generation(host.get_event_generation().wrapping_add(1));
    true
}

fn option(label: String, index: usize, selected: i32) -> SharedMenuItem {
    SharedMenuItem {
        action: index.to_string().into(),
        label: label.into(),
        checked: index as i32 == selected,
        enabled: true,
        ..Default::default()
    }
}

pub fn install(window: &FilesWindow, popup: Rc<RefCell<native_popup::NativePopup>>) {
    let next = Cell::new(0i32);
    window.global::<MenuHost>().on_register_owner(move || {
        let owner = next.get().saturating_add(1);
        next.set(owner);
        owner
    });
    window
        .global::<MenuHost>()
        .on_options_items(|options, selected| {
            ModelRc::new(VecModel::from(
                options
                    .iter()
                    .enumerate()
                    .map(|(index, label)| option(label.to_string(), index, selected))
                    .collect::<Vec<_>>(),
            ))
        });
    window
        .global::<FilePicker>()
        .on_menu_items(|options, selected| {
            ModelRc::new(VecModel::from(
                options
                    .iter()
                    .enumerate()
                    .map(|(index, row)| option(row.label.to_string(), index, selected))
                    .collect::<Vec<_>>(),
            ))
        });
    let weak = window.as_weak();
    let native = popup.clone();
    window
        .global::<MenuHost>()
        .on_request(move |owner, items, x, y, width, height, title| {
            let Some(window) = weak.upgrade() else {
                return false;
            };
            if owner <= 0
                || items.row_count() == 0
                || items.row_count() > 1024
                || ![x, y, width, height].iter().all(|value| value.is_finite())
                || width <= 0.
                || height <= 0.
            {
                return false;
            }
            deliver(&window, String::new(), false);
            if window.get_menu_kind() > 0 {
                window.invoke_dismiss_menu();
            }

            native.borrow_mut().close();
            let logical = window
                .window()
                .size()
                .to_logical(window.window().scale_factor());
            let request = native_popup::MenuRequest {
                items: items
                    .iter()
                    .map(|item| native_popup::OwnedMenuItem {
                        action: item.action.to_string(),
                        label: item.label.to_string(),
                        shortcut: item.shortcut.to_string(),
                        enabled: item.enabled,
                        separator: item.separator,
                        destructive: item.destructive,
                        checked: item.checked,
                    })
                    .collect(),
                title: title.to_string(),
                x: x.round() as i32,
                y: y.round() as i32,
                width: width.min((logical.width - 8.).max(1.)).round().max(1.) as u32,
                height: height.min((logical.height - 8.).max(1.)).round().max(1.) as u32,
                scale: window.window().scale_factor(),
                radius: window.global::<Theme>().get_context_menu_radius(),
            };
            if native.borrow_mut().open(request) {
                window.global::<MenuHost>().set_active_owner(owner);
                true
            } else {
                false
            }
        });
    let weak = window.as_weak();
    window.global::<MenuHost>().on_close(move |owner| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if owner > 0 && window.global::<MenuHost>().get_active_owner() == owner {
            popup.borrow_mut().close();
            deliver(&window, String::new(), false);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_keeps_its_owner_and_cannot_be_delivered_twice() {
        i_slint_backend_testing::init_no_event_loop();
        let window = FilesWindow::new().unwrap();
        let host = window.global::<MenuHost>();
        host.set_active_owner(17);
        host.set_event_generation(i32::MAX);
        assert!(deliver(&window, "copy".into(), false));
        assert_eq!(host.get_active_owner(), 0);
        assert_eq!(host.get_event_owner(), 17);
        assert_eq!(host.get_event_action(), "copy");
        assert!(!host.get_event_unavailable());
        assert_eq!(host.get_event_generation(), i32::MIN);
        assert!(!deliver(&window, "cut".into(), true));
        assert_eq!(host.get_event_action(), "copy");
        assert_eq!(host.get_event_owner(), 17);

        host.set_active_owner(23);
        assert!(deliver(&window, String::new(), true));
        assert_eq!(host.get_event_owner(), 23);
        assert!(host.get_event_unavailable());
        assert_eq!(host.get_event_action(), "");
        assert_eq!(host.get_event_generation(), i32::MIN + 1);
    }
}
