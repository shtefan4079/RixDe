use image::{GenericImageView, Rgba, RgbaImage};
use rix_capture::{
    editor::{selection, Edit, Editor, Point, Rect},
    encoding::encode_png,
};

fn pixel(x: u32, y: u32) -> Rgba<u8> {

    if y == 0 {
        return Rgba([241, 11, 37, 255]);
    }
    if y == 1079 {
        return Rgba([7, 19, 239, 255]);
    }
    if x >= 1440 && y >= 780 {
        let value = if (x + y).is_multiple_of(2) { 0 } else { 255 };
        return Rgba([value, value, value, 255]);
    }
    if x.is_multiple_of(17) {
        Rgba([9, 21, 35, 255])
    } else if y.is_multiple_of(29) {
        Rgba([183, 31, 57, 255])
    } else {
        Rgba([230, 235, 242, 255])
    }
}

fn desktop() -> RgbaImage {
    RgbaImage::from_fn(1920, 1080, pixel)
}

fn exported(editor: &Editor) -> RgbaImage {
    let bytes = encode_png(&editor.render()).unwrap();

    let request = rix_ipc::IpcRequest::ClipboardSetPng {
        data: bytes.clone(),
    };
    let wire = serde_json::to_vec(&request).unwrap();
    let rix_ipc::IpcRequest::ClipboardSetPng { data } = serde_json::from_slice(&wire).unwrap()
    else {
        panic!("Wrong clipboard request")
    };
    assert_eq!(data, bytes);
    image::load_from_memory_with_format(&data, image::ImageFormat::Png)
        .unwrap()
        .into_rgba8()
}

#[test]
fn full_hd_export_preserves_one_pixel_text_stems_checkerboard_and_row_orientation() {
    let image = exported(&Editor::new(desktop()).unwrap());
    assert_eq!(image.dimensions(), (1920, 1080));
    for (x, y, actual) in image.enumerate_pixels() {
        assert_eq!(
            *actual,
            pixel(x, y),
            "Native screenshot pixel changed at ({x}, {y})"
        );
    }
}

#[test]
fn large_lossless_png_remains_exportable_above_the_clipboard_transport_limit() {

    let mut noise = 0x7258_a419_u32;
    let source = RgbaImage::from_fn(2560, 2160, |_, _| {
        noise ^= noise << 13;
        noise ^= noise >> 17;
        noise ^= noise << 5;
        Rgba(noise.to_le_bytes())
    });
    let png = encode_png(&source).unwrap();
    assert!(png.len() > rix_ipc::CLIPBOARD_PNG_MAX_BYTES);
    let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
        .unwrap()
        .into_rgba8();
    assert_eq!(decoded, source);
}

#[test]
fn reverse_selection_at_fractional_scale_crops_native_pixels_without_resampling() {

    let rect = selection(
        Point { x: 612.4, y: 560.4 },
        Point { x: 100.4, y: 200.4 },
        (1536., 864.),
        (1920, 1080),
    )
    .unwrap();
    assert_eq!(
        rect,
        Rect {
            x: 125,
            y: 250,
            width: 641,
            height: 451,
        }
    );
    let source = desktop();
    let crop =
        image::imageops::crop_imm(&source, rect.x, rect.y, rect.width, rect.height).to_image();
    let image = exported(&Editor::new(crop).unwrap());
    assert_eq!(image.dimensions(), (641, 451));
    for (x, y, actual) in image.enumerate_pixels() {
        assert_eq!(*actual, pixel(x + 125, y + 250));
    }
}

#[test]
fn annotation_export_keeps_unedited_native_text_and_checkerboard_exact() {
    let mut editor = Editor::new(desktop()).unwrap();
    editor
        .apply(Edit::Redact(Rect {
            x: 50,
            y: 40,
            width: 500,
            height: 200,
        }))
        .unwrap();
    let image = exported(&editor);
    assert_eq!(image.dimensions(), (1920, 1080));
    for (x, y, actual) in image.enumerate_pixels() {
        let expected = if (50..550).contains(&x) && (40..240).contains(&y) {
            Rgba([24, 27, 36, 255])
        } else {
            pixel(x, y)
        };
        assert_eq!(*actual, expected);
    }
}

#[test]
fn crop_after_annotations_is_lossless_and_undo_restores_the_previous_size_and_pixels() {
    let source = desktop();
    let mut editor = Editor::new(source.clone()).unwrap();
    editor
        .apply(Edit::Rectangle {
            rect: Rect {
                x: 90,
                y: 180,
                width: 60,
                height: 40,
            },
            color: Rgba([31, 220, 81, 255]),
            width: 3.,
        })
        .unwrap();
    let annotated = editor.render();
    editor
        .apply(Edit::Crop(Rect {
            x: 50,
            y: 100,
            width: 700,
            height: 500,
        }))
        .unwrap();
    assert_eq!(editor.dimensions(), (700, 500));
    let cropped = exported(&editor);
    assert_eq!(
        cropped,
        image::imageops::crop_imm(&annotated, 50, 100, 700, 500).to_image()
    );

    editor
        .apply(Edit::Stroke {
            points: vec![Point { x: 20., y: 30. }, Point { x: 80., y: 30. }],
            color: Rgba([30, 90, 245, 255]),
            width: 4.,
        })
        .unwrap();
    assert_eq!(*editor.render().get_pixel(50, 30), Rgba([30, 90, 245, 255]));
    editor.undo();
    assert_eq!(exported(&editor), cropped);
    editor.undo();
    assert_eq!(editor.dimensions(), source.dimensions());
    assert_eq!(exported(&editor), annotated);
    editor.redo();
    assert_eq!(exported(&editor), cropped);
}

#[test]
fn styled_highlighter_has_uniform_opacity_and_annotated_text_is_baked_into_png() {
    let source = RgbaImage::from_pixel(500, 180, Rgba([230, 235, 242, 255]));
    let mut editor = Editor::new(source.clone()).unwrap();
    editor
        .apply(Edit::Highlight {
            points: vec![
                Point { x: 20., y: 30. },
                Point { x: 150., y: 30. },
                Point { x: 80., y: 30. },
            ],
            color: Rgba([20, 220, 60, 255]),
            width: 14.,
        })
        .unwrap();
    let highlighted = exported(&editor);
    assert_eq!(
        highlighted.get_pixel(50, 30),
        highlighted.get_pixel(120, 30)
    );
    let font = rix_capture::text::font("DejaVu Sans")
        .expect("The UI test environment needs a desktop font");
    editor
        .apply(Edit::Text {
            origin: Point { x: 20., y: 70. },
            text: "Screenshot Rix 123".into(),
            font,
            size: 28.,
            color: Rgba([245, 30, 55, 255]),
        })
        .unwrap();
    let annotated = exported(&editor);
    assert!(
        image::imageops::crop_imm(&annotated, 20, 70, 300, 40)
            .pixels()
            .filter(|(_, _, pixel)| pixel[0] > 230 && pixel[1] < 150)
            .count()
            > 200
    );
    assert_eq!(annotated.get_pixel(499, 179), source.get_pixel(499, 179));
    editor.undo();
    assert_eq!(exported(&editor), highlighted);
    editor.redo();
    assert_eq!(exported(&editor), annotated);
}
