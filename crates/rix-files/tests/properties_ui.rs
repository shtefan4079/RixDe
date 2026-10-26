use i_slint_backend_testing::ElementHandle;
use slint::{
    platform::{Key, PointerEventButton, WindowEvent},
    ComponentHandle, ModelRc, VecModel,
};
use std::{cell::Cell, rc::Rc};

slint::slint! {
    import { PropertiesView, PropertyDetail, PropertyPermission, PropertiesPanel } from "../ui/properties.slint";
    import { Theme } from "../../../ui/common/theme.slint";
    import { SkinFrame, SkinRenderer } from "../../../ui/common/frame.slint";
    import { Palette } from "std-widgets.slint";
    export { PropertiesView, PropertyDetail, PropertyPermission, Theme, SkinFrame, SkinRenderer, Palette }
    export component PropertiesFixture inherits Window {
        default-font-family: Theme.font-family;
        default-font-size: Theme.font-size-md;
        preferred-width: 480px; preferred-height: 560px;
        background: Theme.window-background;
        in property <bool> desktop-material;
        callback dismissed();
        public function focus-panel() { panel.focus-panel(); }
        panel := PropertiesPanel {
            x: 16px; y: 16px; width: root.width - 32px; height: root.height - 32px;
            material-supported: root.desktop-material;
            opaque: !root.desktop-material;
            dismissed => { root.dismissed(); }
        }
    }
}
#[path = "../../../ui/common/skins.rs"]
mod skins;
#[path = "../../../ui/common/bindings.rs"]
mod theme_bindings;
fn model<T: Clone + 'static>(rows: Vec<T>) -> ModelRc<T> {
    Rc::new(VecModel::from(rows)).into()
}
fn populate(w: &PropertiesFixture) {
    let ui = w.global::<PropertiesView>();
    ui.set_name("A long document title that wraps over several lines.txt".into());
    ui.set_description("Text document".into());
    ui.set_location(
        "/home/demo/Documents/A long document title that wraps over several lines.txt".into(),
    );
    ui.set_link_target("../Shared/Original document.txt".into());
    ui.set_details(model(vec![
        PropertyDetail {
            label: "Size".into(),
            value: "24.5 KiB (25088 bytes)".into(),
        },
        PropertyDetail {
            label: "Size on disk".into(),
            value: "28.0 KiB (28672 bytes)".into(),
        },
    ]));
    ui.set_dates(model(vec![PropertyDetail {
        label: "Modified".into(),
        value: "2026-10-09 10:20".into(),
    }]));
    ui.set_permissions(model(vec![
        PropertyPermission {
            label: "Owner".into(),
            read: true,
            write: true,
            execute: false,
        },
        PropertyPermission {
            label: "Group".into(),
            read: true,
            write: false,
            execute: false,
        },
    ]));
    ui.set_ownership(model(vec![PropertyDetail {
        label: "Mode".into(),
        value: "0640 · rw-r-----".into(),
    }]));
}
fn control(w: &PropertiesFixture, label: &str) -> ElementHandle {
    ElementHandle::find_by_accessible_label(w, label)
        .next()
        .unwrap_or_else(|| panic!("Missing {label}"))
}
fn press(w: &PropertiesFixture, key: Key) {
    w.window()
        .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
    w.window()
        .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
    slint::platform::update_timers_and_animations();
}

