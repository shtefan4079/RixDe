use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, PointerEventButton, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, Model, ModelRc, SharedString, VecModel};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

slint::slint! {
    import { ComboBox } from "../../../ui/common/combo_box.slint";
    import { Theme } from "../../../ui/common/theme.slint";
    import { MenuHost, SharedMenuItem } from "../../../ui/common/context_menu.slint";
    export { Theme, MenuHost, SharedMenuItem }
    export component ComboProbe inherits Window {
        width: 480px; height: 480px;
        background: #050607;
        in-out property <[string]> choices <=> selector.model;
        in-out property <int> selected-index <=> selector.current-index;
        in-out property <string> selected-value <=> selector.current-value;
        in-out property <bool> selector-enabled <=> selector.enabled;
        in-out property <length> selector-y: 24px;
        out property <bool> menu-open: selector.popup-open;
        out property <bool> selector-focused: selector.has-focus;
        out property <int> notification-count;
        out property <int> notified-index: -1;
        out property <string> notified-value;
        public function focus-selector() { selector.focus(); }
        selector := ComboBox {
            x: 24px; y: root.selector-y; width: 300px; height: 36px;
            selected(value) => {
                root.notification-count += 1;
                root.notified-index = self.current-index;
                root.notified-value = value;
            }
        }
    }
}

struct PrivatePlatform(Rc<MinimalSoftwareWindow>);
impl Platform for PrivatePlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}

fn choices(labels: &[&str]) -> ModelRc<SharedString> {
    Rc::new(VecModel::from(
        labels
            .iter()
            .map(|label| (*label).into())
            .collect::<Vec<_>>(),
    ))
    .into()
}

fn setup() -> (ComboProbe, Rc<MinimalSoftwareWindow>) {
    let surface = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(PrivatePlatform(surface.clone()))).unwrap();
    let ui = ComboProbe::new().unwrap();
    let theme = ui.global::<Theme>();
    theme.set_is_aero(false);
    theme.set_menu_action_size(36.);
    theme.set_menu_row_height(34.);
    theme.set_control_surface(slint::Color::from_rgb_u8(20, 30, 40));
    theme.set_control_border(slint::Color::from_rgb_u8(80, 90, 100));
    theme.set_menu_bg(slint::Color::from_rgb_u8(24, 32, 48));
    theme.set_surface_hover(slint::Color::from_rgb_u8(80, 100, 120));
    theme.set_font_family("DejaVu Sans".into());
    theme.set_scrollbar_gutter(12.);
    theme.set_scrollbar_width(4.);
    theme.set_scrollbar_hover_width(6.);
    theme.set_scrollbar_min_thumb(24.);
    theme.set_scrollbar(slint::Color::from_rgb_u8(110, 120, 130));
    ui.set_choices(choices(&["Alpha", "Beta", "Gamma"]));
    ui.show().unwrap();
    surface.set_size(slint::PhysicalSize::new(480, 480));
    render(&ui, &surface);
    (ui, surface)
}

fn render(ui: &ComboProbe, surface: &MinimalSoftwareWindow) -> Vec<slint::Rgb8Pixel> {
    slint::platform::update_timers_and_animations();
    ui.window().request_redraw();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 480 * 480];
    surface.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 480);
    });
    pixels
}

fn key(ui: &ComboProbe, text: &str) {
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.into() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text: text.into() });
}

fn move_pointer(ui: &ComboProbe, x: f32, y: f32) {
    ui.window().dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(x, y),
    });
}

fn click(ui: &ComboProbe, x: f32, y: f32) {
    move_pointer(ui, x, y);
    let position = LogicalPosition::new(x, y);
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
}

#[test]
fn closed_keyboard_selection_commits_but_popup_navigation_can_be_canceled() {
    let (ui, surface) = setup();
    ui.invoke_focus_selector();
    key(&ui, "\u{f701}");
    assert_eq!(ui.get_selected_index(), 1);
    assert_eq!(ui.get_selected_value(), "Beta");
    assert_eq!(ui.get_notified_index(), 1);
    assert_eq!(ui.get_notified_value(), "Beta");
    assert_eq!(ui.get_notification_count(), 1);

    key(&ui, "\n");
    assert!(ui.get_menu_open());
    key(&ui, "\u{f72b}");
    assert_eq!(ui.get_selected_index(), 1);
    assert_eq!(ui.get_notification_count(), 1);
    key(&ui, "\u{1b}");
    assert!(!ui.get_menu_open() && ui.get_selector_focused());
    assert_eq!(ui.get_selected_value(), "Beta");

    key(&ui, " ");
    render(&ui, &surface);
    key(&ui, "\u{f72b}");
    key(&ui, "\n");
    assert!(!ui.get_menu_open() && ui.get_selector_focused());
    assert_eq!(ui.get_selected_index(), 2);
    assert_eq!(ui.get_notified_value(), "Gamma");
    assert_eq!(ui.get_notification_count(), 2);

    key(&ui, "\u{f729}");
    assert_eq!(ui.get_selected_index(), 0);
    assert_eq!(ui.get_notified_value(), "Alpha");
}

