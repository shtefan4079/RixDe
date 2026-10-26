use i_slint_backend_testing::ElementHandle;
use slint::{
    platform::{Key, PointerEventButton, WindowEvent},
    ComponentHandle,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

slint::slint! {
    import { FilePreview, PreviewPanel } from "../ui/preview.slint";
    import { Theme } from "../../../ui/common/theme.slint";
    import { ThemeFrame, SkinFrame, SkinRenderer } from "../../../ui/common/frame.slint";
    import { Palette } from "std-widgets.slint";
    import { MenuHost, SharedMenuItem } from "../../../ui/common/context_menu.slint";
    export { FilePreview, Theme, Palette, SkinFrame, SkinRenderer, MenuHost, SharedMenuItem }
    export component PreviewFixture inherits Window {
        preferred-width: 1080px; preferred-height: 720px;
        background: Theme.window-background;
        default-font-family: Theme.font-family;
        default-font-size: Theme.font-size-md;
        in property <bool> desktop-material;
        out property <length> document-y: panel.document-y;
        public function focus-preview() { panel.focus-preview(); }
        if root.desktop-material && Theme.panel-frame.available: ThemeFrame {
            width: parent.width; height: parent.height;
            skin: Theme.panel-frame; square-corners: true; material-opacity: Theme.material-opacity;
        }
        panel := PreviewPanel { width: root.width; height: root.height; desktop-material: root.desktop-material; }
    }
}
#[path = "../../../ui/common/skins.rs"]
mod skins;
#[path = "../../../ui/common/bindings.rs"]
mod theme_bindings;

fn element(window: &PreviewFixture, label: &str) -> ElementHandle {
    ElementHandle::find_by_accessible_label(window, label)
        .next()
        .unwrap_or_else(|| panic!("Missing {label}"))
}
fn click(window: &PreviewFixture, label: &str) {
    let control = element(window, label);
    let origin = control.absolute_position();
    let size = control.size();
    let window_size = window.window().size();
    let position = slint::LogicalPosition::new(
        (origin.x + size.width / 2.).min(window_size.width as f32 - 40.),
        (origin.y + size.height / 2.).min(window_size.height as f32 - 40.),
    );
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
fn key(window: &PreviewFixture, text: slint::SharedString) {
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text });
    slint::platform::update_timers_and_animations();
}
fn fixture() -> PreviewFixture {
    i_slint_backend_testing::init_no_event_loop();
    let window = PreviewFixture::new().unwrap();
    theme_bindings::apply(
        window.global::<Theme>(),
        window.global::<Palette>(),
        &rix_config::ThemeSettings::default(),
    );
    let preview = window.global::<FilePreview>();
    preview.set_title("report.pdf".into());
    preview.set_subtitle("PDF document · 2 pages".into());
    preview.set_kind(0);
    preview.set_page_count(2);
    window.show().unwrap();
    window.invoke_focus_preview();
    window
}

fn visual_pdf() -> Vec<u8> {
    use std::io::Write;
    let contents = "0.32 0.44 1 rg 40 680 520 70 re f\nBT /F1 28 Tf 1 1 1 rg 60 700 Td (Rix Files document preview) Tj ET\nBT /F1 18 Tf 0.15 0.18 0.22 rg 40 630 Td (Documents belong close to your files.) Tj 0 -40 Td (Readable text. Consistent controls. Your desktop theme.) Tj 0 -40 Td (This page is rendered entirely in Rust with Hayro.) Tj ET";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_owned(),
        format!("<< /Length {} >>\nstream\n{contents}\nendstream", contents.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
    ];
    let mut data = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(data.len());
        write!(data, "{} 0 obj\n{object}\nendobj\n", index + 1).unwrap();
    }
    let xref = data.len();
    write!(data, "xref\n0 6\n0000000000 65535 f \n").unwrap();
    for offset in offsets {
        writeln!(data, "{offset:010} 00000 n ").unwrap();
    }
    write!(
        data,
        "trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    )
    .unwrap();
    data
}

