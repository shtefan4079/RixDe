use i_slint_backend_testing::ElementHandle;
use slint::{
    platform::{PointerEventButton, WindowEvent},
    ComponentHandle, Model, ModelRc, VecModel,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
slint::slint! {
    import { FilesWindow, FileItem, GridRow, PlaceItem, TabItem, AppItem, ApplicationChooser, ChooserApplication, DialogBackdrop } from "../ui/files.slint";
    import { ArchiveBrowser, ArchiveMember } from "../ui/archive.slint";
    import { FilePreview } from "../ui/preview.slint";
    import { Theme } from "../../../ui/common/theme.slint";
    import { Palette } from "std-widgets.slint";
    import { SkinFrame, SkinRenderer } from "../../../ui/common/frame.slint";
    export { FilesWindow, FileItem, GridRow, PlaceItem, TabItem, AppItem, Theme, Palette, SkinFrame, SkinRenderer, ApplicationChooser, ChooserApplication, ArchiveBrowser, ArchiveMember, FilePreview, DialogBackdrop }
}
#[path = "../../../ui/common/skins.rs"]
mod skins;
#[path = "../../../ui/common/bindings.rs"]
mod theme_bindings;
fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::from(Rc::new(VecModel::from(items)))
}
fn themed_icon(theme: &rix_config::ThemeSettings, directory: bool) -> slint::Image {
    let mut resolver = rix_assets::IconResolver::default();
    if let Some(image) = resolver
        .resolve(
            if directory {
                "folder"
            } else {
                "text-x-generic"
            },
            &theme.icon_theme,
            48,
            None,
        )
        .and_then(|path| slint::Image::load_from_path(&path).ok())
    {
        return image;
    }
    let svg = if directory {
        include_str!("../../../ui/common/icons/folder.svg")
    } else {
        include_str!("../assets/file.svg")
    };
    let svg = svg.replace("#fff", &theme.appearance.color("text_primary").to_hex());
    slint::Image::load_from_svg_data(svg.as_bytes()).unwrap()
}
fn current_theme() -> rix_config::ThemeSettings {
    std::fs::read(rix_files::preferences::config_directory().join("theme.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<rix_config::ThemeSettings>(&bytes).ok())
        .filter(|theme| theme.validate().is_ok())
        .unwrap_or_else(|| {
            serde_json::from_str(include_str!(
                "../../../assets/rix/themes/fairywren-dark/theme.json"
            ))
            .unwrap()
        })
}
fn element(w: &FilesWindow, label: &str) -> ElementHandle {
    ElementHandle::find_by_accessible_label(w, label)
        .next()
        .unwrap_or_else(|| panic!("Visible element with label {label:?} is missing"))
}
fn popup_action(w: &FilesWindow, label: &str, popup: &ElementHandle) -> ElementHandle {
    let origin = popup.absolute_position();
    let size = popup.size();
    ElementHandle::find_by_accessible_label(w, label)
        .find(|action| {
            let point = center(action);
            point.x >= origin.x
                && point.x <= origin.x + size.width
                && point.y >= origin.y
                && point.y <= origin.y + size.height
        })
        .unwrap_or_else(|| panic!("Popup action {label:?} is missing"))
}
fn center(element: &ElementHandle) -> slint::LogicalPosition {
    let origin = element.absolute_position();
    let size = element.size();
    slint::LogicalPosition::new(origin.x + size.width / 2., origin.y + size.height / 2.)
}
fn pointer(w: &FilesWindow, point: slint::LogicalPosition, button: PointerEventButton) {
    w.window()
        .dispatch_event(WindowEvent::PointerMoved { position: point });
    w.window().dispatch_event(WindowEvent::PointerPressed {
        position: point,
        button,
    });
    w.window().dispatch_event(WindowEvent::PointerReleased {
        position: point,
        button,
    });
    slint::platform::update_timers_and_animations();
}
fn populate(w: &FilesWindow, theme: &rix_config::ThemeSettings) {
    w.on_menu_rows_height(|items, row, separator| {
        items
            .iter()
            .map(|item| if item.separator { separator } else { row })
            .sum()
    });
    w.on_menu_items_width(|items, font_size, _base_width| {
        let font_size = if font_size > 0. { font_size } else { 13. };
        let mut max_item_width: f32 = 0.;
        for item in items.iter() {
            if !item.separator {
                let text_w = item.label.chars().count() as f32 * font_size * 0.62;
                let shortcut_w = if item.shortcut.is_empty() {
                    0.
                } else {
                    item.shortcut.chars().count() as f32 * font_size * 0.55 + 16.
                };
                let icon_w = if item.checked { 28. } else { 0. };
                let total_w = 28. + icon_w + text_w + shortcut_w;
                if total_w > max_item_width {
                    max_item_width = total_w;
                }
            }
        }
        max_item_width.clamp(130., 280.)
    });
    w.set_location("/home/demo/Documents".into());
    w.set_disk_name("System disk".into());
    w.set_disk_mount("/".into());
    w.set_disk_detail("184 GiB free of 500 GiB".into());
    w.set_disk_usage("63%".into());
    w.set_disk_ratio(0.63);
    w.set_disk_ready(true);
    w.set_disk_icon(
        rix_assets::IconResolver::default()
            .resolve("drive-harddisk", &theme.icon_theme, 48, None)
            .and_then(|path| slint::Image::load_from_path(&path).ok())
            .unwrap_or_else(|| {
                let svg = include_str!("../assets/drive.svg")
                    .replace("#fff", &theme.appearance.color("text_primary").to_hex());
                slint::Image::load_from_svg_data(svg.as_bytes()).unwrap()
            }),
    );
    w.set_tabs(model(vec![
        TabItem {
            label: "Documents".into(),
        },
        TabItem {
            label: "Downloads".into(),
        },
    ]));
    w.set_places(model(
        [
            "Home",
            "Documents",
            "Downloads",
            "Pictures",
            "Filesystem",
            "Trash",
        ]
        .into_iter()
        .map(|name| PlaceItem {
            label: name.into(),
            icon: themed_icon(theme, true),
            active: name == "Documents",
            removable: name != "Home",
            ..Default::default()
        })
        .collect(),
    ));
    let items: Vec<_> = [
        "Projects",
        "Reports",
        "Holiday photos",
        "notes.txt",
        "Architecture.pdf",
        "budget.csv",
        "A very long filename that should fit gracefully in the file list.txt",
    ]
    .into_iter()
    .enumerate()
    .map(|(i, name)| FileItem {
        name: name.into(),
        detail: if i < 3 { "Folder" } else { "Document" }.into(),
        size: if i < 3 { "" } else { "24.5 KiB" }.into(),
        modified: "2026-10-04 20:00".into(),
        icon: themed_icon(theme, i < 3),
        is_dir: i < 3,
        selected: i == 3,
        focused: i == 3,
        ..Default::default()
    })
    .collect();
    w.set_entries(model(items.clone()));
    let items = Rc::new(items);
    let weak = w.as_weak();
    let refreshed = items.clone();
    w.on_grid_columns_changed(move |columns| {
        if let Some(w) = weak.upgrade() {
            w.set_grid_rows(grid_rows(&refreshed, columns));
        }
    });
    w.set_grid_rows(grid_rows(&items, w.get_grid_columns()));
    w.set_status("7 items · 1 selected".into());
    w.set_selection_count(1);
    w.set_paste_available(true);
    w.set_can_back(true);
    w.set_can_up(true);
}
fn grid_rows(items: &[FileItem], columns: i32) -> ModelRc<GridRow> {
    let columns = columns.max(1) as usize;
    model(
        items
            .chunks(columns)
            .enumerate()
            .map(|(i, chunk)| {
                let mut padded = chunk.to_vec();
                padded.resize(columns, FileItem::default());
                GridRow {
                    items: model(padded),
                    start: (i * columns) as i32,
                }
            })
            .collect(),
    )
}
#[test]
fn real_pointer_keyboard_and_dialog_callbacks() {
    i_slint_backend_testing::init_no_event_loop();
    let w = FilesWindow::new().unwrap();
    theme_bindings::apply(
        w.global::<Theme>(),
        w.global::<Palette>(),
        &rix_config::ThemeSettings::default(),
    );
    skins::install(w.global::<SkinRenderer>(), w.as_weak());
    populate(&w, &rix_config::ThemeSettings::default());
    w.window().set_size(slint::LogicalSize::new(1080., 720.));
    w.show().unwrap();
    let selected = Rc::new(Cell::new(-1));
    let result = selected.clone();
    w.on_select_entry(move |index, _, _| result.set(index));
    let point = center(&element(&w, "Projects"));
    w.window().dispatch_event(WindowEvent::PointerPressed {
        position: point,
        button: PointerEventButton::Left,
    });
    w.window().dispatch_event(WindowEvent::PointerReleased {
        position: point,
        button: PointerEventButton::Left,
    });
    assert_eq!(selected.get(), 0, "Pointer must reach first file row");
    let key = Rc::new(Cell::new(false));
    let result = key.clone();
    w.on_keyboard(move |text, _, _, _| {
        result.set(text == slint::SharedString::from(slint::platform::Key::DownArrow));
    });
    w.invoke_focus_files();
    w.window().dispatch_event(WindowEvent::KeyPressed {
        text: slint::platform::Key::DownArrow.into(),
    });
    assert!(key.get());
    let confirmed = Rc::new(Cell::new(false));
    let result = confirmed.clone();
    w.on_confirm_dialog(move |value| result.set(value == "New folder"));
    w.set_dialog_kind(1);
    w.set_dialog_value("New folder".into());
    w.invoke_focus_dialog();
    w.window().dispatch_event(WindowEvent::KeyPressed {
        text: slint::platform::Key::Return.into(),
    });
    assert!(confirmed.get());
}

#[test]
fn location_and_creation_fields_use_the_shared_editing_context_menu() {
    i_slint_backend_testing::init_no_event_loop();
    let w = FilesWindow::new().unwrap();
    theme_bindings::apply(
        w.global::<Theme>(),
        w.global::<Palette>(),
        &rix_config::ThemeSettings::crisp_light(),
    );
    populate(&w, &rix_config::ThemeSettings::crisp_light());
    w.window().set_size(slint::LogicalSize::new(1080., 720.));
    w.show().unwrap();
    for dialog in [false, true] {
        if dialog {
            w.set_dialog_kind(1);
            w.set_dialog_title("New folder".into());
            w.set_dialog_value("Named folder".into());
        }
        let field = ElementHandle::find_by_accessible_label(
            &w,
            if dialog { "New folder" } else { "Location" },
        )
        .find(|field| field.size().height > 24.)
        .expect("Visible themed input field is missing");
        pointer(&w, center(&field), PointerEventButton::Right);
        assert!(
            ElementHandle::find_by_accessible_label(&w, "Select All")
                .next()
                .is_some(),
            "The field must open the shared four-row editing menu"
        );
        assert!(ElementHandle::find_by_accessible_label(&w, "Paste")
            .next()
            .is_some());
        assert!(ElementHandle::find_by_accessible_label(&w, "File actions")
            .next()
            .is_none());
        pointer(
            &w,
            center(&element(&w, "Select All")),
            PointerEventButton::Left,
        );
        assert!(ElementHandle::find_by_accessible_label(&w, "Select All")
            .next()
            .is_none());
    }
}

#[test]
fn actual_blank_area_drag_selects_rows_and_escape_restores_the_original_selection() {
    use rix_files::{
        fs::{Entry, SortBy},
        selection::{EntryLayout, SelectionRect},
        state::Tab,
    };
    i_slint_backend_testing::init_no_event_loop();
    let w = FilesWindow::new().unwrap();
    theme_bindings::apply(
        w.global::<Theme>(),
        w.global::<Palette>(),
        &rix_config::ThemeSettings::default(),
    );
    populate(&w, &rix_config::ThemeSettings::default());
    let mut tab = Tab::new(1, "/fixture".into(), false, SortBy::Name);
    tab.install_entries(
        w.get_entries()
            .iter()
            .map(|file| Entry {
                path: std::path::Path::new("/fixture").join(file.name.as_str()),
                name: file.name.to_string(),
                is_dir: file.is_dir,
                is_symlink: false,
                size: 0,
                modified: None,
                kind: String::new(),
                icon_name: String::new(),
                icon_names: Vec::new(),
            })
            .collect(),
    );
    tab.select(0, false, false);
    let state = Rc::new(RefCell::new(tab));
    let target = state.clone();
    w.on_begin_rectangle(move |ctrl, shift| target.borrow_mut().begin_rectangle(ctrl, shift));
    let target = state.clone();
    w.on_update_rectangle(move |x1, y1, x2, y2, width, height, gap, columns| {
        target.borrow_mut().update_rectangle(
            SelectionRect { x1, y1, x2, y2 },
            EntryLayout {
                width,
                height,
                gap,
                columns: columns as usize,
            },
        );
    });
    let target = state.clone();
    w.on_finish_rectangle(move |cancelled| target.borrow_mut().end_rectangle(cancelled));
    w.window().set_size(slint::LogicalSize::new(1080., 720.));
    w.show().unwrap();
    assert!(ElementHandle::find_by_accessible_label(&w, "Create")
        .next()
        .is_none());
    let first = element(&w, "Projects");
    let last = element(
        &w,
        "A very long filename that should fit gracefully in the file list.txt",
    );
    let origin = first.absolute_position();
    let size = first.size();
    let last_origin = last.absolute_position();
    let from = slint::LogicalPosition::new(
        origin.x + size.width - 24.,
        last_origin.y + last.size().height + 8.,
    );
    let to = slint::LogicalPosition::new(origin.x + 24., origin.y + size.height * 1.5);
    w.window()
        .dispatch_event(WindowEvent::PointerMoved { position: from });
    w.window().dispatch_event(WindowEvent::PointerPressed {
        position: from,
        button: PointerEventButton::Left,
    });
    assert!(state.borrow().selected.is_empty());
    w.window()
        .dispatch_event(WindowEvent::PointerMoved { position: to });
    assert_eq!(
        state.borrow().selected.len(),
        6,
        "Reverse rubber band must intersect the six lower rows"
    );
    w.window().dispatch_event(WindowEvent::KeyPressed {
        text: slint::platform::Key::Escape.into(),
    });
    assert_eq!(
        state.borrow().selected_paths(),
        vec![std::path::PathBuf::from("/fixture/Projects")]
    );
    w.window().dispatch_event(WindowEvent::PointerReleased {
        position: to,
        button: PointerEventButton::Left,
    });
    assert_eq!(
        state.borrow().selected.len(),
        1,
        "Release after Escape cannot reapply the cancelled rectangle"
    );
}

#[test]
fn drag_threshold_preserves_multiselection_and_does_not_activate_or_collapse_on_release() {
    i_slint_backend_testing::init_no_event_loop();
    let w = FilesWindow::new().unwrap();
    theme_bindings::apply(
        w.global::<Theme>(),
        w.global::<Palette>(),
        &rix_config::ThemeSettings::default(),
    );
    populate(&w, &rix_config::ThemeSettings::default());
    w.window().set_size(slint::LogicalSize::new(1080., 720.));
    w.show().unwrap();
    let drags = Rc::new(Cell::new(0));
    let target = drags.clone();
    w.on_begin_file_drag(move |index| {
        assert_eq!(index, 0);
        target.set(target.get() + 1);
        true
    });
    let selections = Rc::new(Cell::new(0));
    let target = selections.clone();
    w.on_select_entry(move |_, _, _| target.set(target.get() + 1));
    let activations = Rc::new(Cell::new(0));
    let target = activations.clone();
    w.on_activate_entry(move |_| target.set(target.get() + 1));
    let point = center(&element(&w, "Projects"));
    w.window()
        .dispatch_event(WindowEvent::PointerMoved { position: point });
    w.window().dispatch_event(WindowEvent::PointerPressed {
        position: point,
        button: PointerEventButton::Left,
    });
    assert_eq!(
        selections.get(),
        0,
        "Press uses context selection which preserves selected files"
    );
    let moved = slint::LogicalPosition::new(point.x + 40., point.y + 10.);
    w.window()
        .dispatch_event(WindowEvent::PointerMoved { position: moved });
    w.window().dispatch_event(WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(moved.x + 10., moved.y),
    });
    w.window().dispatch_event(WindowEvent::PointerReleased {
        position: moved,
        button: PointerEventButton::Left,
    });
    assert_eq!(
        drags.get(),
        1,
        "A held gesture must start only one native drag"
    );
    assert_eq!(
        selections.get(),
        0,
        "Drag release cannot collapse a multi-selection"
    );
    assert_eq!(activations.get(), 0);
    w.invoke_cancel_drag_gesture();
    pointer(&w, point, PointerEventButton::Left);
    assert_eq!(
        selections.get(),
        1,
        "A subsequent plain click still collapses the selection"
    );
}

#[test]
fn control_drag_preserves_selected_files_and_control_click_toggles_only_on_release() {
    use slint::platform::Key;
    i_slint_backend_testing::init_no_event_loop();
    let w = FilesWindow::new().unwrap();
    let theme = rix_config::ThemeSettings::default();
    theme_bindings::apply(w.global::<Theme>(), w.global::<Palette>(), &theme);
    populate(&w, &theme);
    let items: Vec<_> = w
        .get_entries()
        .iter()
        .enumerate()
        .map(|(index, mut item)| {
            item.selected = index < 2;
            item
        })
        .collect();
    w.set_entries(model(items));
    w.window().set_size(slint::LogicalSize::new(1080., 720.));
    w.show().unwrap();
    let selections = Rc::new(RefCell::new(Vec::new()));
    let target = selections.clone();
    w.on_select_entry(move |index, ctrl, shift| target.borrow_mut().push((index, ctrl, shift)));
    let drags = Rc::new(Cell::new(0));
    let target = drags.clone();
    w.on_begin_file_drag(move |index| {
        assert_eq!(index, 0);
        target.set(target.get() + 1);
        true
    });
    w.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Control.into(),
    });
    let point = center(&element(&w, "Projects"));
    w.window()
        .dispatch_event(WindowEvent::PointerMoved { position: point });
    w.window().dispatch_event(WindowEvent::PointerPressed {
        position: point,
        button: PointerEventButton::Left,
    });
    assert!(
        selections.borrow().is_empty(),
        "Ctrl press on a selected file must preserve the whole drag selection"
    );
    let moved = slint::LogicalPosition::new(point.x + 40., point.y + 10.);
    w.window()
        .dispatch_event(WindowEvent::PointerMoved { position: moved });
    w.window().dispatch_event(WindowEvent::PointerReleased {
        position: moved,
        button: PointerEventButton::Left,
    });
    assert_eq!(drags.get(), 1);
    assert!(
        selections.borrow().is_empty(),
        "Ctrl drag must not toggle or collapse selected files"
    );
    w.invoke_cancel_drag_gesture();
    w.window()
        .dispatch_event(WindowEvent::PointerMoved { position: point });
    w.window().dispatch_event(WindowEvent::PointerPressed {
        position: point,
        button: PointerEventButton::Left,
    });
    assert!(selections.borrow().is_empty());
    w.window().dispatch_event(WindowEvent::PointerReleased {
        position: point,
        button: PointerEventButton::Left,
    });
    assert_eq!(
        &*selections.borrow(),
        &[(0, true, false)],
        "A Ctrl click must still toggle exactly once"
    );
    w.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Control.into(),
    });
}

