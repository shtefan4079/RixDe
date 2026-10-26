use i_slint_backend_testing::ElementHandle;
use slint::{
    platform::{Key, PointerEventButton, WindowEvent},
    ComponentHandle, ModelRc, VecModel,
};
use std::{cell::Cell, rc::Rc};

slint::slint! {
    import { FilesWindow, FilePreview } from "../ui/files.slint";
    import { ArchiveBrowser, ArchiveMember, ArchivePanel } from "../ui/archive.slint";
    import { Theme } from "../../../ui/common/theme.slint";
    import { SkinFrame, SkinRenderer } from "../../../ui/common/frame.slint";
    import { Palette } from "std-widgets.slint";
    export { FilesWindow, FilePreview, ArchiveBrowser, ArchiveMember, Theme, Palette, SkinFrame, SkinRenderer }
    export component ArchiveFixture inherits Window {
        preferred-width: 1080px; preferred-height: 720px;
        default-font-family: Theme.font-family;
        default-font-size: Theme.font-size-md;
        background: Theme.window-background;
        in property <bool> desktop-material;
        public function focus-browser() { panel.focus-browser(); }
        panel := ArchivePanel { width: root.width; height: root.height; desktop-material: root.desktop-material; }
    }
}
#[path = "../../../ui/common/skins.rs"]
mod skins;
#[path = "../../../ui/common/bindings.rs"]
mod theme_bindings;

fn element(window: &ArchiveFixture, label: &str) -> ElementHandle {
    ElementHandle::find_by_accessible_label(window, label)
        .next()
        .unwrap_or_else(|| panic!("Missing {label}"))
}
fn click(window: &ArchiveFixture, label: &str) {
    let control = element(window, label);
    let origin = control.absolute_position();
    let size = control.size();
    let position =
        slint::LogicalPosition::new(origin.x + size.width / 2., origin.y + size.height / 2.);
    window
        .window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    window.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    window
        .window()
        .dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
    slint::platform::update_timers_and_animations();
}
fn key(window: &ArchiveFixture, text: slint::SharedString) {
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text });
    slint::platform::update_timers_and_animations();
}
fn populate(window: &ArchiveFixture, theme: &rix_config::ThemeSettings) {
    populate_browser(window.global::<ArchiveBrowser>(), theme);
}

fn populate_browser(browser: ArchiveBrowser<'_>, theme: &rix_config::ThemeSettings) {
    let icon = |folder: bool| {
        let source = if folder {
            include_str!("../../../ui/common/icons/folder.svg")
        } else {
            include_str!("../assets/file.svg")
        };
        slint::Image::load_from_svg_data(
            source
                .replace("#fff", &theme.appearance.color("text_primary").to_hex())
                .as_bytes(),
        )
        .unwrap()
    };
    browser.set_visible(true);
    browser.set_title("Documents and photos · autumn collection 2026.zip".into());
    browser.set_folder("Projects/RixDE".into());
    browser.set_summary("48 items · 184 MiB".into());
    browser.set_can_up(true);
    browser.set_selected_index(0);
    browser.set_members(ModelRc::from(Rc::new(VecModel::from(vec![
        ArchiveMember {
            name: "Design documents".into(),
            detail: "Folder · 12 items".into(),
            icon: icon(true),
            directory: true,
        },
        ArchiveMember {
            name: "Release notes.pdf".into(),
            detail: "PDF document · 2.4 MiB".into(),
            icon: icon(false),
            directory: false,
        },
        ArchiveMember {
            name: "Team meeting and design decisions.md".into(),
            detail: "Markdown document · 18.2 KiB".into(),
            icon: icon(false),
            directory: false,
        },
        ArchiveMember {
            name: "Document.txt".into(),
            detail: "Text document · 4.8 KiB".into(),
            icon: icon(false),
            directory: false,
        },
    ]))));
}