#[test]
fn pdf_controls_keyboard_zoom_and_unavailable_preview_keep_actions_usable() {
    let window = fixture();
    let page = Rc::new(Cell::new(-1));
    let result = page.clone();
    window
        .global::<FilePreview>()
        .on_page_requested(move |value| result.set(value));
    key(&window, Key::PageDown.into());
    assert_eq!(page.get(), 1);
    click(&window, "Next Page");
    assert_eq!(page.get(), 1);
    let zoom = Rc::new(Cell::new(0.));
    let result = zoom.clone();
    window
        .global::<FilePreview>()
        .on_zoom_requested(move |value| result.set(value));
    click(&window, "Zoom In");
    assert_eq!(zoom.get(), 1.25);
    window.global::<FilePreview>().set_zoom(4.);
    click(&window, "Zoom In");
    assert_eq!(zoom.get(), 1.25, "maximum scale disables the control");
    click(&window, "Fit");
    assert_eq!(zoom.get(), 1.);
    window
        .global::<FilePreview>()
        .set_message("Password-protected PDFs need an external application".into());
    let external = Rc::new(Cell::new(0));
    let result = external.clone();
    window
        .global::<FilePreview>()
        .on_open_externally(move || result.set(result.get() + 1));
    click(&window, "Open");
    assert_eq!(external.get(), 1);
    let closed = Rc::new(Cell::new(false));
    let result = closed.clone();
    window
        .global::<FilePreview>()
        .on_close(move || result.set(true));
    window.invoke_focus_preview();
    key(&window, Key::Escape.into());
    assert!(closed.get());
}

#[test]
fn temporary_archive_member_disables_external_actions_including_the_error_fallback() {
    let window = fixture();
    let preview = window.global::<FilePreview>();
    preview.set_external_allowed(false);
    preview.set_note("Extract the archive to open this file in another application".into());
    let external = Rc::new(Cell::new(0));
    let result = external.clone();
    preview.on_open_externally(move || result.set(result.get() + 1));
    let chooser = Rc::new(Cell::new(0));
    let result = chooser.clone();
    preview.on_open_with(move || result.set(result.get() + 1));
    for label in ["Open", "Open With…"] {
        assert_eq!(element(&window, label).accessible_enabled(), Some(false));
        click(&window, label);
        element(&window, label).invoke_accessible_default_action();
    }
    assert_eq!((external.get(), chooser.get()), (0, 0));
    preview.set_message("This format needs an external application".into());
    slint::platform::update_timers_and_animations();
    let fallback_buttons: Vec<_> = ElementHandle::find_by_accessible_label(&window, "Open With…")
        .filter(|action| action.accessible_enabled().is_some())
        .collect();
    let header = element(&window, "Open With…");
    let header_bottom = header.absolute_position().y + header.size().height;
    assert!(
        fallback_buttons
            .iter()
            .any(|action| action.absolute_position().y > header_bottom),
        "the unavailable-preview fallback is a separate action below the header"
    );
    for action in fallback_buttons {
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
    assert_eq!((external.get(), chooser.get()), (0, 0));
    preview.set_message("".into());
    preview.set_external_allowed(true);
    slint::platform::update_timers_and_animations();
    click(&window, "Open");
    click(&window, "Open With…");
    assert_eq!(
        (external.get(), chooser.get()),
        (1, 1),
        "normal local files retain distinct working actions"
    );
}

#[test]
fn document_is_read_only_and_search_reveals_a_match_below_the_fold() {
    let window = fixture();
    let text = (0..250)
        .map(|index| format!("{index}: A paragraph in our document.\n"))
        .collect::<String>();
    let preview = window.global::<FilePreview>();
    preview.set_title("readme.txt".into());
    preview.set_kind(2);
    preview.set_body(text.clone().into());
    slint::platform::update_timers_and_animations();
    click(&window, "Document Text");
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Control.into(),
    });
    key(&window, "a".into());
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Control.into(),
    });
    key(&window, "overwrite".into());
    assert_eq!(
        preview.get_body().as_str(),
        text,
        "read-only input must never modify source text"
    );
    assert_eq!(
        element(&window, "Document Text")
            .accessible_value()
            .unwrap()
            .as_str(),
        text
    );
    preview.set_title("readme refreshed.txt".into());
    slint::platform::update_timers_and_animations();
    assert!(
        window.get_document_y().abs() < 1.,
        "new document resets the scroll before search"
    );
    click(&window, "Find");
    let query = Rc::new(Cell::new(false));
    let result = query.clone();
    preview.on_find_requested(move |text, backwards| result.set(text == "document" && !backwards));
    key(&window, "document".into());
    assert!(query.get());
    let start = text.rfind("document").unwrap();
    preview.set_match_start(start as i32);
    preview.set_match_end((start + "document".len()) as i32);
    preview.set_match_generation(1);
    slint::platform::update_timers_and_animations();
    assert!(
        window.get_document_y() < -1000.,
        "selection must reveal a real match far down the scrollable text"
    );
}

