use i_slint_backend_testing::ElementHandle;
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{
    Clipboard, Platform, PlatformError, PointerEventButton, WindowAdapter, WindowEvent,
};
use slint::{ComponentHandle, LogicalPosition, Model};
use std::{cell::RefCell, rc::Rc};

slint::slint! {
    import { LineEdit } from "../../../ui/common/line_edit.slint";
    import { Theme } from "../../../ui/common/theme.slint";
    import { MenuHost, SharedMenuItem } from "../../../ui/common/context_menu.slint";
    export { Theme, MenuHost, SharedMenuItem }
    export component EditingProbe inherits Window {
        width: 480px; height: 320px;
        in-out property <string> text <=> edit.text;
        in-out property <bool> edit-enabled <=> edit.enabled;
        in-out property <bool> read-only <=> edit.read-only;
        in property <bool> password;
        in property <bool> right-aligned;
        out property <bool> menu-open: edit.context-menu-open;
        out property <bool> focused: edit.has-focus;
        out property <int> edited-count;
        out property <int> accepted-count;
        public function select-all() { edit.select-all(); }
        public function clear-selection() { edit.clear-selection(); }
        public function select(start: int, end: int) { edit.set-selection-offsets(start, end); }
        public function cut() { edit.cut(); }
        public function copy() { edit.copy(); }
        public function paste() { edit.paste(); }
        public function focus-edit() { edit.focus(); }
        edit := LineEdit {
            x: 24px; y: 24px; width: 360px; height: 36px;
            input-type: root.password ? InputType.password : InputType.text;
            horizontal-alignment: root.right-aligned ? TextHorizontalAlignment.right : TextHorizontalAlignment.left;
            placeholder-text: "Search";
            accessible-label: "Editing field";
            edited(value) => { root.edited-count += 1; }
            accepted(value) => { root.accepted-count += 1; }
        }
    }
}

struct PrivatePlatform {
    surface: Rc<MinimalSoftwareWindow>,
    clipboard: Rc<RefCell<Option<String>>>,
}
impl Platform for PrivatePlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.surface.clone())
    }
    fn set_clipboard_text(&self, text: &str, clipboard: Clipboard) {
        if clipboard == Clipboard::DefaultClipboard {
            *self.clipboard.borrow_mut() = Some(text.into());
        }
    }
    fn clipboard_text(&self, clipboard: Clipboard) -> Option<String> {
        (clipboard == Clipboard::DefaultClipboard)
            .then(|| self.clipboard.borrow().clone())
            .flatten()
    }
}

fn setup() -> (
    EditingProbe,
    Rc<MinimalSoftwareWindow>,
    Rc<RefCell<Option<String>>>,
) {
    let surface = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    let clipboard = Rc::new(RefCell::new(None));
    slint::platform::set_platform(Box::new(PrivatePlatform {
        surface: surface.clone(),
        clipboard: clipboard.clone(),
    }))
    .unwrap();
    let ui = EditingProbe::new().unwrap();
    let theme = ui.global::<Theme>();
    theme.set_menu_action_size(36.);
    theme.set_control_surface(slint::Color::from_rgb_u8(20, 30, 40));
    theme.set_control_border(slint::Color::from_rgb_u8(80, 90, 100));
    theme.set_menu_bg(slint::Color::from_rgb_u8(24, 32, 48));
    theme.set_surface_hover(slint::Color::from_rgb_u8(80, 100, 120));
    theme.set_font_family("DejaVu Sans".into());
    theme.set_context_menu_width(294.);
    ui.show().unwrap();
    surface.set_size(slint::PhysicalSize::new(480, 320));

    slint::platform::update_timers_and_animations();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 480 * 320];
    surface.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 480);
    });
    (ui, surface, clipboard)
}

fn click(ui: &EditingProbe, x: f32, y: f32, button: PointerEventButton) {
    let position = LogicalPosition::new(x, y);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window()
        .dispatch_event(WindowEvent::PointerPressed { position, button });
    ui.window()
        .dispatch_event(WindowEvent::PointerReleased { position, button });
}
fn key(ui: &EditingProbe, text: &str) {
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.into() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text: text.into() });
}
fn control_key(ui: &EditingProbe, text: &str) {
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: "\u{11}".into(),
    });
    key(ui, text);
    ui.window().dispatch_event(WindowEvent::KeyReleased {
        text: "\u{11}".into(),
    });
}