#[test]
fn file_shortcuts_bubble_from_toolbar_without_taking_text_edit_shortcuts() {
    use slint::platform::Key;
    i_slint_backend_testing::init_no_event_loop();
    let window = FilesWindow::new().unwrap();
    theme_bindings::apply(
        window.global::<Theme>(),
        window.global::<Palette>(),
        &rix_config::ThemeSettings::default(),
    );
    populate(&window, &rix_config::ThemeSettings::default());
    window
        .window()
        .set_size(slint::LogicalSize::new(1080., 720.));
    window.show().unwrap();
    let shortcuts = Rc::new(RefCell::new(Vec::new()));
    let result = shortcuts.clone();
    window.on_keyboard(move |text, ctrl, shift, alt| {
        if ctrl && text != slint::SharedString::from(Key::Control) {
            result.borrow_mut().push((text.to_string(), shift, alt));
        }
    });
    let press = |text: slint::SharedString| {
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        window
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text });
    };
    let ctrl = |text: &str| {
        window.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::Control.into(),
        });
        press(text.into());
        window.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Control.into(),
        });
    };
    window.invoke_focus_files();
    ctrl("a");
    assert_eq!(shortcuts.borrow().len(), 1);
    window.set_search("original query".into());
    window.invoke_focus_search();
    ctrl("a");
    press("replacement".into());
    assert_eq!(
        window.get_search(),
        "replacement",
        "Ctrl+A must select text in the search field"
    );
    assert_eq!(
        shortcuts.borrow().len(),
        1,
        "Text selection must not select files"
    );
    ctrl("f");
    assert_eq!(
        shortcuts.borrow().last().unwrap().0,
        "f",
        "Application navigation must bubble out of text fields"
    );

    press(Key::Tab.into());
    ctrl("a");
    assert_eq!(
        shortcuts.borrow().last().unwrap().0,
        "a",
        "A focused toolbar button must not lose file shortcuts"
    );
    assert_eq!(shortcuts.borrow().len(), 3);
    window.set_dialog_kind(1);
    window.set_dialog_value("original name".into());
    window.invoke_focus_dialog();
    ctrl("a");
    press("new name".into());
    assert_eq!(window.get_dialog_value(), "new name");
    assert_eq!(
        shortcuts.borrow().len(),
        3,
        "A modal must keep file shortcuts scoped"
    );
}