#[test]
fn popup_hover_and_arrow_navigation_share_a_cursor_and_commit_once() {
    let (ui, surface) = setup();
    click(&ui, 80., 40.);
    render(&ui, &surface);
    assert!(ui.get_menu_open());
    move_pointer(&ui, 80., 114.);
    assert_eq!(ui.get_selected_index(), 0);
    assert_eq!(ui.get_notification_count(), 0);
    key(&ui, "\u{f701}");
    assert_eq!(ui.get_selected_index(), 0);
    key(&ui, "\n");
    assert!(!ui.get_menu_open());
    assert_eq!(ui.get_selected_index(), 2);
    assert_eq!(ui.get_notified_value(), "Gamma");
    assert_eq!(ui.get_notification_count(), 1);

    click(&ui, 80., 40.);
    render(&ui, &surface);
    key(&ui, "\u{f729}");
    click(&ui, 450., 440.);
    assert!(!ui.get_menu_open());
    assert_eq!(ui.get_selected_index(), 2);
    assert_eq!(ui.get_notification_count(), 1, "Click-away cancels preview");
}

#[test]
fn replacing_models_preserves_no_selection_clamps_existing_selection_and_disables_empty_popup() {
    let (ui, surface) = setup();
    ui.set_selected_index(-1);
    ui.set_choices(choices(&["One", "Two"]));
    render(&ui, &surface);
    assert_eq!(ui.get_selected_index(), -1);
    assert_eq!(ui.get_selected_value(), "");
    ui.set_selected_index(1);
    render(&ui, &surface);
    assert_eq!(ui.get_selected_value(), "Two");
    click(&ui, 80., 40.);
    assert!(ui.get_menu_open());
    ui.set_choices(choices(&["Only"]));
    render(&ui, &surface);
    assert_eq!(ui.get_selected_index(), 0);
    assert_eq!(ui.get_selected_value(), "Only");
    assert_eq!(ui.get_notification_count(), 0);
    ui.set_choices(choices(&[]));
    render(&ui, &surface);
    assert_eq!(ui.get_selected_index(), -1);
    assert_eq!(ui.get_selected_value(), "");
    assert!(!ui.get_menu_open());
    click(&ui, 80., 40.);
    key(&ui, "\n");
    assert!(!ui.get_menu_open());

    ui.set_choices(choices(&["One", "Two"]));
    ui.set_selected_index(0);
    render(&ui, &surface);
    click(&ui, 80., 40.);
    assert!(ui.get_menu_open());
    ui.set_selector_enabled(false);
    render(&ui, &surface);
    assert!(!ui.get_menu_open());
    click(&ui, 80., 40.);
    key(&ui, "\u{f701}");
    key(&ui, "\n");
    assert_eq!(ui.get_selected_index(), 0);
    assert_eq!(ui.get_notification_count(), 0);
    assert!(!ui.get_menu_open());
}

#[test]
fn an_existing_model_updates_the_selected_label_and_clamps_after_row_removal() {
    let (ui, surface) = setup();
    let model = Rc::new(VecModel::from(vec![
        SharedString::from("One"),
        SharedString::from("Two"),
        SharedString::from("Three"),
    ]));
    ui.set_choices(ModelRc::from(model.clone()));
    render(&ui, &surface);
    ui.invoke_focus_selector();
    key(&ui, "\u{f72b}");
    assert_eq!(ui.get_selected_value(), "Three");
    assert_eq!(ui.get_notification_count(), 1);
    model.set_row_data(2, "Renamed".into());
    render(&ui, &surface);
    assert_eq!(ui.get_selected_value(), "Renamed");
    model.remove(2);
    render(&ui, &surface);
    assert_eq!(ui.get_selected_index(), 1);
    assert_eq!(ui.get_selected_value(), "Two");
    model.set_vec(Vec::<SharedString>::new());
    render(&ui, &surface);
    assert_eq!(ui.get_selected_index(), -1);
    assert_eq!(ui.get_selected_value(), "");
    assert_eq!(
        ui.get_notification_count(),
        1,
        "Model edits are not user choices"
    );
}

#[test]
fn a_long_popup_scrolls_keyboard_targets_into_view_and_stays_inside_the_bottom_edge() {
    let (ui, surface) = setup();
    ui.set_choices(
        Rc::new(VecModel::from(
            (0..30)
                .map(|index| format!("Item {index}").into())
                .collect::<Vec<SharedString>>(),
        ))
        .into(),
    );
    ui.set_selector_y(400.);
    render(&ui, &surface);
    click(&ui, 80., 416.);
    key(&ui, "\u{f72b}");
    render(&ui, &surface);
    assert_eq!(ui.get_selected_index(), 0, "End only previews");

    click(&ui, 80., 459.);
    assert!(!ui.get_menu_open());
    assert_eq!(ui.get_notified_index(), 29);
    assert_eq!(ui.get_notified_value(), "Item 29");
    assert_eq!(ui.get_notification_count(), 1);
}

