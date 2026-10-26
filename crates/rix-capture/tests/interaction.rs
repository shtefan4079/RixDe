use rix_capture::{apply_theme, CaptureWindow, Palette, Theme};
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, PointerEventButton, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, PhysicalSize};
use std::{cell::RefCell, rc::Rc};

struct Headless(Rc<MinimalSoftwareWindow>);
impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}

fn render(ui: &CaptureWindow, adapter: &MinimalSoftwareWindow) -> image::RgbImage {
    slint::platform::update_timers_and_animations();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1280 * 800];
    ui.window().request_redraw();
    adapter.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 1280);
    });
    let bytes: Vec<u8> = pixels
        .iter()
        .flat_map(|pixel| [pixel.r, pixel.g, pixel.b])
        .collect();
    image::RgbImage::from_raw(1280, 800, bytes).unwrap()
}

fn assert_frozen_view(image: &image::RgbImage, source: &image::RgbaImage) {

    for (x, y) in [(100, 200), (100, 700), (1100, 200), (1100, 700)] {
        let actual = image.get_pixel(x, y).0;
        let expected = source.get_pixel(x, y).0.map(|channel| {

            (u32::from(channel) * 175 / 255) as u8
        });
        for channel in 0..3 {
            assert!(
                actual[channel].abs_diff(expected[channel]) <= 2,
                "Frozen screenshot must fill the selection surface at ({x}, {y}): \
                 expected {:?}, found {:?}",
                &expected[..3],
                actual,
            );
        }
    }
}