#[test]
#[ignore = "requires the private Wayland/input fixture in tests/keyboard_wayland.py"]
fn native_keyboard_shortcut_probe() {
    let report = std::path::PathBuf::from(std::env::var_os("RIX_FILES_KEYBOARD_REPORT").unwrap());
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
        .unwrap();
    slint::set_xdg_app_id("org.rixde.Files.KeyboardFixture").unwrap();
    let window = FilesWindow::new().unwrap();
    rix_files::keyboard::install(window.window());
    theme_bindings::apply(
        window.global::<Theme>(),
        window.global::<Palette>(),
        &rix_config::ThemeSettings::default(),
    );
    populate(&window, &rix_config::ThemeSettings::default());
    let shortcuts = Rc::new(RefCell::new(Vec::new()));
    let received = shortcuts.clone();
    let weak = window.as_weak();
    window.on_keyboard(move |text, ctrl, shift, alt| {
        if !ctrl {
            return;
        }
        let window = weak.upgrade().unwrap();
        received
            .borrow_mut()
            .push(serde_json::json!({"text": text.as_str(), "shift": shift, "alt": alt}));
        match text.to_lowercase().as_str() {
            "a" => window.set_selection_count(3),
            "f" => window.invoke_focus_search(),
            "l" => window.invoke_focus_location(),
            "n" if shift => {
                window.set_dialog_kind(1);
                window.set_dialog_value("original name".into());
                window.invoke_focus_dialog();
            }
            _ => {}
        }
    });
    let weak = window.as_weak();
    window.on_dismiss_dialog(move || {
        let window = weak.upgrade().unwrap();
        window.set_dialog_kind(0);
    });
    let timer = slint::Timer::default();
    let weak = window.as_weak();
    timer.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(40), move || {
        let window = weak.upgrade().unwrap();
        let value = serde_json::json!({"shortcuts": *shortcuts.borrow(), "search": window.get_search().as_str(), "location": window.get_location().as_str(), "dialog": window.get_dialog_value().as_str(), "dialog_kind": window.get_dialog_kind(), "selected": window.get_selection_count()});
        let temporary = report.with_extension("tmp");
        std::fs::write(&temporary, serde_json::to_vec(&value).unwrap()).unwrap();
        std::fs::rename(temporary, &report).unwrap();
    });
    window.show().unwrap();
    window.invoke_focus_files();
    window.run().unwrap();
}
#[test]
fn modal_keyboard_is_scoped_and_file_focus_returns_after_dismissal() {
    use slint::platform::Key;
    i_slint_backend_testing::init_no_event_loop();
    let w = FilesWindow::new().unwrap();
    theme_bindings::apply(
        w.global::<Theme>(),
        w.global::<Palette>(),
        &rix_config::ThemeSettings::default(),
    );
    populate(&w, &rix_config::ThemeSettings::default());
    w.global::<ApplicationChooser>()
        .set_applications(model(vec![
            ChooserApplication {
                id: "editor.desktop".into(),
                label: "Text Editor".into(),
                ..Default::default()
            },
            ChooserApplication {
                id: "viewer.desktop".into(),
                label: "Viewer".into(),
                ..Default::default()
            },
        ]));
    w.global::<ApplicationChooser>().set_selected_index(0);
    w.window().set_size(slint::LogicalSize::new(1080., 720.));
    w.show().unwrap();

    let dismissed = Rc::new(Cell::new(0));
    let result = dismissed.clone();
    let weak = w.as_weak();
    w.on_dismiss_dialog(move || {
        result.set(result.get() + 1);
        weak.upgrade().unwrap().set_dialog_kind(0);
    });
    let confirmed = Rc::new(RefCell::new(Vec::new()));
    let result = dismissed.clone();
    let weak = w.as_weak();
    w.global::<ApplicationChooser>().on_cancelled(move || {
        result.set(result.get() + 1);
        weak.upgrade().unwrap().set_dialog_kind(0);
    });
    let result = confirmed.clone();
    let weak = w.as_weak();
    w.on_confirm_dialog(move |value| {
        result.borrow_mut().push(value.to_string());
        weak.upgrade().unwrap().set_dialog_kind(0);
    });
    let chosen = Rc::new(Cell::new(-1));
    let result = chosen.clone();
    let weak = w.as_weak();
    w.global::<ApplicationChooser>().on_confirm(move |id, _| {
        result.set(if id == "viewer.desktop" { 1 } else { 0 });
        weak.upgrade().unwrap().set_dialog_kind(0);
    });
    let file_keys = Rc::new(Cell::new(0));
    let result = file_keys.clone();
    w.on_keyboard(move |_, _, _, _| result.set(result.get() + 1));
    let press = |key: Key| {
        slint::platform::update_timers_and_animations();
        w.window()
            .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
        w.window()
            .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
    };

    for kind in 1..=6 {
        w.set_dialog_kind(kind);
        let before = file_keys.get();
        press(Key::Escape);
        assert_eq!(
            w.get_dialog_kind(),
            0,
            "Escape must dismiss modal kind {kind}"
        );
        assert_eq!(
            dismissed.get(),
            kind,
            "Escape must invoke the dismissal callback once"
        );
        assert_eq!(
            file_keys.get(),
            before,
            "Modal Escape must not reach file shortcuts"
        );

        press(Key::DownArrow);
        assert_eq!(
            file_keys.get(),
            before + 1,
            "File keyboard focus must return after kind {kind}"
        );
    }
    for kind in [1, 2, 3, 6] {
        w.set_dialog_value(format!("value {kind}").into());
        w.set_dialog_kind(kind);
        w.invoke_focus_dialog();
        press(Key::Return);
        assert_eq!(
            w.get_dialog_kind(),
            0,
            "Return must confirm modal kind {kind}"
        );
    }
    assert_eq!(
        *confirmed.borrow(),
        ["value 1", "value 2", "value 3", "value 6"]
    );

    w.set_dialog_kind(4);
    w.invoke_focus_dialog();
    press(Key::Return);
    assert_eq!(
        dismissed.get(),
        7,
        "Return must close the details/error dialog"
    );
    w.set_dialog_kind(5);
    w.invoke_focus_dialog();
    press(Key::DownArrow);
    press(Key::Return);
    assert_eq!(
        chosen.get(),
        1,
        "Open-with keyboard navigation must activate the selected application"
    );
    assert_eq!(w.get_dialog_kind(), 0);

    w.set_dialog_kind(1);
    w.invoke_focus_dialog();

    press(Key::Tab);
    press(Key::Return);
    assert_eq!(dismissed.get(), 8);
    assert_eq!(w.get_dialog_kind(), 0);
}
fn assert_inside(element: &ElementHandle, width: f32, height: f32) {
    let origin = element.absolute_position();
    let size = element.size();
    assert!(
        size.width > 0. && size.height > 0.,
        "Popup must have a usable size"
    );
    assert!(
        origin.x >= 0. && origin.y >= 0.,
        "Popup escaped the top/left edge: {origin:?}"
    );
    assert!(origin.x + size.width <= width + 0.5 && origin.y + size.height <= height + 0.5,
        "Popup escaped the bottom/right edge: origin={origin:?}, size={size:?}, window={width}x{height}");
}
fn close_menu(w: &FilesWindow) {
    w.window().dispatch_event(WindowEvent::KeyPressed {
        text: slint::platform::Key::Escape.into(),
    });
    w.window().dispatch_event(WindowEvent::KeyReleased {
        text: slint::platform::Key::Escape.into(),
    });
    slint::platform::update_timers_and_animations();
    assert!(
        ElementHandle::find_by_accessible_label(w, "File actions")
            .next()
            .is_none(),
        "Escape must dismiss the file menu"
    );
    assert!(
        ElementHandle::find_by_accessible_label(w, "Folder actions")
            .next()
            .is_none(),
        "Escape must dismiss the folder menu"
    );
}
#[test]
fn right_click_uses_the_pointer_and_background_actions_fit_the_window() {
    i_slint_backend_testing::init_no_event_loop();
    let w = FilesWindow::new().unwrap();
    let theme = current_theme();
    theme_bindings::apply(w.global::<Theme>(), w.global::<Palette>(), &theme);
    skins::install(w.global::<SkinRenderer>(), w.as_weak());
    populate(&w, &theme);
    let context = Rc::new(Cell::new(-1));
    let selected = context.clone();
    w.on_context_entry(move |index| selected.set(index));
    let actions = Rc::new(RefCell::new(Vec::new()));
    let result = actions.clone();
    w.on_action(move |action| result.borrow_mut().push(action.to_string()));
    for (width, height) in [(760., 480.), (1080., 720.), (1440., 900.)] {
        w.window().set_size(slint::LogicalSize::new(width, height));
        w.show().unwrap();
        for grid in [false, true] {
            w.set_grid_view(grid);
            slint::platform::update_timers_and_animations();
            let cell = element(&w, "Projects");
            let cell_origin = cell.absolute_position();
            let first = slint::LogicalPosition::new(
                cell_origin.x + 20.,
                cell_origin.y + cell.size().height / 2.,
            );
            pointer(&w, first, PointerEventButton::Right);
            assert_eq!(
                context.get(),
                0,
                "Right click must identify the file under the pointer"
            );
            let menu = element(&w, "File actions");
            assert_inside(&menu, width, height);
            let origin = menu.absolute_position();
            assert!(
                (origin.x - first.x).abs() < 1.,
                "File context menu must anchor to the actual pointer: {origin:?} vs {first:?}"
            );
            close_menu(&w);
            let shifted = slint::LogicalPosition::new(first.x + 24., first.y);
            pointer(&w, shifted, PointerEventButton::Right);
            let menu = element(&w, "File actions");
            assert!(
                (menu.absolute_position().x - origin.x - 24.).abs() < 1.,
                "Context menu must follow the pointer within the same cell"
            );
            close_menu(&w);

            if grid {
                let between_cards = slint::LogicalPosition::new(
                    cell_origin.x + cell.size().width + theme.appearance.metric("spacing_lg") / 2.,
                    first.y,
                );
                pointer(&w, between_cards, PointerEventButton::Right);
                assert_inside(&element(&w, "Folder actions"), width, height);
                close_menu(&w);
            }

            w.set_entries(model(Vec::new()));
            w.set_grid_rows(model(Vec::new()));
            let contents = element(&w, "Folder contents");
            let origin = contents.absolute_position();
            let size = contents.size();
            let inset = theme.appearance.metric("spacing_xl") + 12.;
            let bottom_right = slint::LogicalPosition::new(
                origin.x + size.width - inset,
                origin.y + size.height - inset,
            );
            pointer(&w, bottom_right, PointerEventButton::Right);
            let menu = element(&w, "Folder actions");
            assert_inside(&menu, width, height);
            assert!(
                menu.absolute_position().x < bottom_right.x,
                "Background popup must clamp at the right edge"
            );
            let new_folder = popup_action(&w, "New folder", &menu);
            pointer(&w, center(&new_folder), PointerEventButton::Left);
            assert_eq!(
                actions.borrow().last().map(String::as_str),
                Some("new-folder"),
                "Blank-space context menu must expose a working folder action"
            );
            close_menu(&w);
            let outer_padding = slint::LogicalPosition::new(
                origin.x + size.width - 4.,
                origin.y + size.height - 4.,
            );
            pointer(&w, outer_padding, PointerEventButton::Right);
            assert_inside(&element(&w, "Folder actions"), width, height);
            close_menu(&w);
            populate(&w, &theme);
        }
    }
}
#[test]
fn tab_add_button_follows_the_last_tab_and_grid_cells_keep_a_readable_width() {
    i_slint_backend_testing::init_no_event_loop();
    let w = FilesWindow::new().unwrap();
    let theme = current_theme();
    theme_bindings::apply(w.global::<Theme>(), w.global::<Palette>(), &theme);
    populate(&w, &theme);
    let added = Rc::new(Cell::new(false));
    let result = added.clone();
    w.on_action(move |action| result.set(action == "new-tab"));
    for width in [760., 1080., 1440.] {
        w.window().set_size(slint::LogicalSize::new(width, 720.));
        w.show().unwrap();
        slint::platform::update_timers_and_animations();
        let places = element(&w, "Places");
        let contents = element(&w, "Folder contents");
        assert!(
            places.absolute_position().x < 1.,
            "Places sidebar must remain against the left edge"
        );
        assert!(
            contents.absolute_position().x >= places.absolute_position().x + places.size().width,
            "Folder contents must begin after the sidebar without overlapping it"
        );
        let tabs = element(&w, "Tabs");
        let location = element(&w, "Location");
        assert!(
            location.absolute_position().y + 0.5 >= tabs.absolute_position().y + tabs.size().height,
            "Location field must remain below the tab strip without overlapping it"
        );
        let plus = element(&w, "New tab");
        let last_tab = element(&w, "Downloads");
        let gap =
            plus.absolute_position().x - last_tab.absolute_position().x - last_tab.size().width;
        assert!(
            gap >= -0.5 && gap <= theme.appearance.metric("default_spacing") * 2. + 1.,
            "Add-tab button must immediately follow the last tab, gap={gap} at width={width}"
        );
        assert_inside(&plus, width, 720.);
        pointer(&w, center(&plus), PointerEventButton::Left);
        assert!(added.get(), "Adjacent add-tab button must be clickable");
        w.set_grid_view(true);
        slint::platform::update_timers_and_animations();
        let first = element(&w, "Projects");
        let second = element(&w, "Reports");
        assert!(
            first.size().width <= 180.,
            "Grid cells must not stretch into large cards at width={width}"
        );
        assert!(
            (first.size().width - second.size().width).abs() < 1.,
            "Grid columns must use consistent cell widths"
        );
        assert!(
            second.absolute_position().x > first.absolute_position().x,
            "The responsive grid must lay out files horizontally"
        );
    }
}

