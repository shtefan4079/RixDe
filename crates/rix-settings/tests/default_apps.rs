use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::{cell::Cell, rc::Rc};
slint::slint! {
    import { DefaultAppsSettings, DefaultAppGroup, DefaultAppsView } from "../ui/default_apps.slint";
    import { ApplicationChooser, ChooserApplication } from "../../../ui/common/application_chooser.slint";
    import { Theme } from "../../../ui/common/theme.slint";
    export { DefaultAppsSettings, DefaultAppGroup, ApplicationChooser, ChooserApplication, Theme }
    export { Palette } from "std-widgets.slint";
    export { SkinFrame, SkinRenderer } from "../../../ui/common/frame.slint";
    export component DefaultsProbe inherits Window {
        width: 760px; height: 690px;
        background: Theme.window-background;
        DefaultAppsView { x: 20px; y: 20px; width: parent.width - 40px; height: parent.height - 40px; }
    }
}
#[path = "../../../ui/common/skins.rs"]
mod skins;
#[path = "../../../ui/common/bindings.rs"]
mod theme_bindings;
struct PrivatePlatform(Rc<MinimalSoftwareWindow>);
impl Platform for PrivatePlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}
fn press(ui: &DefaultsProbe, value: impl Into<slint::SharedString>) {
    let text: slint::SharedString = value.into();
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text });
}
fn snapshot(surface: &MinimalSoftwareWindow, name: &str) {
    slint::platform::update_timers_and_animations();
    surface.window().request_redraw();
    let size = surface.window().size();
    let mut pixels = vec![slint::Rgb8Pixel::default(); size.width as usize * size.height as usize];
    surface.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, size.width as usize);
    });
    if let Ok(directory) = std::env::var("RIX_CHOOSER_PROBE_DIR") {
        let bytes: Vec<_> = pixels
            .iter()
            .flat_map(|pixel| [pixel.r, pixel.g, pixel.b])
            .collect();
        std::fs::create_dir_all(&directory).unwrap();
        image::save_buffer(
            std::path::Path::new(&directory).join(name),
            &bytes,
            size.width,
            size.height,
            image::ColorType::Rgb8,
        )
        .unwrap();
    }
}
#[test]
fn defaults_and_shared_chooser_keep_theme_search_keyboard_and_confirmation() {
    let surface = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(PrivatePlatform(surface.clone()))).unwrap();
    let ui = DefaultsProbe::new().unwrap();
    skins::install(ui.global::<SkinRenderer>(), ui.as_weak());
    let theme: rix_config::ThemeSettings = serde_json::from_str(include_str!(
        "../../../assets/rix/themes/fairywren-dark/theme.json"
    ))
    .unwrap();
    theme_bindings::apply(ui.global::<Theme>(), ui.global::<Palette>(), &theme);
    let groups = [
        (
            "browser",
            "Web browser",
            "Web links and HTML files",
            "RixBrowser",
        ),
        ("pdf", "PDF viewer", "PDF documents", "Document Viewer"),
        ("audio", "Audio player", "Music and audio files", "Music"),
        (
            "archives",
            "Archive manager",
            "ZIP, TAR and compressed archives",
            "Archive Manager",
        ),
    ]
    .into_iter()
    .map(|(id, label, detail, app)| DefaultAppGroup {
        id: id.into(),
        label: label.into(),
        detail: detail.into(),
        application: app.into(),
        icon: slint::Image::default(),
    })
    .collect::<Vec<_>>();
    ui.global::<DefaultAppsSettings>()
        .set_groups(ModelRc::from(Rc::new(VecModel::from(groups))));
    ui.show().unwrap();
    surface.set_size(slint::PhysicalSize::new(760, 690));
    ui.window().dispatch_event(WindowEvent::Resized {
        size: slint::LogicalSize::new(760., 690.),
    });
    snapshot(&surface, "default-applications.png");
    let chooser = ui.global::<ApplicationChooser>();
    chooser.set_title("Choose default application".into());
    chooser.set_description("PDF documents · application/pdf".into());
    chooser.set_allow_always(false);
    chooser.set_accept_label("Set as default".into());
    let rows = (0..40)
        .map(|index| ChooserApplication {
            id: format!("viewer-{index}.desktop").into(),
            label: format!("Document Viewer {index}").into(),
            detail: "Supports this file type".into(),
            icon: slint::Image::default(),
            recommended: true,
            is_default: index == 0,
        })
        .collect::<Vec<_>>();
    chooser.set_applications(ModelRc::from(Rc::new(VecModel::from(rows))));
    chooser.set_selected_index(0);
    let confirmed = Rc::new(Cell::new(0));
    let counter = confirmed.clone();
    chooser.on_confirm(move |id, always| {
        assert_eq!(id, "viewer-1.desktop");
        assert!(!always);
        counter.set(counter.get() + 1);
    });
    let cancelled = Rc::new(Cell::new(0));
    let counter = cancelled.clone();
    chooser.on_cancelled(move || counter.set(counter.get() + 1));
    ui.global::<DefaultAppsSettings>().set_chooser_visible(true);
    snapshot(&surface, "default-application-chooser.png");
    press(&ui, slint::platform::Key::DownArrow);
    assert_eq!(chooser.get_selected_index(), 1);
    press(&ui, "p");
    press(&ui, "d");
    press(&ui, "f");
    assert_eq!(chooser.get_query(), "pdf");
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: slint::platform::Key::Control.into(),
    });
    press(&ui, "a");
    ui.window().dispatch_event(WindowEvent::KeyReleased {
        text: slint::platform::Key::Control.into(),
    });
    press(&ui, "t");
    assert_eq!(
        chooser.get_query(),
        "t",
        "Ctrl+A must select search text inside the chooser"
    );
    press(&ui, slint::platform::Key::Return);
    assert_eq!(confirmed.get(), 1);
    chooser.set_busy(true);
    press(&ui, slint::platform::Key::Return);
    assert_eq!(confirmed.get(), 1);
    chooser.set_busy(false);
    chooser.set_selected_index(39);
    snapshot(&surface, "default-chooser-scrolled.png");
    press(&ui, slint::platform::Key::UpArrow);
    assert_eq!(chooser.get_selected_index(), 38);
    snapshot(&surface, "default-chooser-keyboard-scroll.png");
    press(&ui, slint::platform::Key::Escape);
    assert_eq!(cancelled.get(), 1);
    ui.hide().unwrap();
}
