use super::*;
use std::{fs, io::Write};

fn save(dir: &tempfile::TempDir, name: &str, data: &[u8]) -> Request {
    let path = dir.path().join(name);
    fs::write(&path, data).unwrap();
    Request::new(path)
}

fn body(preview: &Preview) -> &str {
    match &preview.content {
        Content::Text { body } => body,
        _ => panic!("expected text"),
    }
}

#[test]
fn utf16_document_and_binary_detection_do_not_depend_on_filename() {
    let dir = tempfile::tempdir().unwrap();
    let text = "Hello\nA & B 🦀";
    let mut bytes = vec![0xff, 0xfe];
    for word in text.encode_utf16() {
        bytes.extend(word.to_le_bytes());
    }
    let preview = load(save(&dir, "utf16.bin", &bytes)).unwrap();
    assert_eq!(preview.kind, Kind::Text);
    assert_eq!(body(&preview), text);
    assert!(preview.notes.is_empty());
    let preview = load(save(&dir, "claimed-text.txt", &[0, 0xff, 0x7f, 0x21])).unwrap();
    assert_eq!(preview.kind, Kind::Binary);
    match preview.content {
        Content::Binary { hex } => assert!(hex.contains("00 ff 7f 21")),
        _ => panic!(),
    }
}

#[test]
fn large_text_is_bounded_and_invalid_encoding_is_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let preview = load(save(&dir, "log.txt", &vec![b'A'; MAX_TEXT_BYTES + 200])).unwrap();
    assert_eq!(body(&preview).len(), MAX_TEXT_BYTES);
    assert_eq!(preview.notes, vec![Note::Truncated]);
    let preview = load(save(&dir, "note.txt", b"hello \xff world")).unwrap();
    assert_eq!(body(&preview), "hello � world");
    assert_eq!(preview.notes, vec![Note::LossyText]);
    let preview = load(save(&dir, "unknown.bin", b"hello \xff world")).unwrap();
    assert_eq!(preview.kind, Kind::Binary);
}

#[test]
fn special_files_are_rejected_without_blocking_or_writing() {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("fifo.txt");
    let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    assert!(load(Request::new(fifo))
        .unwrap_err()
        .to_string()
        .contains("regular"));
    assert!(load(Request::new(dir.path().to_path_buf()))
        .unwrap_err()
        .to_string()
        .contains("regular"));
    assert!(fs::read_dir(dir.path())
        .unwrap()
        .all(|entry| entry.unwrap().file_name() == "fifo.txt"));
}

fn package(xml_path: &str, xml: &str) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file(
            xml_path,
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
    writer.write_all(xml.as_bytes()).unwrap();
    writer.finish().unwrap().into_inner()
}

#[test]
fn docx_and_odt_preserve_paragraphs_tabs_unicode_and_entities() {
    let dir = tempfile::tempdir().unwrap();
    let docx = package("word/document.xml", "<w:document xmlns:w='http://schemas.openxmlformats.org/wordprocessingml/2006/main'><w:body><w:p><w:r><w:t>A &amp; B &#x1F980;</w:t><w:tab/><w:t>World</w:t><w:br/><w:t>next</w:t></w:r></w:p><w:p><w:r><w:t>Paragraph</w:t></w:r></w:p></w:body></w:document>");
    let preview = load(save(&dir, "report.docx", &docx)).unwrap();
    assert_eq!(preview.kind, Kind::Document);
    assert_eq!(body(&preview), "A & B 🦀\tWorld\nnext\nParagraph\n");
    assert_eq!(preview.notes, vec![Note::DocumentTextOnly]);
    let odt = package("content.xml", "<office:document-content xmlns:office='urn:oasis:names:tc:opendocument:xmlns:office:1.0' xmlns:text='urn:oasis:names:tc:opendocument:xmlns:text:1.0'><text:p>Hello <text:span>bold &lt;note&gt;</text:span><text:tab/>World</text:p><text:p>Again</text:p></office:document-content>");
    let preview = load(save(&dir, "letter.odt", &odt)).unwrap();
    assert_eq!(body(&preview), "Hello bold <note>\tWorld\nAgain\n");
}