#[test]
fn an_open_selector_menu_and_its_preview_repaint_the_shared_theme() {
    let (ui, surface) = setup();
    let pixel = |pixels: &[slint::Rgb8Pixel], x: usize, y: usize| {
        let p = pixels[y * 480 + x];
        (p.r, p.g, p.b)
    };
    let closed = render(&ui, &surface);
    assert_eq!(pixel(&closed, 270, 42), (20, 30, 40));
    click(&ui, 80., 40.);
    let pixels = render(&ui, &surface);
    assert_eq!(pixel(&pixels, 321, 110), (24, 32, 48));
    assert_eq!(pixel(&pixels, 300, 80), (80, 100, 120));
    ui.global::<Theme>()
        .set_menu_bg(slint::Color::from_rgb_u8(180, 200, 220));
    ui.global::<Theme>()
        .set_surface_hover(slint::Color::from_rgb_u8(60, 90, 130));
    let changed = render(&ui, &surface);
    assert_eq!(pixel(&changed, 321, 110), (180, 200, 220));
    assert_eq!(pixel(&changed, 300, 80), (60, 90, 130));
    key(&ui, "\u{f701}");
    let moved = render(&ui, &surface);
    assert_eq!(pixel(&moved, 300, 114), (60, 90, 130));
    assert_eq!(pixel(&moved, 300, 80), (180, 200, 220));
    assert_eq!(ui.get_notification_count(), 0);
}

#[test]
fn native_selector_routes_owned_results_and_preserves_inline_fallback_on_decline_or_failure() {
    let (ui, surface) = setup();
    ui.global::<MenuHost>().on_register_owner(|| 42);
    ui.global::<MenuHost>()
        .on_options_items(|labels, selected| {
            let rows = (0..labels.row_count())
                .filter_map(|index| {
                    labels.row_data(index).map(|label| SharedMenuItem {
                        action: index.to_string().into(),
                        label,
                        enabled: true,
                        checked: index as i32 == selected,
                        ..Default::default()
                    })
                })
                .collect::<Vec<_>>();
            Rc::new(VecModel::from(rows)).into()
        });
    let requests = Rc::new(RefCell::new(Vec::new()));
    let recorded = requests.clone();
    let accept_native = Rc::new(Cell::new(true));
    let accepted = accept_native.clone();
    ui.global::<MenuHost>()
        .on_request(move |owner, items, x, y, width, height, title| {
            recorded
                .borrow_mut()
                .push((owner, items, x, y, width, height, title));
            accepted.get()
        });
    let closes = Rc::new(Cell::new(0));
    let counted = closes.clone();
    ui.global::<MenuHost>().on_close(move |owner| {
        assert_eq!(owner, 42);
        counted.set(counted.get() + 1);
    });
    click(&ui, 80., 40.);
    render(&ui, &surface);
    assert!(ui.get_menu_open());
    let request = requests.borrow()[0].clone();
    assert_eq!(
        (request.0, request.2, request.3, request.4),
        (42, 24., 60., 300.)
    );
    assert!(request.5 > 0.);
    assert_eq!(request.6, "Alpha");
    assert_eq!(request.1.row_count(), 3);
    assert_eq!(request.1.row_data(2).unwrap().action, "2");
    assert!(request.1.row_data(0).unwrap().checked);

    let host = ui.global::<MenuHost>();
    host.set_event_owner(7);
    host.set_event_action("1".into());
    host.set_event_generation(1);
    render(&ui, &surface);
    assert!(ui.get_menu_open());
    assert_eq!(ui.get_notification_count(), 0);
    host.set_event_owner(42);
    host.set_event_action("2".into());
    host.set_event_generation(2);
    render(&ui, &surface);
    assert!(!ui.get_menu_open() && ui.get_selector_focused());
    assert_eq!(ui.get_notified_index(), 2);
    assert_eq!(ui.get_notification_count(), 1);
    assert_eq!(
        closes.get(),
        0,
        "A result has already closed its native popup"
    );

    click(&ui, 80., 40.);
    host.set_event_action("".into());
    host.set_event_unavailable(true);
    host.set_event_generation(3);
    render(&ui, &surface);
    assert!(
        ui.get_menu_open(),
        "A renderer failure opens the embedded fallback"
    );
    key(&ui, "\u{f729}");
    key(&ui, "\n");
    assert_eq!(ui.get_notified_index(), 0);
    assert_eq!(ui.get_notification_count(), 2);
    assert!(!ui.get_menu_open());

    accept_native.set(false);
    click(&ui, 80., 40.);
    render(&ui, &surface);
    assert!(
        ui.get_menu_open(),
        "A declined host request retains the themed fallback"
    );
    key(&ui, "\u{1b}");
    assert!(!ui.get_menu_open() && ui.get_selector_focused());
    assert_eq!(ui.get_notification_count(), 2);
}