#[test]
fn disk_card_stays_in_sidebar_and_opens_the_mount_at_compact_and_large_sizes() {
    i_slint_backend_testing::init_no_event_loop();
    let w = FilesWindow::new().unwrap();
    let opened = Rc::new(Cell::new(0));
    let callback = opened.clone();
    w.on_open_disk(move || callback.set(callback.get() + 1));
    for theme in [current_theme(), rix_config::ThemeSettings::crisp_light()] {
        theme_bindings::apply(w.global::<Theme>(), w.global::<Palette>(), &theme);
        populate(&w, &theme);
        for (width, height) in [(760., 480.), (1080., 720.), (1440., 900.)] {
            w.window().set_size(slint::LogicalSize::new(width, height));
            w.show().unwrap();
            slint::platform::update_timers_and_animations();
            let sidebar = element(&w, "Places");
            let card = element(&w, "Disk storage");
            let contents = element(&w, "Folder contents");
            assert_inside(&card, width, height);
            assert!(card.absolute_position().x >= sidebar.absolute_position().x);
            assert!(
                card.absolute_position().x + card.size().width <= contents.absolute_position().x
            );
            assert!(card.absolute_position().y > element(&w, "Home").absolute_position().y);
            let before = opened.get();
            pointer(&w, center(&card), PointerEventButton::Left);
            assert_eq!(opened.get(), before + 1);
        }
    }
    w.set_disk_mount("".into());
    w.set_disk_ready(false);
    let before = opened.get();
    pointer(
        &w,
        center(&element(&w, "Disk storage")),
        PointerEventButton::Left,
    );
    assert_eq!(
        opened.get(),
        before,
        "A failed query must not open a fake mount"
    );
}
#[test]
fn sidebar_context_menu_unpins_the_place_under_the_pointer() {
    i_slint_backend_testing::init_no_event_loop();
    let w = FilesWindow::new().unwrap();
    let theme = current_theme();
    theme_bindings::apply(w.global::<Theme>(), w.global::<Palette>(), &theme);
    skins::install(w.global::<SkinRenderer>(), w.as_weak());
    populate(&w, &theme);
    let unpinned = Rc::new(Cell::new(-1));
    let result = unpinned.clone();
    w.on_unpin_place(move |index| result.set(index));
    let keys = Rc::new(Cell::new(false));
    let result = keys.clone();
    w.on_keyboard(move |text, _, _, _| {
        result.set(text == slint::SharedString::from(slint::platform::Key::DownArrow));
    });
    for (width, height) in [(760., 480.), (1080., 720.), (1440., 900.)] {
        w.window().set_size(slint::LogicalSize::new(width, height));
        w.show().unwrap();
        slint::platform::update_timers_and_animations();
        let sidebar = element(&w, "Places");
        let place = popup_action(&w, "Documents", &sidebar);
        pointer(&w, center(&place), PointerEventButton::Right);
        let popup = element(&w, "Place actions");
        assert_inside(&popup, width, height);
        assert_eq!(
            popup.size().height,
            w.global::<Theme>().get_menu_row_height()
                + 2. * w.global::<Theme>().get_context_menu_padding(),
            "Single-row menu must not reuse the previous menu's layout"
        );
        let action = popup_action(&w, "Unpin from Places", &popup);
        pointer(&w, center(&action), PointerEventButton::Left);
        assert_eq!(
            unpinned.get(),
            1,
            "Unpin must receive the sidebar place index"
        );
        assert!(ElementHandle::find_by_accessible_label(&w, "Place actions")
            .next()
            .is_none());
        keys.set(false);
        w.window().dispatch_event(WindowEvent::KeyPressed {
            text: slint::platform::Key::DownArrow.into(),
        });
        w.window().dispatch_event(WindowEvent::KeyReleased {
            text: slint::platform::Key::DownArrow.into(),
        });
        assert!(
            keys.get(),
            "Unpin must dismiss the menu and return focus to the files"
        );
    }
}
#[test]
fn double_click_requires_the_same_file_across_adjacent_rows() {
    i_slint_backend_testing::init_no_event_loop();
    let w = FilesWindow::new().unwrap();
    let theme = rix_config::ThemeSettings::default();
    theme_bindings::apply(w.global::<Theme>(), w.global::<Palette>(), &theme);
    populate(&w, &theme);
    w.window().set_size(slint::LogicalSize::new(1080., 720.));
    w.show().unwrap();
    let activated = Rc::new(RefCell::new(Vec::new()));
    let result = activated.clone();
    w.on_activate_entry(move |index| result.borrow_mut().push(index));
    let selected = Rc::new(RefCell::new(Vec::new()));
    let result = selected.clone();
    w.on_select_entry(move |index, _, _| result.borrow_mut().push(index));
    let first = element(&w, "Projects");
    let second = element(&w, "Reports");
    let first_origin = first.absolute_position();
    let second_origin = second.absolute_position();
    let first_edge = slint::LogicalPosition::new(
        first_origin.x + 60.,
        first_origin.y + first.size().height - 1.,
    );
    let second_edge = slint::LogicalPosition::new(first_edge.x, second_origin.y + 1.);
    assert!(
        (second_edge.y - first_edge.y).abs() < 10.,
        "Fixture must exercise the double-click distance threshold"
    );
    pointer(&w, first_edge, PointerEventButton::Left);
    pointer(&w, second_edge, PointerEventButton::Left);
    assert_eq!(*selected.borrow(), [0, 1]);
    assert!(
        activated.borrow().is_empty(),
        "Two clicks on adjacent files must not activate either file"
    );
    let same_file = center(&second);
    pointer(&w, same_file, PointerEventButton::Left);
    pointer(&w, same_file, PointerEventButton::Left);
    assert_eq!(
        *activated.borrow(),
        [1],
        "Two clicks on the same file must activate that file once"
    );
}
#[test]
fn render_theme_changes_grid_and_confirmation_at_multiple_sizes() {
    use slint::platform::{
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
        Platform, PlatformError, WindowAdapter,
    };
    struct Surface(Rc<MinimalSoftwareWindow>);
    impl Platform for Surface {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
            Ok(self.0.clone())
        }
    }
    let surface = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Surface(surface.clone()))).unwrap();
    let w = FilesWindow::new().unwrap();
    skins::install(w.global::<SkinRenderer>(), w.as_weak());
    w.set_desktop_material_supported(true);
    let output = std::env::var_os("RIX_FILES_RENDER_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("rix-files-ui"));
    std::fs::create_dir_all(&output).unwrap();
    let themes = [
        ("dark", rix_config::ThemeSettings::default()),
        ("light", rix_config::ThemeSettings::crisp_light()),
        ("current", current_theme()),
    ];
    let mut cases = Vec::new();
    for (name, theme) in &themes {
        for (width, height) in [(760u32, 480u32), (1080, 720), (1440, 900)] {
            for grid in [false, true] {
                cases.push((
                    format!("{name}-{width}-{}", if grid { "grid" } else { "list" }),
                    theme.clone(),
                    width,
                    height,
                    grid,
                    0,
                ));
                if !grid {
                    cases.push((
                        format!("{name}-{width}-open-with"),
                        theme.clone(),
                        width,
                        height,
                        grid,
                        5,
                    ));
                }
            }
        }
    }
    cases.extend([
        (
            "compact-dialog".into(),
            rix_config::ThemeSettings::default(),
            760,
            480,
            false,
            3,
        ),
        (
            "properties-dialog".into(),
            rix_config::ThemeSettings::crisp_light(),
            1080,
            720,
            false,
            4,
        ),
    ]);
    for (name, theme, width, height, grid, dialog) in cases {
        theme_bindings::apply(w.global::<Theme>(), w.global::<Palette>(), &theme);
        populate(&w, &theme);
        let chooser = w.global::<ApplicationChooser>();
        chooser.set_title("Open with".into());
        chooser.set_description("Architecture.pdf · PDF document".into());
        chooser.set_selected_index(0);
        chooser.set_allow_always(true);
        chooser.set_always_use(true);
        chooser.set_show_all(false);
        chooser.set_applications(model(vec![
            ChooserApplication {
                id: "viewer.desktop".into(),
                label: "Document Viewer".into(),
                detail: "Supports this file type".into(),
                icon: themed_icon(&theme, false),
                recommended: true,
                is_default: true,
            },
            ChooserApplication {
                id: "browser.desktop".into(),
                label: "RixBrowser".into(),
                detail: "Supports this file type".into(),
                icon: themed_icon(&theme, false),
                recommended: true,
                is_default: false,
            },
            ChooserApplication {
                id: "editor.desktop".into(),
                label: "PDF Editor".into(),
                detail: "Supports this file type".into(),
                icon: themed_icon(&theme, false),
                recommended: true,
                is_default: false,
            },
        ]));
        w.set_grid_view(grid);
        w.set_dialog_kind(dialog);
        w.set_dialog_title(
            if dialog == 4 {
                "Properties"
            } else {
                "Delete permanently?"
            }
            .into(),
        );
        w.set_dialog_text(if dialog == 4 { "/home/demo/Documents/notes.txt\nType: File\nSize: 24.5 KiB\nModified: 2026-10-04 20:00\nPermissions: 0644\nOwner ID: 1000\nGroup ID: 1000" } else { "Delete the selected file? This action cannot be undone." }.into());
        w.window().set_size(slint::PhysicalSize::new(width, height));
        w.show().unwrap();
        slint::platform::update_timers_and_animations();
        w.invoke_grid_columns_changed(w.get_grid_columns());
        let capture = |name: &str| {
            let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
            surface.request_redraw();
            surface.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, width as usize);
            });
            let bytes: Vec<_> = pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
            assert!(
                bytes.iter().any(|b| *b > 100),
                "Rendered UI must contain visible content"
            );
            image::save_buffer(
                output.join(format!("{name}.png")),
                &bytes,
                width,
                height,
                image::ColorType::Rgb8,
            )
            .unwrap();
        };
        capture(&name);
        let expected = w.global::<Theme>().get_window_background();
        assert_eq!(
            expected,
            slint::Color::from_argb_f32(
                theme.appearance.color("window_background").a,
                theme.appearance.color("window_background").r,
                theme.appearance.color("window_background").g,
                theme.appearance.color("window_background").b
            )
        );
        assert_eq!(
            w.global::<Theme>().get_font_family().as_str(),
            theme.appearance.font_family
        );
        assert_eq!(
            w.global::<Theme>().get_radius_sm(),
            theme.appearance.metric("radius_sm")
        );
        if !theme.appearance.menu_frame.is_empty()
            && rix_assets::resolve(&theme.appearance.menu_frame).is_some()
        {
            assert!(
                w.global::<Theme>().get_menu_frame().available,
                "Available theme menu skin must be loaded"
            );
        }
        if name == "current-760-grid" || name == "light-760-list" {
            pointer(
                &w,
                center(&element(&w, "Projects")),
                PointerEventButton::Right,
            );
            assert_inside(&element(&w, "File actions"), width as f32, height as f32);
            capture(&format!("{name}-file-menu"));
            close_menu(&w);
            w.set_entries(model(Vec::new()));
            w.set_grid_rows(model(Vec::new()));
            let contents = element(&w, "Folder contents");
            let origin = contents.absolute_position();
            let size = contents.size();
            let inset = theme.appearance.metric("spacing_xl") + 12.;
            pointer(
                &w,
                slint::LogicalPosition::new(
                    origin.x + size.width - inset,
                    origin.y + size.height - inset,
                ),
                PointerEventButton::Right,
            );
            assert_inside(&element(&w, "Folder actions"), width as f32, height as f32);
            capture(&format!("{name}-folder-menu"));
            close_menu(&w);
        }
    }
}