#[test]
fn document_rejects_dtds_excessive_depth_and_decompression_bombs() {
    let dir = tempfile::tempdir().unwrap();
    let external = "<!DOCTYPE w:document [<!ENTITY secret SYSTEM 'file:///etc/passwd'>]><w:document><w:t>&secret;</w:t></w:document>";
    assert!(load(save(
        &dir,
        "external.docx",
        &package("word/document.xml", external)
    ))
    .unwrap_err()
    .to_string()
    .contains("declarations"));
    let depth = format!("{}text{}", "<w:p>".repeat(129), "</w:p>".repeat(129));
    assert!(load(save(
        &dir,
        "nested.docx",
        &package("word/document.xml", &depth)
    ))
    .unwrap_err()
    .to_string()
    .contains("deeply"));
    let huge = "A".repeat(MAX_DOCUMENT_XML_BYTES + 1);
    assert!(load(save(
        &dir,
        "huge.docx",
        &package("word/document.xml", &huge)
    ))
    .unwrap_err()
    .to_string()
    .contains("limit"));
}

#[test]
fn genuine_image_renders_pixels_and_unknown_office_format_has_a_summary() {
    let dir = tempfile::tempdir().unwrap();
    let source = image::RgbaImage::from_fn(40, 20, |x, _| {
        if x < 20 {
            image::Rgba([255, 0, 0, 255])
        } else {
            image::Rgba([0, 0, 255, 255])
        }
    });
    let mut output = Cursor::new(Vec::new());
    source
        .write_to(&mut output, image::ImageFormat::Png)
        .unwrap();
    let preview = load(save(&dir, "image-renamed.bin", output.get_ref())).unwrap();
    assert_eq!(preview.kind, Kind::Image);
    match preview.content {
        Content::Raster {
            width,
            height,
            rgba,
        } => {
            assert_eq!((width, height), (40, 20));
            assert_eq!(&rgba[..4], &[255, 0, 0, 255]);
            assert_eq!(&rgba[39 * 4..40 * 4], &[0, 0, 255, 255]);
        }
        _ => panic!(),
    }
    let preview = load(save(
        &dir,
        "sheet.xlsx",
        &package("xl/sharedStrings.xml", "<sst/>"),
    ))
    .unwrap();
    assert_eq!(preview.kind, Kind::Binary);
    match preview.content {
        Content::Binary { hex } => assert!(hex.contains("50 4b")),
        _ => panic!(),
    }
}

fn two_page_pdf() -> Vec<u8> {
    let red = "1 0 0 rg 0 0 200 100 re f";
    let blue = "0 0 1 rg 0 0 200 100 re f";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Resources << >> /Contents 4 0 R >>"
            .to_string(),
        format!("<< /Length {} >>\nstream\n{red}\nendstream", red.len()),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Resources << >> /Contents 6 0 R >>"
            .to_string(),
        format!("<< /Length {} >>\nstream\n{blue}\nendstream", blue.len()),
    ];
    let mut data = b"%PDF-1.4\n".to_vec();
    let mut offsets = vec![0];
    for (index, object) in objects.iter().enumerate() {
        offsets.push(data.len());
        write!(data, "{} 0 obj\n{object}\nendobj\n", index + 1).unwrap();
    }
    let xref = data.len();
    write!(data, "xref\n0 7\n0000000000 65535 f \n").unwrap();
    for offset in &offsets[1..] {
        writeln!(data, "{offset:010} 00000 n ").unwrap();
    }
    write!(
        data,
        "trailer\n<< /Size 7 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    )
    .unwrap();
    data
}