#[test]
fn text_selection_and_menu_clipboard_actions_preserve_focus_and_edit_notifications() {
    let (ui, _, clipboard) = setup();
    ui.set_text("alpha beta gamma".into());
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position: LogicalPosition::new(37., 40.),
        button: PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(115., 40.),
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(115., 40.),
        button: PointerEventButton::Left,
    });
    ui.invoke_copy();
    let dragged = clipboard.borrow().clone().unwrap();
    assert!(
        !dragged.is_empty() && dragged != "alpha beta gamma",
        "Pointer dragging must keep TextInput selection"
    );

    ui.invoke_select(0, 5);
    click(&ui, 70., 40., PointerEventButton::Right);
    assert!(ui.get_menu_open());
    click(&ui, 120., 95., PointerEventButton::Left);
    assert!(!ui.get_menu_open());
    assert!(ui.get_focused());
    assert_eq!(clipboard.borrow().as_deref(), Some("alpha"));
    assert_eq!(ui.get_edited_count(), 0);

    ui.invoke_select(0, 5);
    click(&ui, 70., 40., PointerEventButton::Right);
    click(&ui, 120., 60., PointerEventButton::Left);
    assert_eq!(ui.get_text(), " beta gamma");
    assert!(ui.get_focused() && !ui.get_menu_open());
    assert_eq!(ui.get_edited_count(), 1);
    click(&ui, 70., 40., PointerEventButton::Right);
    click(&ui, 120., 130., PointerEventButton::Left);
    assert_eq!(ui.get_text(), "alpha beta gamma");
    assert_eq!(ui.get_edited_count(), 2);
    assert!(ui.get_focused() && !ui.get_menu_open());
    key(&ui, "\n");
    assert_eq!(ui.get_accepted_count(), 1);
}

#[test]
fn menu_escape_click_away_and_protected_read_only_actions_are_guarded() {
    let (ui, _, clipboard) = setup();
    ui.set_text("secret".into());
    let accessible_edit = ElementHandle::find_by_accessible_label(&ui, "Editing field")
        .find(|element| element.accessible_value().is_some())
        .expect("The shared LineEdit exposes its accessible editing control");
    assert_eq!(
        accessible_edit.accessible_value().as_deref(),
        Some("secret")
    );
    accessible_edit.set_accessible_value("ordinary text");
    assert_eq!(ui.get_text(), "ordinary text");
    assert_eq!(
        accessible_edit.accessible_value().as_deref(),
        Some("ordinary text")
    );
    assert_eq!(ui.get_edited_count(), 1);
    *clipboard.borrow_mut() = Some("private clipboard".into());
    ui.set_password(true);
    assert_eq!(accessible_edit.accessible_value().unwrap_or_default(), "");
    accessible_edit.set_accessible_value("secret");
    assert_eq!(ui.get_text(), "secret");
    assert_eq!(accessible_edit.accessible_value().unwrap_or_default(), "");
    assert_eq!(ui.get_edited_count(), 2);
    ui.invoke_select_all();
    ui.invoke_copy();
    ui.invoke_cut();
    ui.invoke_focus_edit();
    control_key(&ui, "c");
    control_key(&ui, "x");
    assert_eq!(clipboard.borrow().as_deref(), Some("private clipboard"));
    assert_eq!(ui.get_text(), "secret");
    click(&ui, 70., 40., PointerEventButton::Right);
    assert!(ui.get_menu_open());
    key(&ui, "\u{1b}");
    assert!(!ui.get_menu_open() && ui.get_focused());
    click(&ui, 70., 40., PointerEventButton::Right);
    click(&ui, 450., 280., PointerEventButton::Left);
    assert!(
        !ui.get_menu_open() && ui.get_focused(),
        "Click-away must restore the editing focus"
    );

    ui.set_password(false);
    assert_eq!(
        accessible_edit.accessible_value().as_deref(),
        Some("secret")
    );
    ui.set_read_only(true);
    accessible_edit.set_accessible_value("forbidden read-only edit");
    ui.invoke_select_all();
    ui.invoke_cut();
    ui.invoke_paste();
    assert_eq!(ui.get_text(), "secret");
    ui.invoke_copy();
    assert_eq!(clipboard.borrow().as_deref(), Some("secret"));
    assert_eq!(ui.get_edited_count(), 2);
    ui.set_edit_enabled(false);
    ui.set_read_only(false);
    accessible_edit.set_accessible_value("forbidden disabled edit");
    assert_eq!(ui.get_text(), "secret");
    click(&ui, 70., 40., PointerEventButton::Right);
    assert!(!ui.get_menu_open());
}