#[test]
fn document_text_menu_requests_native_blur_and_returns_to_a_read_only_selection() {
    use slint::Model;
    let window = fixture();
    let preview = window.global::<FilePreview>();
    preview.set_title("readme.txt".into());
    preview.set_kind(2);
    preview.set_body("A document we can select and copy.".into());
    slint::platform::update_timers_and_animations();
    window.global::<MenuHost>().on_register_owner(|| 73);
    let requests = Rc::new(RefCell::new(Vec::new()));
    let recorded = requests.clone();
    window
        .global::<MenuHost>()
        .on_request(move |owner, items, x, y, width, height, title| {
            recorded
                .borrow_mut()
                .push((owner, items, x, y, width, height, title));
            true
        });
    click(&window, "Document Text");
    key(&window, Key::Menu.into());
    let request = requests.borrow()[0].clone();
    assert_eq!(request.0, 73);
    assert_eq!(request.1.row_count(), 2);
    assert_eq!(request.1.row_data(0).unwrap().action, "copy");
    assert_eq!(request.1.row_data(1).unwrap().action, "select-all");
    assert!(request.2 >= 0. && request.3 > 0. && request.4 > 0. && request.5 > 0.);
    assert!(request.2 + request.4 <= window.window().size().width as f32);
    assert!(request.3 + request.5 <= window.window().size().height as f32);
    let host = window.global::<MenuHost>();
    host.set_event_owner(73);
    host.set_event_action("select-all".into());
    host.set_event_generation(1);
    slint::platform::update_timers_and_animations();
    key(&window, Key::Menu.into());
    let request = requests.borrow().last().unwrap().clone();
    assert!(
        request.1.row_data(0).unwrap().enabled,
        "The native result selected the actual document text"
    );
    host.set_event_action("".into());
    host.set_event_unavailable(true);
    host.set_event_generation(2);
    slint::platform::update_timers_and_animations();
    assert!(
        ElementHandle::find_by_accessible_label(&window, "Select All")
            .next()
            .is_some(),
        "Native failure must open the themed embedded context menu"
    );
    key(&window, Key::Escape.into());
    key(&window, "overwrite".into());
    assert_eq!(preview.get_body(), "A document we can select and copy.");
}