#[test]
fn frozen_selection_and_editor_receive_real_pointer_input_and_cancel_does_not_capture() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Headless(adapter.clone()))).unwrap();
    let ui = CaptureWindow::new().unwrap();
    assert_eq!(ui.get_phase(), 1, "Selection must be ready at startup");
    assert!(
        !ui.get_native_size(),
        "The ordinary editor initially fits the document without changing exported pixels"
    );
    let dark = rix_config::ThemeSettings::default();
    apply_theme(&ui, &dark);
    let mut sample = image::RgbaImage::new(1280, 800);
    for (x, y, pixel) in sample.enumerate_pixels_mut() {
        *pixel = image::Rgba([
            25 + (x / 50) as u8,
            40 + (y / 25) as u8,
            95 + (x / 20) as u8,
            255,
        ]);
    }
    let pixels =
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(sample.as_raw(), 1280, 800);
    ui.set_frozen(slint::Image::from_rgba8(pixels.clone()));
    ui.set_result(slint::Image::from_rgba8(pixels));
    ui.set_pixel_width(1280);
    ui.set_pixel_height(800);
    ui.set_phase(1);
    ui.set_monitor("DP-1".into());
    ui.set_monitors(slint::ModelRc::new(slint::VecModel::from(vec![
        "DP-1".into(),
        "HDMI-A-1".into(),
    ])));
    ui.show().unwrap();
    ui.window().set_size(PhysicalSize::new(1280, 800));
    ui.set_phase(0);
    let frozen_view = render(&ui, &adapter);
    assert_frozen_view(&frozen_view, &sample);
    ui.set_phase(1);
    assert_frozen_view(&render(&ui, &adapter), &sample);
    let mut light = rix_config::ThemeSettings::crisp_light();
    light
        .appearance
        .palette
        .insert("accent".into(), "#ea580c".into());
    light.accent_color = "#ea580c".into();
    light.appearance.font_family = "Noto Sans".into();
    light.appearance.menu_frame.clear();
    apply_theme(&ui, &light);
    assert_eq!(
        ui.global::<Theme>().get_accent(),
        slint::Color::from_rgb_u8(234, 88, 12)
    );
    assert_eq!(ui.global::<Theme>().get_font_family().as_str(), "Noto Sans");
    assert_eq!(
        ui.global::<Palette>().get_color_scheme(),
        slint::language::ColorScheme::Light
    );
    let light_view = render(&ui, &adapter);
    assert_frozen_view(&light_view, &sample);
    assert_ne!(
        light_view.get_pixel(640, 20),
        frozen_view.get_pixel(640, 20),
        "The capture toolbar must use the active theme"
    );
    if let Some(directory) = std::env::var_os("RIX_CAPTURE_RENDER_DIR") {
        std::fs::create_dir_all(&directory).unwrap();
        frozen_view
            .save(std::path::Path::new(&directory).join("capture-frozen.png"))
            .unwrap();
        light_view
            .save(std::path::Path::new(&directory).join("capture-light.png"))
            .unwrap();
    }
    apply_theme(&ui, &dark);
    let selected = Rc::new(RefCell::new(Vec::new()));
    let recorded = selected.clone();
    ui.on_selection(move |ax, ay, bx, by, w, h| recorded.borrow_mut().push((ax, ay, bx, by, w, h)));
    for event in [
        WindowEvent::PointerPressed {
            position: LogicalPosition::new(300., 250.),
            button: PointerEventButton::Left,
        },
        WindowEvent::PointerMoved {
            position: LogicalPosition::new(900., 650.),
        },
        WindowEvent::PointerReleased {
            position: LogicalPosition::new(900., 650.),
            button: PointerEventButton::Left,
        },
    ] {
        ui.window().dispatch_event(event);
    }
    assert_eq!(
        selected.borrow().as_slice(),
        &[(300., 250., 900., 650., 1280., 800.)]
    );
    ui.set_phase(2);
    ui.set_has_image(true);
    ui.set_tool(3);
    ui.set_status("PNG copied · Ctrl+C to repeat".into());
    ui.set_save_path("/home/rix/Pictures/Screenshots/Rix.png".into());
    let result = render(&ui, &adapter);
    let center = result.get_pixel(640, 400).0;
    assert!(
        center[0] >= 30 && center[1] >= 48 && center[2] >= 100,
        "Editor must display the screenshot rather than its empty canvas: {center:?}"
    );
    ui.set_native_size(false);
    render(&ui, &adapter)
        .save("/tmp/rix-capture-editor-before.png")
        .unwrap();
    eprintln!("Editor size: {:?}", ui.window().size());
    slint::platform::update_timers_and_animations();
    let fitted_zoom = ui.get_preview_zoom();
    assert!(fitted_zoom > 0. && fitted_zoom < 1.);

    ui.set_pixel_width(2560);
    ui.set_pixel_height(1600);
    render(&ui, &adapter);
    assert!((ui.get_preview_zoom() - fitted_zoom / 2.).abs() < 0.001);
    ui.set_pixel_width(1280);
    ui.set_pixel_height(800);
    ui.set_native_size(true);
    render(&ui, &adapter);
    assert_eq!(ui.get_preview_zoom(), 1.);
    let strokes = Rc::new(RefCell::new(Vec::new()));
    let recorded = strokes.clone();
    ui.on_stroke(move |stage, x, y, w, h| recorded.borrow_mut().push((stage, x, y, w, h)));
    for event in [
        WindowEvent::PointerPressed {
            position: LogicalPosition::new(400., 300.),
            button: PointerEventButton::Left,
        },
        WindowEvent::PointerMoved {
            position: LogicalPosition::new(850., 500.),
        },
        WindowEvent::PointerReleased {
            position: LogicalPosition::new(850., 500.),
            button: PointerEventButton::Left,
        },
    ] {
        ui.window().dispatch_event(event);
    }
    assert_eq!(strokes.borrow().first().unwrap().0, 0);
    assert_eq!(strokes.borrow().last().unwrap().0, 2);
    assert!(strokes.borrow().iter().all(|s| s.3 > 0. && s.4 > 0.));
    let cancelled = Rc::new(RefCell::new(false));
    let recorded = cancelled.clone();
    ui.on_cancel(move || *recorded.borrow_mut() = true);
    ui.invoke_focus_controls();
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: slint::platform::Key::Escape.into(),
    });
    assert!(
        !*cancelled.borrow(),
        "Escape in the ordinary editor resets the tool, not the application"
    );
    assert_eq!(ui.get_tool(), 0);
    assert_eq!(selected.borrow().len(), 1);
    *cancelled.borrow_mut() = false;
    ui.window().dispatch_event(WindowEvent::KeyReleased {
        text: slint::platform::Key::Escape.into(),
    });
    ui.invoke_show_save();
    render(&ui, &adapter);
    let before = ui.get_save_path();
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: "X".into() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text: "X".into() });
    assert_ne!(
        ui.get_save_path(),
        before,
        "The path input must actually receive focus"
    );
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: slint::platform::Key::Escape.into(),
    });
    assert!(
        !*cancelled.borrow(),
        "Escape dismisses the save dialog without closing the editor"
    );
    assert!(!ui.get_dialog_open());
    ui.set_phase(1);
    ui.invoke_focus_controls();
    ui.window().dispatch_event(WindowEvent::KeyReleased {
        text: slint::platform::Key::Escape.into(),
    });
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: slint::platform::Key::Escape.into(),
    });
    assert!(
        *cancelled.borrow(),
        "Escape must still cancel a fullscreen selection"
    );
    ui.set_phase(2);
    if let Some(directory) = std::env::var_os("RIX_CAPTURE_RENDER_DIR") {
        std::fs::create_dir_all(&directory).unwrap();
        render(&ui, &adapter)
            .save(std::path::Path::new(&directory).join("capture-editor.png"))
            .unwrap();
    }
}