#[test]
fn actions_without_selection_stay_disabled_and_keyboard_skips_them() {
    let (ui, _, clipboard) = setup();
    ui.set_text("sample".into());
    *clipboard.borrow_mut() = Some("kept".into());
    ui.invoke_clear_selection();
    click(&ui, 70., 40., PointerEventButton::Right);
    click(&ui, 120., 60., PointerEventButton::Left);
    assert!(ui.get_menu_open());
    assert_eq!(ui.get_text(), "sample");
    click(&ui, 120., 95., PointerEventButton::Left);
    assert!(ui.get_menu_open());
    assert_eq!(clipboard.borrow().as_deref(), Some("kept"));
    key(&ui, "\u{1b}");

    ui.set_read_only(true);
    click(&ui, 70., 40., PointerEventButton::Right);
    key(&ui, "\u{f729}");
    key(&ui, "\n");
    assert!(!ui.get_menu_open());
    click(&ui, 70., 40., PointerEventButton::Right);
    key(&ui, "\u{f729}");
    key(&ui, "\n");
    assert!(!ui.get_menu_open() && ui.get_focused());
    assert_eq!(clipboard.borrow().as_deref(), Some("sample"));
    assert_eq!(ui.get_edited_count(), 0);
}

#[test]
fn open_editing_menu_uses_shared_surface_palette_and_updates_with_the_theme() {
    let (ui, surface, _) = setup();
    ui.set_text("selected text".into());
    click(&ui, 70., 40., PointerEventButton::Right);
    let pixel = |x: usize, y: usize| {
        let mut pixels = vec![slint::Rgb8Pixel::default(); 480 * 320];
        ui.window().request_redraw();
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 480);
        });
        let p = pixels[y * 480 + x];
        (p.r, p.g, p.b)
    };
    assert_eq!(pixel(330, 175), (24, 32, 48));
    ui.global::<Theme>()
        .set_menu_bg(slint::Color::from_rgb_u8(180, 200, 220));
    assert_eq!(
        pixel(330, 175),
        (180, 200, 220),
        "An already open editing menu must repaint the shared theme"
    );
}

#[test]
fn controller_replacements_keep_focus_reset_scrolling_and_preserve_alignment() {
    let (ui, surface, _) = setup();
    let long = "long text extending beyond the input viewport ".repeat(6);
    ui.set_text(long.clone().into());
    ui.invoke_focus_edit();
    ui.invoke_select(long.len() as i32, long.len() as i32);
    let render = || {
        let mut pixels = vec![slint::Rgb8Pixel::default(); 480 * 320];
        ui.window().request_redraw();
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 480);
        });
        pixels
    };
    render();
    ui.set_text("W".into());
    let left = render();
    let has_glyph = |pixels: &[slint::Rgb8Pixel], start: usize, end: usize| {
        (30..53).any(|y| {
            (start..end).any(|x| {
                let p = pixels[y * 480 + x];
                p.r > 200 && p.g > 200 && p.b > 200
            })
        })
    };
    assert!(
        has_glyph(&left, 36, 65),
        "Replacing long text must show the beginning of the new text"
    );
    assert!(ui.get_focused());
    assert_eq!(
        ui.get_edited_count(),
        0,
        "Controller changes must not emit edited callbacks"
    );
    ui.set_right_aligned(true);
    let right = render();
    assert!(!has_glyph(&right, 36, 65));
    assert!(
        has_glyph(&right, 346, 373),
        "The LineEdit API must preserve horizontal alignment"
    );
}

