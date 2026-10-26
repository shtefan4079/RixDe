use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, PointerEventButton, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, Model, ModelRc, VecModel};
use std::{cell::RefCell, rc::Rc};

slint::slint! {
    import { PickerSelect, PickerOption, FilePicker } from "../ui/picker.slint";
    import { MenuHost, SharedMenuItem } from "../../../ui/common/context_menu.slint";
    import { Theme } from "../../../ui/common/theme.slint";
    export { PickerOption, FilePicker, MenuHost, SharedMenuItem, Theme }
    export component PickerProbe inherits Window {
        width: 480px; height: 480px;
        in-out property <[PickerOption]> options;
        in-out property <int> selected-index;
        in-out property <length> selector-y: 24px;
        out property <bool> menu-open: selector.popup-open;
        out property <int> notification-count;
        public function focus-selector() { selector.focus(); }
        selector := PickerSelect {
            x: 24px; y: root.selector-y; width: 300px; height: 36px;
            options: root.options; selected: root.selected-index;
            chosen(index) => { root.selected-index = index; root.notification-count += 1; }
        }
    }
}

struct PrivatePlatform(Rc<MinimalSoftwareWindow>);
impl Platform for PrivatePlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}
fn options(count: usize) -> ModelRc<PickerOption> {
    Rc::new(VecModel::from(
        (0..count)
            .map(|index| PickerOption {
                label: format!("Item {index}").into(),
                value: format!("value-{index}").into(),
            })
            .collect::<Vec<_>>(),
    ))
    .into()
}
fn setup() -> (PickerProbe, Rc<MinimalSoftwareWindow>) {
    let surface = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(PrivatePlatform(surface.clone()))).unwrap();
    let ui = PickerProbe::new().unwrap();
    let theme = ui.global::<Theme>();
    theme.set_menu_action_size(36.);
    theme.set_menu_row_height(34.);
    theme.set_context_menu_padding(4.);
    theme.set_context_menu_max_height(280.);
    theme.set_menu_bg(slint::Color::from_rgb_u8(24, 32, 48));
    theme.set_control_surface(slint::Color::from_rgb_u8(20, 30, 40));
    theme.set_font_family("DejaVu Sans".into());
    theme.set_scrollbar_gutter(12.);
    theme.set_scrollbar_min_thumb(24.);
    ui.set_options(options(30));
    ui.show().unwrap();
    surface.set_size(slint::PhysicalSize::new(480, 480));
    render(&ui, &surface);
    (ui, surface)
}
fn render(ui: &PickerProbe, surface: &MinimalSoftwareWindow) {
    slint::platform::update_timers_and_animations();
    ui.window().request_redraw();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 480 * 480];
    surface.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 480);
    });
}
fn key(ui: &PickerProbe, text: &str) {
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.into() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text: text.into() });
}
fn click(ui: &PickerProbe, x: f32, y: f32) {
    let position = LogicalPosition::new(x, y);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
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
fn long_picker_menu_reveals_targets_and_cancel_reopen_restores_the_committed_choice() {
    let (ui, surface) = setup();
    ui.set_selector_y(400.);
    render(&ui, &surface);
    click(&ui, 80., 416.);
    key(&ui, "\u{f72b}");
    render(&ui, &surface);
    assert_eq!(ui.get_selected_index(), 0);
    click(&ui, 80., 459.);
    assert!(!ui.get_menu_open());
    assert_eq!(ui.get_selected_index(), 29);
    assert_eq!(ui.get_notification_count(), 1);
    click(&ui, 80., 416.);
    assert!(ui.get_menu_open());
    key(&ui, "\u{f729}");
    key(&ui, "\u{1b}");
    assert!(!ui.get_menu_open(), "Escape must close the preview menu");
    assert_eq!(ui.get_selected_index(), 29);
    assert_eq!(ui.get_notification_count(), 1);
    click(&ui, 80., 416.);
    assert!(ui.get_menu_open(), "Click must reopen the canceled menu");
    render(&ui, &surface);
    key(&ui, "\n");
    assert!(!ui.get_menu_open());
    assert_eq!(
        ui.get_selected_index(),
        29,
        "Reopening resets a canceled preview cursor"
    );
    assert_eq!(ui.get_notification_count(), 2);
    click(&ui, 80., 416.);
    render(&ui, &surface);

    ui.window().dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(80., 420.),
    });
    key(&ui, "\n");
    assert_eq!(
        ui.get_selected_index(),
        28,
        "Actual pointer movement can preview and commit another visible row"
    );
    assert_eq!(ui.get_notification_count(), 3);
    ui.set_options(options(0));
    render(&ui, &surface);
    click(&ui, 80., 416.);
    key(&ui, "\n");
    assert!(
        !ui.get_menu_open(),
        "An empty picker does not open an empty menu"
    );
    assert_eq!(ui.get_notification_count(), 3);
}

#[test]
fn native_picker_request_preserves_model_indices_and_routes_only_the_matching_owner() {
    let (ui, surface) = setup();
    ui.global::<MenuHost>().on_register_owner(|| 64);
    ui.global::<FilePicker>()
        .on_menu_items(|options, selected| {
            Rc::new(VecModel::from(
                (0..options.row_count())
                    .filter_map(|index| {
                        options.row_data(index).map(|option| SharedMenuItem {
                            action: index.to_string().into(),
                            label: option.label,
                            enabled: true,
                            checked: index as i32 == selected,
                            ..Default::default()
                        })
                    })
                    .collect::<Vec<_>>(),
            ))
            .into()
        });
    let requests = Rc::new(RefCell::new(Vec::new()));
    let recorded = requests.clone();
    ui.global::<MenuHost>()
        .on_request(move |owner, items, x, y, width, height, title| {
            recorded
                .borrow_mut()
                .push((owner, items, x, y, width, height, title));
            true
        });
    click(&ui, 80., 40.);
    render(&ui, &surface);
    let request = requests.borrow()[0].clone();
    assert_eq!(
        (request.0, request.2, request.3, request.4),
        (64, 24., 60., 300.)
    );
    assert_eq!(request.1.row_count(), 30);
    assert_eq!(request.1.row_data(29).unwrap().action, "29");
    assert!(request.1.row_data(0).unwrap().checked);
    let host = ui.global::<MenuHost>();
    host.set_event_owner(10);
    host.set_event_action("29".into());
    host.set_event_generation(1);
    render(&ui, &surface);
    assert!(ui.get_menu_open());
    assert_eq!(ui.get_notification_count(), 0);
    host.set_event_owner(64);
    host.set_event_generation(2);
    render(&ui, &surface);
    assert!(!ui.get_menu_open());
    assert_eq!(ui.get_selected_index(), 29);
    assert_eq!(ui.get_notification_count(), 1);
    click(&ui, 80., 40.);
    host.set_event_action("300".into());
    host.set_event_generation(3);
    render(&ui, &surface);
    assert_eq!(
        ui.get_selected_index(),
        29,
        "Out-of-range feedback cannot choose a row"
    );
    assert_eq!(ui.get_notification_count(), 1);
    click(&ui, 80., 40.);
    host.set_event_action("".into());
    host.set_event_unavailable(true);
    host.set_event_generation(4);
    render(&ui, &surface);
    assert!(ui.get_menu_open());
    key(&ui, "\u{f729}");
    key(&ui, "\n");
    assert!(!ui.get_menu_open());
    assert_eq!(ui.get_selected_index(), 0);
    assert_eq!(ui.get_notification_count(), 2);
}