#[test]
fn render_pdf_and_document_previews_in_dark_light_and_glass_themes() {
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
    let window = PreviewFixture::new().unwrap();
    skins::install(window.global::<SkinRenderer>(), window.as_weak());
    let glass: rix_config::ThemeSettings = serde_json::from_str(include_str!(
        "../../../assets/rix/themes/fairywren-dark/theme.json"
    ))
    .unwrap();
    let output = std::env::var_os("RIX_FILES_PREVIEW_RENDER_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("rix-files-preview-ui"));
    std::fs::create_dir_all(&output).unwrap();
    let fixture_directory = tempfile::tempdir().unwrap();
    let path = fixture_directory.path().join("brief.pdf");
    std::fs::write(&path, visual_pdf()).unwrap();
    let mut request = rix_files::preview::Request::new(path);
    request.target_width = 1280;
    let rendered = rix_files::preview::load(request).unwrap();
    let rix_files::preview::Content::Raster {
        width: page_width,
        height: page_height,
        rgba,
    } = rendered.content
    else {
        panic!("PDF must provide real rendered pixels")
    };
    let page_image = slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(
        &rgba,
        page_width,
        page_height,
    ));
    for (name, theme) in [
        ("dark", rix_config::ThemeSettings::default()),
        ("light", rix_config::ThemeSettings::crisp_light()),
        ("glass", glass),
    ] {
        theme_bindings::apply(window.global::<Theme>(), window.global::<Palette>(), &theme);
        window.set_desktop_material(name == "glass");
        for (width, height) in [(760u32, 480u32), (1080, 720)] {
            window
                .window()
                .set_size(slint::PhysicalSize::new(width, height));
            for document in [false, true] {
                let preview = window.global::<FilePreview>();
                preview.set_title(
                    if document {
                        "Team notes.docx"
                    } else {
                        "Design brief.pdf"
                    }
                    .into(),
                );
                preview.set_subtitle(
                    if document {
                        "Office document · 24.5 KiB"
                    } else {
                        "PDF document · 1 page · 64.2 KiB"
                    }
                    .into(),
                );
                preview.set_kind(if document { 3 } else { 0 });
                preview.set_page_count(1);
                preview.set_zoom(1.);
                preview.set_body("Designing a consistent desktop\n\nRix Files keeps documents close to your files. Select text, find a passage and open the original in your preferred application.\n\nOur viewer uses the same colours, spacing and controls as the desktop. Office documents show readable text without claiming to preserve the original page layout.\n\nThe following paragraph also preserves clean formatting, punctuation and readability.".into());
                preview.set_external_allowed(!document);
                preview.set_note(if document { "Text preview. Original formatting and page layout are available in an external application. Extract the archive to open this file in another application." } else { "" }.into());
                preview.set_page_image(page_image.clone());
                preview.set_image_width(page_width as i32);
                preview.set_image_height(page_height as i32);
                window.show().unwrap();
                slint::platform::update_timers_and_animations();
                for label in [
                    "Open",
                    "Open With…",
                    "Close Preview",
                    "Zoom In",
                    "Zoom Out",
                    "Fit",
                ] {
                    let control = element(&window, label);
                    let origin = control.absolute_position();
                    let size = control.size();
                    assert!(
                        origin.x >= 0.
                            && origin.y >= 0.
                            && origin.x + size.width <= width as f32 + 0.5
                            && origin.y + size.height <= height as f32 + 0.5,
                        "{label} must fit the {width}x{height} preview: {origin:?} {size:?}"
                    );
                }
                if document {
                    let note = ElementHandle::find_by_element_id(&window, "PreviewPanel::note")
                        .next()
                        .expect("the archive extraction notice must stay visible");
                    let origin = note.absolute_position();
                    let size = note.size();
                    assert!(
                        size.height > 0. && origin.y + size.height <= height as f32 + 0.5,
                        "the extraction notice must fit the compact {width}x{height} preview"
                    );
                }
                let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
                surface.request_redraw();
                surface.draw_if_needed(|renderer| {
                    renderer.render(&mut pixels, width as usize);
                });
                let bytes: Vec<_> = pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
                assert!(bytes.iter().any(|b| *b > 120));
                image::save_buffer(
                    output.join(format!(
                        "{name}-{width}-{}.png",
                        if document { "document" } else { "pdf" }
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