#[test]
fn archive_overlay_remains_visible_in_the_real_files_window() {
    i_slint_backend_testing::init_no_event_loop();
    let window = FilesWindow::new().unwrap();
    let theme = rix_config::ThemeSettings::default();
    theme_bindings::apply(window.global::<Theme>(), window.global::<Palette>(), &theme);
    populate_browser(window.global::<ArchiveBrowser>(), &theme);
    window.window().set_size(slint::PhysicalSize::new(760, 480));
    window.show().unwrap();
    slint::platform::update_timers_and_animations();
    window.invoke_focus_files();
    for label in ["Password…", "Extract…", "Close", "Release notes.pdf"] {
        let control = ElementHandle::find_by_accessible_label(&window, label).next().unwrap_or_else(|| panic!("The real FilesWindow must expose archive action {label}; an invisible ancestor must not hide its overlay"));
        let size = control.size();
        assert!(size.width > 0. && size.height > 0.);
    }
    let closed = Rc::new(Cell::new(false));
    let result = closed.clone();
    window
        .global::<ArchiveBrowser>()
        .on_close(move || result.set(true));
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Escape.into(),
    });
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Escape.into(),
    });
    assert!(
        closed.get(),
        "the real archive overlay receives focus after hiding the file canvas"
    );
}

#[test]
fn viewer_overlay_remains_visible_and_keyboard_never_reaches_background_files() {
    i_slint_backend_testing::init_no_event_loop();
    let window = FilesWindow::new().unwrap();
    let theme = rix_config::ThemeSettings::default();
    theme_bindings::apply(window.global::<Theme>(), window.global::<Palette>(), &theme);
    let preview = window.global::<FilePreview>();
    preview.set_title("readme.txt".into());
    preview.set_kind(2);
    preview.set_body("A real viewer overlay remains visible above the hidden file canvas.".into());
    preview.set_visible(true);
    preview.set_external_allowed(false);
    preview.set_note("Extract the archive to open this file in another application".into());
    window.window().set_size(slint::PhysicalSize::new(760, 480));
    window.show().unwrap();
    slint::platform::update_timers_and_animations();
    window.invoke_focus_files();
    for label in ["Close Preview", "Open With…", "Document Text", "Zoom In"] {
        let control = ElementHandle::find_by_accessible_label(&window, label)
            .next()
            .unwrap_or_else(|| panic!("The real FilesWindow must expose preview control {label}"));
        let size = control.size();
        assert!(size.width > 0. && size.height > 0.);
    }
    let external = Rc::new(Cell::new(0));
    let result = external.clone();
    preview.on_open_externally(move || result.set(result.get() + 1));
    let chooser = Rc::new(Cell::new(0));
    let result = chooser.clone();
    preview.on_open_with(move || result.set(result.get() + 1));
    for label in ["Open", "Open With…"] {
        let action = ElementHandle::find_by_accessible_label(&window, label)
            .next()
            .unwrap();
        assert_eq!(action.accessible_enabled(), Some(false));
        let origin = action.absolute_position();
        let size = action.size();
        let position =
            slint::LogicalPosition::new(origin.x + size.width / 2., origin.y + size.height / 2.);
        window.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        window
            .window()
            .dispatch_event(WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            });
        action.invoke_accessible_default_action();
    }
    assert_eq!(
        (external.get(), chooser.get()),
        (0, 0),
        "the real FilesWindow must refuse external launches for temporary archive members"
    );
    let file_keys = Rc::new(Cell::new(0));
    let result = file_keys.clone();
    window.on_keyboard(move |_, _, _, _| result.set(result.get() + 1));
    for key in [Key::Delete, Key::DownArrow] {
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
        window
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
    }
    assert_eq!(
        file_keys.get(),
        0,
        "viewer keyboard events must never delete or change background file selection"
    );
    let closed = Rc::new(Cell::new(false));
    let result = closed.clone();
    preview.on_close(move || result.set(true));
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Escape.into(),
    });
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Escape.into(),
    });
    assert!(closed.get(), "Escape reaches the visible viewer scope");
}