#[test]
fn pdf_page_navigation_and_zoom_produce_real_bounded_rasters() {
    let dir = tempfile::tempdir().unwrap();
    let mut request = save(&dir, "document.pdf", &two_page_pdf());
    request.target_width = 200;
    let first = load(request.clone()).unwrap();
    assert_eq!(first.kind, Kind::Pdf);
    assert_eq!((first.page, first.page_count), (0, 2));
    match first.content {
        Content::Raster {
            width,
            height,
            rgba,
        } => {
            assert_eq!((width, height), (200, 100));
            assert_eq!(&rgba[100 * 4..101 * 4], &[255, 0, 0, 255]);
        }
        _ => panic!(),
    }
    request.page = usize::MAX;
    request.zoom = 2.0;
    let second = load(request).unwrap();
    assert_eq!((second.page, second.page_count), (1, 2));
    match second.content {
        Content::Raster {
            width,
            height,
            rgba,
        } => {
            assert_eq!((width, height), (400, 200));
            assert_eq!(&rgba[100 * 4..101 * 4], &[0, 0, 255, 255]);
        }
        _ => panic!(),
    }
    assert!(load(save(&dir, "corrupt.pdf", b"%PDF-1.4\ninvalid")).is_err());
}

#[test]
fn parsed_pdf_cache_is_worker_safe_and_invalidates_when_a_file_is_replaced() {
    fn assert_send<T: Send>() {}
    assert_send::<Loader>();
    let dir = tempfile::tempdir().unwrap();
    let mut request = save(&dir, "document.pdf", &two_page_pdf());
    request.target_width = 200;
    let mut loader = Loader::default();
    loader.load(request.clone()).unwrap();
    let first_pdf = loader
        .cached_pdf
        .as_ref()
        .unwrap()
        .document
        .data()
        .as_ref()
        .as_ptr();
    request.page = 1;
    let second = loader.load(request.clone()).unwrap();
    assert_eq!(second.page, 1);
    assert_eq!(
        loader
            .cached_pdf
            .as_ref()
            .unwrap()
            .document
            .data()
            .as_ref()
            .as_ptr(),
        first_pdf
    );
    let replacement = dir.path().join("replacement.pdf");
    fs::write(&replacement, b"%PDF-1.4\ndamaged replacement").unwrap();
    fs::rename(replacement, &request.path).unwrap();
    assert!(
        loader.load(request).is_err(),
        "an atomically replaced PDF must not show old cached pages"
    );
    assert!(loader.cached_pdf.is_none());
    loader
        .load(save(&dir, "again.pdf", &two_page_pdf()))
        .unwrap();
    loader.clear();
    assert!(loader.cached_pdf.is_none());
}

#[test]
fn pathological_page_geometry_is_clamped_and_non_finite_zoom_is_safe() {
    assert!(raster_dimensions(f32::NAN, 100.0, 200.0).is_err());
    assert!(raster_dimensions(200.0, 0.0, 200.0).is_err());
    let (width, height, _, limited) = raster_dimensions(10.0, 1_000_000.0, 2000.0).unwrap();
    assert!(limited);
    assert!(width > 0 && height > 0 && height <= MAX_RASTER_SIDE);
    assert!(width as u64 * height as u64 <= MAX_PIXELS);
    assert_eq!(normalized_zoom(f32::NAN), 1.0);
    assert_eq!(normalized_zoom(f32::INFINITY), 1.0);
}

#[test]
fn unicode_find_returns_original_utf8_boundaries_and_caps_dense_results() {
    let text = "Grüße · İSTANBUL · grüße · IstanBul";
    let matches = find_matches(text, "GRÜSSE");
    assert_eq!(matches.len(), 0);
    let matches = find_matches(text, "GRÜßE");
    assert_eq!(matches.len(), 2);
    assert_eq!(&text[matches[0].start..matches[0].end], "Grüße");
    assert_eq!(&text[matches[1].start..matches[1].end], "grüße");
    let matches = find_matches(text, "i");
    assert_eq!(matches.len(), 2);
    assert_eq!(&text[matches[0].start..matches[0].end], "İ");
    assert_eq!(&text[matches[1].start..matches[1].end], "I");
    assert!(find_matches(text, "").is_empty());
    assert_eq!(find_matches(&"a".repeat(5000), "a").len(), 4096);
}
