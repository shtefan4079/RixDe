use std::{io::Read, path::PathBuf};

#[test]
#[ignore = "requires the disposable Capture Wayland fixture"]
fn capture_png_reaches_an_external_wayland_client_without_resampling() {
    assert_eq!(
        std::env::var("RIX_CAPTURE_ISOLATED_WAYLAND").as_deref(),
        Ok("1")
    );
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    let base = PathBuf::from(std::env::var_os("RIX_FILES_PRIVATE_FIXTURE").unwrap());
    assert_eq!(runtime, base.join("runtime"));
    assert_eq!(
        std::env::var("WAYLAND_DISPLAY").as_deref(),
        Ok("rix-capture-fixture")
    );
    use wl_clipboard_rs::paste::{get_contents, ClipboardType, MimeType, Seat};
    let (mut pipe, mime) = get_contents(
        ClipboardType::Regular,
        Seat::Unspecified,
        MimeType::Specific("image/png"),
    )
    .unwrap();
    assert_eq!(mime, "image/png");
    let mut data = Vec::new();
    (&mut pipe)
        .take(rix_ipc::CLIPBOARD_PNG_MAX_BYTES as u64 + 1)
        .read_to_end(&mut data)
        .unwrap();
    assert!(data.len() <= rix_ipc::CLIPBOARD_PNG_MAX_BYTES);
    let actual = image::load_from_memory_with_format(&data, image::ImageFormat::Png)
        .unwrap()
        .into_rgba8();
    let rect: Vec<u32> = std::env::var("RIX_CAPTURE_EXPECTED_RECT")
        .unwrap()
        .split(',')
        .map(|n| n.parse().unwrap())
        .collect();
    assert_eq!(actual.dimensions(), (rect[2], rect[3]));
    if let Some(path) = std::env::var_os("RIX_CAPTURE_EXPECTED_SOURCE") {
        let expected = image::open(path).unwrap().into_rgba8();
        assert_eq!(
            actual,
            image::imageops::crop_imm(&expected, rect[0], rect[1], rect[2], rect[3]).to_image()
        );
    }
    if let Some(path) = std::env::var_os("RIX_CAPTURE_CLIPBOARD_PNG") {
        std::fs::write(path, data).unwrap();
    }
}