#[test]
fn archive_member_preview_hides_the_archive_and_escape_restores_its_navigation() {
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
    let window = FilesWindow::new().unwrap();
    skins::install(window.global::<SkinRenderer>(), window.as_weak());
    let theme: rix_config::ThemeSettings = serde_json::from_str(include_str!(
        "../../../assets/rix/themes/fairywren-dark/theme.json"
    ))
    .unwrap();
    theme_bindings::apply(window.global::<Theme>(), window.global::<Palette>(), &theme);
    window.set_desktop_material_supported(true);
    populate_browser(window.global::<ArchiveBrowser>(), &theme);
    let browser = window.global::<ArchiveBrowser>();
    browser.set_selected_index(1);
    let preview = window.global::<FilePreview>();
    preview.set_title("Release notes.txt".into());
    preview.set_subtitle("Text document · 4.8 KiB".into());
    preview.set_kind(2);
    preview.set_body("Rix Files keeps document previews close to the archive.\n\nEscape returns to the same archive folder and preserves the selected member.\n\nThe archive header and file list must not appear through the desktop glass.".into());
    preview.set_note("Extract the archive to open this file in another application".into());
    preview.set_external_allowed(false);
    preview.set_visible(true);
    window.window().set_size(slint::PhysicalSize::new(760, 480));
    window.show().unwrap();
    window.invoke_focus_files();
    slint::platform::update_timers_and_animations();
    assert!(window.get_using_desktop_material());
    assert!(
        browser.get_visible(),
        "archive state stays open while its member is viewed"
    );
    for label in ["Password…", "Extract…", "Release notes.pdf"] {
        assert!(
            ElementHandle::find_by_accessible_label(&window, label)
                .next()
                .is_none(),
            "archive control {label} must not be present beneath a transparent member preview"
        );
    }
    assert!(
        ElementHandle::find_by_accessible_label(&window, "Document Text")
            .next()
            .is_some()
    );
    let file_keys = Rc::new(Cell::new(0));
    let result = file_keys.clone();
    window.on_keyboard(move |_, _, _, _| result.set(result.get() + 1));
    let archive_closes = Rc::new(Cell::new(0));
    let result = archive_closes.clone();
    browser.on_close(move || result.set(result.get() + 1));
    let weak = window.as_weak();
    preview.on_close(move || {
        let window = weak.upgrade().unwrap();
        window.global::<FilePreview>().set_visible(false);
        window.invoke_focus_files();
    });
    let key = |text: slint::SharedString| {
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        window
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text });
        slint::platform::update_timers_and_animations();
    };
    let output = std::env::temp_dir().join("rix-files-archive-ui");
    std::fs::create_dir_all(&output).unwrap();
    let render = |name: &str| {
        let mut pixels = vec![slint::Rgb8Pixel::default(); 760 * 480];
        surface.request_redraw();
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 760);
        });
        let bytes: Vec<_> = pixels
            .iter()
            .flat_map(|pixel| [pixel.r, pixel.g, pixel.b])
            .collect();
        image::save_buffer(output.join(name), &bytes, 760, 480, image::ColorType::Rgb8).unwrap();
    };
    render("glass-760-real-member-preview.png");
    key(Key::DownArrow.into());
    assert_eq!(
        browser.get_selected_index(),
        1,
        "preview keys must not move archive selection"
    );
    key(Key::Escape.into());
    assert!(!preview.get_visible());
    assert!(browser.get_visible());
    assert_eq!(
        archive_closes.get(),
        0,
        "the first Escape closes only the member preview"
    );
    assert!(
        ElementHandle::find_by_accessible_label(&window, "Document Text")
            .next()
            .is_none()
    );
    for label in ["Password…", "Extract…", "Release notes.pdf"] {
        assert!(
            ElementHandle::find_by_accessible_label(&window, label)
                .next()
                .is_some(),
            "archive control {label} returns after closing the preview"
        );
    }
    assert_eq!(browser.get_folder(), "Projects/RixDE");
    assert_eq!(browser.get_selected_index(), 1);
    render("glass-760-real-archive-return.png");
    key(Key::DownArrow.into());
    assert_eq!(
        browser.get_selected_index(),
        2,
        "restored archive scope receives navigation keys"
    );
    let opened = Rc::new(Cell::new(-1));
    let result = opened.clone();
    browser.on_open(move |index| result.set(index));
    key(Key::Return.into());
    assert_eq!(opened.get(), 2);
    key(Key::Escape.into());
    assert_eq!(
        archive_closes.get(),
        1,
        "a second Escape now reaches the archive"
    );
    assert_eq!(
        file_keys.get(),
        0,
        "neither content overlay forwards keys to background files"
    );
}