#[test]
fn properties_keep_copy_and_close_reachable_and_support_keyboard_on_small_outputs() {
    i_slint_backend_testing::init_no_event_loop();
    let w = PropertiesFixture::new().unwrap();
    let theme = rix_config::ThemeSettings::default();
    theme_bindings::apply(w.global::<Theme>(), w.global::<Palette>(), &theme);
    populate(&w);
    let copies = Rc::new(Cell::new(0));
    let result = copies.clone();
    w.global::<PropertiesView>()
        .on_copy_paths(move || result.set(result.get() + 1));
    let closes = Rc::new(Cell::new(0));
    let result = closes.clone();
    w.on_dismissed(move || result.set(result.get() + 1));
    for (width, height) in [(360., 420.), (480., 560.), (640., 720.)] {
        w.window().set_size(slint::LogicalSize::new(width, height));
        w.show().unwrap();
        slint::platform::update_timers_and_animations();
        for label in ["Copy path", "Close"] {
            let button = control(&w, label);
            let pos = button.absolute_position();
            let size = button.size();
            assert!(
                size.width > 20. && size.height > 20.,
                "{label} collapsed at {width}x{height}"
            );
            assert!(
                pos.x >= 16.
                    && pos.y >= 16.
                    && pos.x + size.width <= width - 15.
                    && pos.y + size.height <= height - 15.,
                "{label} escaped its panel at {width}x{height}"
            );
        }
        let copy = control(&w, "Copy path");
        let origin = copy.absolute_position();
        let size = copy.size();
        let position =
            slint::LogicalPosition::new(origin.x + size.width / 2., origin.y + size.height / 2.);
        let before = copies.get();
        w.window()
            .dispatch_event(WindowEvent::PointerMoved { position });
        w.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        w.window().dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
        assert_eq!(copies.get(), before + 1);
        w.invoke_focus_panel();
        let before = closes.get();
        press(&w, Key::Tab);
        press(&w, Key::Escape);
        assert_eq!(
            closes.get(),
            before + 1,
            "Escape in the read-only path must close properties"
        );
        w.invoke_focus_panel();
        press(&w, Key::Return);
        assert_eq!(
            closes.get(),
            before + 2,
            "Enter on Close must dismiss properties"
        );
        w.invoke_focus_panel();
        press(&w, Key::Backtab);
        press(&w, Key::Backtab);
        press(&w, Key::Backtab);
        press(&w, Key::Return);
        assert_eq!(
            closes.get(),
            before + 2,
            "Reverse Tab must visit the link target before the full path"
        );
        press(&w, Key::Backtab);
        press(&w, Key::Return);
        assert_eq!(closes.get(), before + 3);
    }
    w.global::<PropertiesView>().set_multiple(true);
    slint::platform::update_timers_and_animations();
    assert!(control(&w, "Copy paths").size().width > 20.);
}

#[test]
fn properties_footer_stacks_for_large_font_themes_without_clipping_actions() {
    i_slint_backend_testing::init_no_event_loop();
    let w = PropertiesFixture::new().unwrap();
    theme_bindings::apply(
        w.global::<Theme>(),
        w.global::<Palette>(),
        &rix_config::ThemeSettings::default(),
    );
    populate(&w);
    w.global::<Theme>().set_font_size_md(28.);
    w.global::<Theme>().set_font_size_lg(34.);
    w.global::<Theme>().set_font_size_sm(22.);
    w.window().set_size(slint::LogicalSize::new(360., 420.));
    w.show().unwrap();
    slint::platform::update_timers_and_animations();
    let copy = control(&w, "Copy path");
    let close = control(&w, "Close");
    for button in [&copy, &close] {
        let pos = button.absolute_position();
        let size = button.size();
        assert!(pos.x >= 16. && pos.x + size.width <= 344.);
        assert!(pos.y >= 16. && pos.y + size.height <= 404.);
        assert!(
            size.width >= 250.,
            "Stacked actions must use the available width"
        );
    }
    assert!(close.absolute_position().y >= copy.absolute_position().y + copy.size().height);
}

#[test]
fn properties_render_current_material_and_opaque_theme_with_readable_footer() {
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
    let w = PropertiesFixture::new().unwrap();
    skins::install(w.global::<SkinRenderer>(), w.as_weak());
    populate(&w);
    let output = std::env::var_os("RIX_FILES_RENDER_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("rix-files-ui"));
    std::fs::create_dir_all(&output).unwrap();
    for (name, theme, supported, width, height) in [
        (
            "properties-glass",
            rix_config::ThemeSettings::aero_glass(),
            true,
            480u32,
            560u32,
        ),
        (
            "properties-light-small",
            rix_config::ThemeSettings::crisp_light(),
            false,
            360,
            420,
        ),
    ] {
        theme_bindings::apply(w.global::<Theme>(), w.global::<Palette>(), &theme);
        w.set_desktop_material(supported);
        w.window().set_size(slint::PhysicalSize::new(width, height));
        w.show().unwrap();
        slint::platform::update_timers_and_animations();
        let mut pixels = vec![
            slint::platform::software_renderer::PremultipliedRgbaColor::default();
            (width * height) as usize
        ];
        surface.request_redraw();
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width as usize);
        });
        assert!(pixels.iter().any(|pixel| pixel.alpha > 0));
        let bytes: Vec<_> = pixels
            .iter()
            .flat_map(|pixel| [pixel.red, pixel.green, pixel.blue, pixel.alpha])
            .collect();
        image::save_buffer(
            output.join(format!("{name}.png")),
            &bytes,
            width,
            height,
            image::ColorType::Rgba8,
        )
        .unwrap();
    }
}