#[test]
fn editing_shortcuts_and_keyboard_context_menu_use_the_same_selection() {
    let (ui, _, clipboard) = setup();
    ui.set_text("keyboard".into());
    ui.invoke_focus_edit();
    control_key(&ui, "a");
    control_key(&ui, "c");
    assert_eq!(clipboard.borrow().as_deref(), Some("keyboard"));
    control_key(&ui, "x");
    assert_eq!(ui.get_text(), "");
    control_key(&ui, "v");
    assert_eq!(ui.get_text(), "keyboard");
    control_key(&ui, "z");
    assert_eq!(ui.get_text(), "");
    ui.set_text("context".into());
    control_key(&ui, "a");
    key(&ui, "\u{f735}");
    assert!(ui.get_menu_open());
    key(&ui, "\u{f729}");
    key(&ui, "\n");
    assert!(!ui.get_menu_open() && ui.get_focused());
    assert_eq!(ui.get_text(), "");
    assert_eq!(clipboard.borrow().as_deref(), Some("context"));
}

#[test]
fn native_editing_menu_uses_absolute_anchor_owned_feedback_and_safe_fallback() {
    let (ui, _, clipboard) = setup();
    ui.set_text("selected".into());
    ui.invoke_select_all();
    ui.global::<MenuHost>().on_register_owner(|| 51);
    let requests = Rc::new(RefCell::new(Vec::new()));
    let recorded = requests.clone();
    ui.global::<MenuHost>()
        .on_request(move |owner, items, x, y, width, height, title| {
            recorded
                .borrow_mut()
                .push((owner, items, x, y, width, height, title));
            true
        });
    click(&ui, 120., 40., PointerEventButton::Right);
    slint::platform::update_timers_and_animations();
    assert!(ui.get_menu_open());
    let request = requests.borrow()[0].clone();
    assert_eq!((request.0, request.2, request.3), (51, 120., 40.));
    assert!(request.4 > 0. && request.5 > 0.);
    assert_eq!(request.6, "Search");
    assert_eq!(request.1.row_count(), 4);
    let cut = request.1.row_data(0).unwrap();
    assert_eq!(cut.action, "cut");
    assert!(cut.enabled);
    let host = ui.global::<MenuHost>();
    host.set_event_owner(9);
    host.set_event_action("cut".into());
    host.set_event_generation(1);
    slint::platform::update_timers_and_animations();
    assert!(ui.get_menu_open());
    assert_eq!(ui.get_text(), "selected");
    host.set_event_owner(51);
    host.set_event_generation(2);
    slint::platform::update_timers_and_animations();
    assert!(!ui.get_menu_open() && ui.get_focused());
    assert_eq!(ui.get_text(), "");
    assert_eq!(clipboard.borrow().as_deref(), Some("selected"));

    click(&ui, 120., 40., PointerEventButton::Right);
    host.set_event_action("".into());
    host.set_event_unavailable(true);
    host.set_event_generation(3);
    slint::platform::update_timers_and_animations();
    assert!(
        ui.get_menu_open(),
        "Native failure must retain an editing menu"
    );
    key(&ui, "\u{f729}");
    key(&ui, "\n");
    assert!(!ui.get_menu_open() && ui.get_focused());
    assert_eq!(ui.get_text(), "selected");

    ui.set_password(true);
    ui.invoke_select_all();
    click(&ui, 120., 40., PointerEventButton::Right);
    let request = requests.borrow().last().unwrap().clone();
    assert!(!request.1.row_data(0).unwrap().enabled);
    assert!(!request.1.row_data(1).unwrap().enabled);
    host.set_event_unavailable(false);
    host.set_event_action("cut".into());
    host.set_event_generation(4);
    slint::platform::update_timers_and_animations();
    assert_eq!(
        ui.get_text(),
        "selected",
        "Protected fields recheck the action"
    );
    assert!(!ui.get_menu_open());
}