#[test]
fn archive_keyboard_scopes_alt_up_password_escape_and_restored_focus() {
    i_slint_backend_testing::init_no_event_loop();
    let window = ArchiveFixture::new().unwrap();
    let theme = rix_config::ThemeSettings::default();
    theme_bindings::apply(window.global::<Theme>(), window.global::<Palette>(), &theme);
    populate(&window, &theme);
    window.show().unwrap();
    window.invoke_focus_browser();
    let browser = window.global::<ArchiveBrowser>();
    let up = Rc::new(Cell::new(0));
    let result = up.clone();
    browser.on_up(move || result.set(result.get() + 1));
    browser.set_selected_index(2);
    key(&window, Key::UpArrow.into());
    assert_eq!(browser.get_selected_index(), 1);
    assert_eq!(up.get(), 0);
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Alt.into(),
    });
    key(&window, Key::UpArrow.into());
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Alt.into(),
    });
    assert_eq!(
        up.get(),
        1,
        "Alt+Up opens the parent instead of moving selection"
    );
    assert_eq!(browser.get_selected_index(), 1);
    let closed = Rc::new(Cell::new(0));
    let result = closed.clone();
    browser.on_close(move || result.set(result.get() + 1));
    click(&window, "Password…");
    click(&window, "Archive password");
    key(&window, "private-password".into());
    assert_eq!(browser.get_password(), "private-password");
    key(&window, Key::Escape.into());
    assert_eq!(
        closed.get(),
        1,
        "Escape from the password field closes the browser"
    );
    let selected = browser.get_selected_index();
    click(&window, "Archive password");
    browser.set_focus_generation(browser.get_focus_generation() + 1);
    slint::platform::update_timers_and_animations();
    key(&window, Key::DownArrow.into());
    assert_eq!(
        browser.get_selected_index(),
        selected + 1,
        "the worker completion focus generation restores archive navigation"
    );
    let opened = Rc::new(Cell::new(-1));
    let result = opened.clone();
    browser.on_open(move |index| result.set(index));
    key(&window, Key::Return.into());
    assert_eq!(opened.get(), selected + 1);
    let extracted = Rc::new(Cell::new(false));
    let result = extracted.clone();
    browser.on_extract(move || result.set(true));
    click(&window, "Extract…");
    assert!(extracted.get());
}

#[test]
fn render_archive_and_password_controls_fit_compact_and_large_desktop_themes() {
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
    let window = ArchiveFixture::new().unwrap();
    skins::install(window.global::<SkinRenderer>(), window.as_weak());
    let glass: rix_config::ThemeSettings = serde_json::from_str(include_str!(
        "../../../assets/rix/themes/fairywren-dark/theme.json"
    ))
    .unwrap();
    let output = std::env::temp_dir().join("rix-files-archive-ui");
    std::fs::create_dir_all(&output).unwrap();
    for (name, theme) in [
        ("dark", rix_config::ThemeSettings::default()),
        ("light", rix_config::ThemeSettings::crisp_light()),
        ("glass", glass),
    ] {
        theme_bindings::apply(window.global::<Theme>(), window.global::<Palette>(), &theme);
        window.set_desktop_material(name == "glass");
        populate(&window, &theme);
        for (width, height) in [(760u32, 480u32), (1080, 720)] {
            window
                .window()
                .set_size(slint::PhysicalSize::new(width, height));
            window.show().unwrap();
            window.invoke_focus_browser();
            for password in [false, true] {
                window.global::<ArchiveBrowser>().set_message(
                    if password {
                        "This archive needs a password. Enter it to read its contents."
                    } else {
                        ""
                    }
                    .into(),
                );
                slint::platform::update_timers_and_animations();
                let mut labels = vec!["Password…", "Extract…", "Close", "Up", "Release notes.pdf"];
                if password {
                    labels.extend(["Archive password", "Unlock"]);
                }
                for label in labels {
                    let control = element(&window, label);
                    let origin = control.absolute_position();
                    let size = control.size();
                    assert!(
                        origin.x >= 0.
                            && origin.y >= 0.
                            && origin.x + size.width <= width as f32 + 0.5
                            && origin.y + size.height <= height as f32 + 0.5,
                        "{label} must fit the {width}x{height} archive: {origin:?} {size:?}"
                    );
                }
                let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
                surface.request_redraw();
                surface.draw_if_needed(|renderer| {
                    renderer.render(&mut pixels, width as usize);
                });
                let bytes: Vec<_> = pixels
                    .iter()
                    .flat_map(|pixel| [pixel.r, pixel.g, pixel.b])
                    .collect();
                assert!(bytes.iter().any(|byte| *byte > 120));
                image::save_buffer(
                    output.join(format!(
                        "{name}-{width}-{}.png",
                        if password { "password" } else { "browse" }
                    )),
                    &bytes,
                    width,
                    height,
                    image::ColorType::Rgb8,
                )
                .unwrap();
            }
        }
    }
}