#[test]
fn entire_window_and_dialog_follow_live_material_without_opaque_patches() {
    use slint::platform::{
        software_renderer::{
            MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType, TargetPixel,
        },
        Platform, PlatformError, WindowAdapter,
    };
    struct Surface(Rc<MinimalSoftwareWindow>);
    impl Platform for Surface {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
            Ok(self.0.clone())
        }
    }
    let surface = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Surface(surface.clone()))).unwrap();
    let w = FilesWindow::new().unwrap();
    skins::install(w.global::<SkinRenderer>(), w.as_weak());
    let output = std::env::var_os("RIX_FILES_RENDER_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("rix-files-ui"));
    std::fs::create_dir_all(&output).unwrap();
    let fixtures = tempfile::tempdir().unwrap();

    let frame = |name: &str, color: &str, opacity: f32| {
        let parts: String = rix_assets::FRAME_PARTS
            .iter()
            .enumerate()
            .map(|(i, part)| format!(
                "<rect id='{part}' x='{}' y='{}' width='8' height='8' fill='{color}' fill-opacity='{opacity}'/>",
                i % 3 * 8, i / 3 * 8
            ))
            .collect();
        let path = fixtures.path().join(name);
        std::fs::write(
            &path,
            format!("<svg xmlns='http://www.w3.org/2000/svg' width='24' height='24'>{parts}</svg>"),
        )
        .unwrap();
        path.to_string_lossy().into_owned()
    };
    let mut svg_theme = rix_config::ThemeSettings::aero_glass();
    svg_theme.opacity = 0.63;
    svg_theme.appearance.panel_frame = frame("panel.svg", "#1040a0", 0.45);
    svg_theme.appearance.menu_frame = frame("menu.svg", "#18b040", 0.7);
    let mut glass_theme = rix_config::ThemeSettings::aero_glass();
    glass_theme.opacity = 0.5;
    glass_theme.appearance.panel_frame.clear();
    glass_theme.appearance.menu_frame.clear();
    glass_theme
        .appearance
        .palette
        .insert("glass_start".into(), "#123a72".into());
    glass_theme
        .appearance
        .palette
        .insert("glass_end".into(), "#123a72".into());
    let mut light_theme = rix_config::ThemeSettings::crisp_light();
    light_theme.appearance.panel_frame.clear();
    light_theme.appearance.menu_frame.clear();
    let mut flat_theme = glass_theme.clone();
    flat_theme.blur_radius = 0;
    let mut flat_svg_theme = svg_theme.clone();
    flat_svg_theme.blur_radius = 0;
    let cases = [
        ("current-svg", current_theme(), true, true),
        ("distinct-svg", svg_theme.clone(), true, true),
        ("distinct-svg-unsupported", svg_theme.clone(), true, false),
        ("distinct-svg-disabled", svg_theme, false, true),
        ("distinct-svg-blur-disabled", flat_svg_theme, true, true),
        ("glass", glass_theme.clone(), true, true),
        ("live-light", light_theme, true, true),
        ("material-disabled", glass_theme.clone(), false, true),
        ("unsupported-compositor", glass_theme, true, false),
        ("blur-disabled", flat_theme, true, true),
    ];
    let width = 1080u32;
    let height = 720u32;
    w.window().set_size(slint::PhysicalSize::new(width, height));
    w.show().unwrap();
    let capture = |name: &str| {
        slint::platform::update_timers_and_animations();
        let mut pixels = vec![PremultipliedRgbaColor::default(); (width * height) as usize];
        surface.request_redraw();
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width as usize);
        });
        let bytes: Vec<_> = pixels
            .iter()
            .flat_map(|p| {
                let straight = |component: u8| {
                    if p.alpha == 0 {
                        0
                    } else {
                        (u16::from(component) * 255 / u16::from(p.alpha)).min(255) as u8
                    }
                };
                [
                    straight(p.red),
                    straight(p.green),
                    straight(p.blue),
                    p.alpha,
                ]
            })
            .collect();
        image::save_buffer(
            output.join(format!("full-material-{name}.png")),
            &bytes,
            width,
            height,
            image::ColorType::Rgba8,
        )
        .unwrap();
        pixels
    };
    let channels = |p: PremultipliedRgbaColor| [p.red, p.green, p.blue, p.alpha];
    let assert_pixel =
        |actual: PremultipliedRgbaColor, expected: PremultipliedRgbaColor, context: &str| {
            assert!(
                channels(actual)
                    .into_iter()
                    .zip(channels(expected))
                    .all(|(a, b)| a.abs_diff(b) <= 3),
                "{context}: actual {:?}, expected {:?}",
                channels(actual),
                channels(expected)
            );
        };
    let mut evidence = Vec::new();
    let mut glass_canvas = None;
    for (name, theme, enabled, supported) in cases {
        theme_bindings::apply(w.global::<Theme>(), w.global::<Palette>(), &theme);
        populate(&w, &theme);
        w.set_desktop_material_enabled(enabled);
        w.set_desktop_material_supported(supported);
        w.set_grid_view(true);
        w.set_dialog_kind(0);
        w.global::<DialogBackdrop>().set_ready(false);
        w.global::<DialogBackdrop>()
            .set_image(slint::Image::default());
        w.set_operation_active(true);
        w.set_operation_message("Copying files…".into());
        w.set_operation_progress(0.4);
        let material = enabled && supported && theme.blur_radius > 0;
        assert_eq!(w.get_using_desktop_material(), material, "{name}");
        let footer_height = theme.appearance.metric("font_size_sm")
            + theme.appearance.metric("default_spacing") * 2.;
        let operation_height =
            theme.appearance.metric("icon_size") + theme.appearance.metric("default_spacing") * 4.;
        let probes = [
            ("chrome", width - 8, 80),
            ("places", 25, height * 2 / 3),
            ("canvas", width * 2 / 3, height * 2 / 3),
            ("status", width * 2 / 3, height - 10),
            (
                "operation",
                width / 2,
                (height as f32 - footer_height - operation_height + 3.) as u32,
            ),
        ];
        let base = capture(name);
        for viewer in [false, true] {
            if viewer {
                let preview = w.global::<FilePreview>();
                preview.set_title("Viewer theme proof".into());
                preview.set_body("".into());
                preview.set_kind(4);
                preview.set_visible(true);
            } else {
                let archive = w.global::<ArchiveBrowser>();
                archive.set_title("Archive theme proof".into());
                archive.set_members(model(Vec::<ArchiveMember>::new()));
                archive.set_visible(true);
            }
            let panel = capture(&format!(
                "{name}-{}",
                if viewer { "viewer" } else { "archive" }
            ));
            let index = (height * 2 / 3 * width + width * 2 / 3) as usize;
            if material {
                assert_pixel(
                    panel[index],
                    base[index],
                    "Archive/viewer must retain the top-level material without a second panel skin",
                );
            } else {
                let color = if viewer {
                    w.global::<Theme>().get_sidebar_background()
                } else {
                    w.global::<Theme>().get_window_background()
                };
                assert_pixel(
                    panel[index],
                    slint::Color::from_rgb_u8(color.red(), color.green(), color.blue()).into(),
                    "Archive/viewer must follow the current opaque theme color",
                );
            }
            w.global::<FilePreview>().set_visible(false);
            w.global::<ArchiveBrowser>().set_visible(false);
        }
        let panel = w.global::<Theme>().get_panel_frame();
        let panel_pixels = if material && panel.available {
            Some(
                w.global::<SkinRenderer>()
                    .invoke_render(panel, width as f32, height as f32, true)
                    .to_rgba8_premultiplied()
                    .expect("Panel skin must render"),
            )
        } else {
            None
        };
        let mut samples = serde_json::Map::new();
        for (region, x, y) in probes {
            let index = (y * width + x) as usize;
            let actual = base[index];
            if let Some(panel_pixels) = &panel_pixels {
                let pixel = panel_pixels.as_slice()[index];
                let scale = |value: u8| (f32::from(value) * theme.opacity).round() as u8;
                assert_pixel(
                    actual,
                    PremultipliedRgbaColor {
                        red: scale(pixel.r),
                        green: scale(pixel.g),
                        blue: scale(pixel.b),
                        alpha: scale(pixel.a),
                    },
                    &format!("{name}/{region} must retain the same panel material"),
                );
            } else if material {
                assert!(
                    actual.alpha.abs_diff((theme.opacity * 255.).round() as u8) <= 2,
                    "{name}/{region} contains an opaque or stacked background: {:?}",
                    channels(actual)
                );
            } else {
                assert_eq!(
                    actual.alpha, 255,
                    "{name}/{region} must be opaque without blur"
                );
                if region == "canvas" {
                    let color = w.global::<Theme>().get_window_background();
                    assert_pixel(
                        actual,
                        slint::Color::from_rgb_u8(color.red(), color.green(), color.blue()).into(),
                        &format!("{name}/canvas must follow the theme's opaque window color"),
                    );
                }
            }
            samples.insert(region.into(), serde_json::json!(channels(actual)));
            if region == "canvas" && name == "glass" {
                glass_canvas = Some(actual);
            }
            if region == "canvas" && name == "live-light" {
                assert!(
                    actual.red > glass_canvas.unwrap().red + 80,
                    "Changing the theme on the existing window must repaint the file canvas"
                );
            }
        }
        w.set_dialog_title("Properties".into());
        w.set_dialog_text("Theme material preview".into());
        w.set_dialog_kind(4);
        let dialog = capture(&format!("{name}-dialog"));
        let center = (height / 2 * width + width / 2) as usize;

        assert_eq!(
            dialog[center].alpha, 255,
            "{name}: no translucent dialog before local blur is ready"
        );
        if name.starts_with("distinct-svg") || !material {
            let color = w.global::<Theme>().get_menu_bg();
            let mut expected: PremultipliedRgbaColor =
                slint::Color::from_rgb_u8(color.red(), color.green(), color.blue()).into();
            let menu = w.global::<Theme>().get_menu_frame();
            if menu.available {
                let pixels = w
                    .global::<SkinRenderer>()
                    .invoke_render_rounded(
                        menu,
                        560.,
                        200.,
                        false,
                        w.global::<Theme>().get_context_menu_radius(),
                    )
                    .to_rgba8_premultiplied()
                    .unwrap();
                let menu = pixels.as_slice()[100 * 560 + 280];
                let scale = |value: u8| (f32::from(value) * theme.opacity).round() as u8;
                expected.blend(PremultipliedRgbaColor {
                    red: scale(menu.r),
                    green: scale(menu.g),
                    blue: scale(menu.b),
                    alpha: scale(menu.a),
                });
            }
            assert_pixel(
                dialog[center],
                expected,
                &format!("{name}: opaque fallback retains menu artwork"),
            );
        }
        if material {
            let rgba = [34u8, 45, 60, 255].repeat((width * height) as usize);
            w.global::<DialogBackdrop>()
                .set_image(slint::Image::from_rgba8(
                    slint::SharedPixelBuffer::clone_from_slice(&rgba, width, height),
                ));
            w.global::<DialogBackdrop>().set_ready(true);
            let prepared = capture(&format!("{name}-dialog-prepared"));

            assert_eq!(prepared[center].alpha, 255);
            if name == "distinct-svg" || name == "glass" {
                let mut expected: PremultipliedRgbaColor =
                    slint::Color::from_rgb_u8(34, 45, 60).into();
                let shadow = w.global::<Theme>().get_shadow();
                expected.blend(
                    slint::Color::from_argb_u8(153, shadow.red(), shadow.green(), shadow.blue())
                        .into(),
                );
                let menu = w.global::<Theme>().get_menu_frame();
                if menu.available {
                    let pixels = w
                        .global::<SkinRenderer>()
                        .invoke_render_rounded(
                            menu,
                            560.,
                            200.,
                            false,
                            w.global::<Theme>().get_context_menu_radius(),
                        )
                        .to_rgba8_premultiplied()
                        .unwrap();
                    let pixel = pixels.as_slice()[100 * 560 + 280];
                    let scale = |value: u8| (f32::from(value) * theme.opacity).round() as u8;
                    expected.blend(PremultipliedRgbaColor {
                        red: scale(pixel.r),
                        green: scale(pixel.g),
                        blue: scale(pixel.b),
                        alpha: scale(pixel.a),
                    });
                } else {
                    expected.blend(w.global::<Theme>().get_glass_start().into());
                }
                assert_pixel(
                    prepared[center],
                    expected,
                    "Prepared dialog uses the current material over local blurred content",
                );
            }
        }
        evidence.push(serde_json::json!({"case": name, "material": material,
            "regions_premultiplied_rgba": samples, "dialog_center_premultiplied_rgba": channels(dialog[center])}));
    }
    std::fs::write(
        output.join("full-material-proof.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
}
