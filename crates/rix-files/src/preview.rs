use anyhow::{anyhow, bail, Context, Result};
use quick_xml::{events::Event, Reader};
use std::{
    fs::{File, Metadata, OpenOptions},
    io::{Cursor, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
};

pub const MAX_TEXT_BYTES: usize = 1024 * 1024;
const MAX_PDF_BYTES: usize = 128 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_DOCUMENT_BYTES: usize = 64 * 1024 * 1024;
const MAX_DOCUMENT_XML_BYTES: usize = 8 * 1024 * 1024;
const MAX_PIXELS: u64 = 12 * 1024 * 1024;
const MAX_RASTER_SIDE: u32 = 4096;
const HEX_BYTES: usize = 4096;

#[derive(Clone, Debug)]
pub struct Request {
    pub path: PathBuf,

    pub page: usize,

    pub zoom: f32,
    pub target_width: u32,
}

impl Request {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            page: 0,
            zoom: 1.0,
            target_width: 960,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Pdf,
    Image,
    Text,
    Document,
    Binary,
}

#[derive(Clone, Debug)]
pub enum Content {
    Raster {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    Text {
        body: String,
    },
    Binary {
        hex: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Note {

    Truncated,

    DocumentTextOnly,

    UnsupportedFormat,

    LossyText,

    RenderingLimited,
}

#[derive(Clone, Debug)]
pub struct Preview {
    pub path: PathBuf,
    pub title: String,
    pub kind: Kind,
    pub content: Content,
    pub page: usize,
    pub page_count: usize,
    pub zoom: f32,
    pub file_size: u64,
    pub notes: Vec<Note>,
}

impl Preview {
    fn new(request: &Request, kind: Kind, content: Content, file_size: u64) -> Self {
        Self {
            path: request.path.clone(),
            title: request
                .path
                .file_name()
                .unwrap_or(request.path.as_os_str())
                .to_string_lossy()
                .into_owned(),
            kind,
            content,
            page: 0,
            page_count: 1,
            zoom: normalized_zoom(request.zoom),
            file_size,
            notes: Vec::new(),
        }
    }
}

pub fn normalized_zoom(zoom: f32) -> f32 {
    if zoom.is_finite() {
        zoom.clamp(0.25, 4.0)
    } else {
        1.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextMatch {

    pub start: usize,
    pub end: usize,
}

pub fn find_matches(body: &str, query: &str) -> Vec<TextMatch> {
    const MAX_MATCHES: usize = 4096;
    if query.is_empty() {
        return Vec::new();
    }
    let query = query.to_lowercase();
    let mut folded = String::with_capacity(body.len());
    let mut boundaries = vec![(0usize, 0usize)];
    for (offset, character) in body.char_indices() {
        folded.extend(character.to_lowercase());
        boundaries.push((folded.len(), offset + character.len_utf8()));
    }
    folded
        .match_indices(&query)
        .take(MAX_MATCHES)
        .map(|(start, matched)| {
            let end = start + matched.len();
            let first = boundaries
                .partition_point(|&(folded_offset, _)| folded_offset <= start)
                .saturating_sub(1);
            let last = boundaries
                .partition_point(|&(folded_offset, _)| folded_offset < end)
                .min(boundaries.len() - 1);
            TextMatch {
                start: boundaries[first].1,
                end: boundaries[last].1,
            }
        })
        .collect()
}

#[derive(PartialEq, Eq)]
struct Identity {
    path: PathBuf,
    device: u64,
    inode: u64,
    length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl Identity {
    fn new(path: &std::path::Path, metadata: &Metadata) -> Self {
        Self {
            path: path.to_path_buf(),
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

struct CachedPdf {
    identity: Identity,
    document: hayro::hayro_syntax::Pdf,
}

#[derive(Default)]
pub struct Loader {
    cached_pdf: Option<CachedPdf>,
}

impl Loader {
    pub fn clear(&mut self) {
        self.cached_pdf = None;
    }

    pub fn load(&mut self, request: Request) -> Result<Preview> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.load_inner(request))) {
            Ok(result) => result,
            Err(_) => {
                self.clear();
                Err(anyhow!("This file could not be rendered safely"))
            }
        }
    }

    fn load_inner(&mut self, request: Request) -> Result<Preview> {
        if self
            .cached_pdf
            .as_ref()
            .is_some_and(|cached| cached.identity.path != request.path)
        {
            self.clear();
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&request.path)
            .with_context(|| format!("Could not read {}", request.path.display()))?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            bail!("Only regular files can be previewed");
        }
        let size = metadata.len();
        let identity = Identity::new(&request.path, &metadata);
        if let Some(cached) = &self.cached_pdf {
            if cached.identity == identity {
                return render_pdf(&request, &cached.document, size);
            }
        }
        self.clear();
        load_file(self, request, &mut file, size, identity)
    }
}

pub fn load(request: Request) -> Result<Preview> {
    Loader::default().load(request)
}

fn load_file(
    loader: &mut Loader,
    request: Request,
    file: &mut File,
    size: u64,
    identity: Identity,
) -> Result<Preview> {
    let ext = request
        .path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let mut prefix = vec![0; 4096];
    let count = file.read(&mut prefix)?;
    prefix.truncate(count);
    let is_pdf = prefix.windows(5).take(1024).any(|w| w == b"%PDF-");
    if is_pdf || ext == "pdf" {
        if size > MAX_PDF_BYTES as u64 {
            bail!("This file exceeds the built-in preview size limit");
        }
        let data = read_remainder(file, prefix, MAX_PDF_BYTES, false)?.0;
        let document = parse_pdf(data)?;
        let result = render_pdf(&request, &document, size)?;
        loader.cached_pdf = Some(CachedPdf { identity, document });
        return Ok(result);
    }
    if matches!(
        ext.as_str(),
        "docx" | "odt" | "pptx" | "xlsx" | "odp" | "ods"
    ) {
        if size > MAX_DOCUMENT_BYTES as u64 {
            bail!("This file exceeds the built-in preview size limit");
        }
        let data = read_remainder(file, prefix, MAX_DOCUMENT_BYTES, false)?.0;
        return document(&request, data, size, &ext);
    }
    if image::guess_format(&prefix).is_ok() {
        if size > MAX_IMAGE_BYTES as u64 {
            bail!("This file exceeds the built-in preview size limit");
        }
        let data = read_remainder(file, prefix, MAX_IMAGE_BYTES, false)?.0;
        return render_image(&request, data, size);
    }
    let (mut bytes, truncated) = read_remainder(file, prefix, MAX_TEXT_BYTES, true)?;

    if truncated {
        if let Err(error) = std::str::from_utf8(&bytes) {
            if error.error_len().is_none() && bytes.len() - error.valid_up_to() <= 3 {
                bytes.truncate(error.valid_up_to());
            }
        }
    }
    if let Some((body, lossy)) = decode_text(&bytes, known_text_extension(&ext)) {
        let mut preview = Preview::new(&request, Kind::Text, Content::Text { body }, size);
        if truncated {
            preview.notes.push(Note::Truncated);
        }
        if lossy {
            preview.notes.push(Note::LossyText);
        }
        return Ok(preview);
    }
    let mut preview = Preview::new(
        &request,
        Kind::Binary,
        Content::Binary {
            hex: hex_dump(&bytes[..bytes.len().min(HEX_BYTES)]),
        },
        size,
    );
    preview.notes.push(Note::UnsupportedFormat);
    if bytes.len() > HEX_BYTES || truncated {
        preview.notes.push(Note::Truncated);
    }
    Ok(preview)
}

fn read_remainder(
    file: &mut File,
    mut prefix: Vec<u8>,
    maximum: usize,
    allow_prefix: bool,
) -> Result<(Vec<u8>, bool)> {
    let remaining = maximum.saturating_add(1).saturating_sub(prefix.len());
    file.take(remaining as u64).read_to_end(&mut prefix)?;
    let truncated = prefix.len() > maximum;
    if truncated && !allow_prefix {
        bail!("This file exceeds the built-in preview size limit");
    }
    prefix.truncate(maximum);
    Ok((prefix, truncated))
}

fn raster_dimensions(width: f32, height: f32, desired_width: f32) -> Result<(u32, u32, f32, bool)> {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        bail!("This page has invalid dimensions");
    }
    let requested_scale = desired_width / width;
    let max_scale = (MAX_RASTER_SIDE as f32 / width)
        .min(MAX_RASTER_SIDE as f32 / height)
        .min((MAX_PIXELS as f64 / (width as f64 * height as f64)).sqrt() as f32);
    let scale = requested_scale.min(max_scale).max(f32::MIN_POSITIVE);
    let output_width = (width * scale).floor().clamp(1.0, MAX_RASTER_SIDE as f32) as u32;
    let output_height = (height * scale).floor().clamp(1.0, MAX_RASTER_SIDE as f32) as u32;
    Ok((output_width, output_height, scale, scale < requested_scale))
}

fn parse_pdf(bytes: Vec<u8>) -> Result<hayro::hayro_syntax::Pdf> {
    use hayro::hayro_syntax::{LoadPdfError, Pdf};
    Pdf::new(bytes).map_err(|error| match error {
        LoadPdfError::Decryption(_) => {
            anyhow!("Password-protected PDFs need an external application")
        }
        LoadPdfError::Invalid => anyhow!("This PDF is damaged or unsupported"),
    })
}

fn render_pdf(request: &Request, pdf: &hayro::hayro_syntax::Pdf, size: u64) -> Result<Preview> {
    use hayro::{
        hayro_interpret::InterpreterSettings, vello_cpu::color::palette::css::WHITE, RenderCache,
        RenderSettings,
    };
    if pdf.pages().is_empty() {
        bail!("This PDF contains no pages");
    }
    let page_count = pdf.pages().len();
    let page_index = request.page.min(page_count - 1);
    let page = &pdf.pages()[page_index];
    let (page_width, page_height) = page.render_dimensions();
    let desired_width =
        request.target_width.clamp(160, 2048) as f32 * normalized_zoom(request.zoom);
    let (width, height, scale, limited) =
        raster_dimensions(page_width, page_height, desired_width)?;
    let raster = hayro::render(
        page,
        &RenderCache::new(),
        &InterpreterSettings::default(),
        &RenderSettings {
            x_scale: scale,
            y_scale: scale,
            width: Some(width as u16),
            height: Some(height as u16),
            bg_color: WHITE,
        },
    );
    let mut preview = Preview::new(
        request,
        Kind::Pdf,
        Content::Raster {
            width,
            height,
            rgba: raster.data_as_u8_slice().to_vec(),
        },
        size,
    );
    preview.page = page_index;
    preview.page_count = page_count;
    if limited {
        preview.notes.push(Note::RenderingLimited);
    }
    Ok(preview)
}

fn render_image(request: &Request, bytes: Vec<u8>, size: u64) -> Result<Preview> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    limits.max_alloc = Some(MAX_PIXELS * 4);
    reader.limits(limits);
    let source = reader
        .decode()
        .context("This image is damaged, unsupported or too large")?;
    let desired_width = (request.target_width.clamp(160, 2048) as f32
        * normalized_zoom(request.zoom))
    .min(source.width() as f32);
    let (width, height, _, limited) =
        raster_dimensions(source.width() as f32, source.height() as f32, desired_width)?;
    let raster = source
        .resize_exact(width, height, image::imageops::FilterType::Triangle)
        .into_rgba8();
    let mut preview = Preview::new(
        request,
        Kind::Image,
        Content::Raster {
            width,
            height,
            rgba: raster.into_raw(),
        },
        size,
    );
    if limited {
        preview.notes.push(Note::RenderingLimited);
    }
    Ok(preview)
}

fn known_text_extension(ext: &str) -> bool {
    matches!(
        ext,
        "txt"
            | "md"
            | "markdown"
            | "log"
            | "csv"
            | "tsv"
            | "json"
            | "toml"
            | "yaml"
            | "yml"
            | "xml"
            | "html"
            | "htm"
            | "css"
            | "js"
            | "ts"
            | "jsx"
            | "tsx"
            | "rs"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "py"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "ini"
            | "conf"
            | "desktop"
            | "service"
            | "svg"
            | "sql"
            | "nix"
    )
}

fn decode_text(bytes: &[u8], known_text: bool) -> Option<(String, bool)> {
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        let little_endian = bytes[0] == 0xff;
        let (words, remainder) = bytes[2..].as_chunks::<2>();
        let mut lossy = !remainder.is_empty();
        let words = words.iter().map(|pair| {
            if little_endian {
                u16::from_le_bytes(*pair)
            } else {
                u16::from_be_bytes(*pair)
            }
        });
        let text: String = char::decode_utf16(words)
            .map(|c| match c {
                Ok(c) => c,
                Err(_) => {
                    lossy = true;
                    '\u{fffd}'
                }
            })
            .collect();
        return Some((text, lossy));
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);

    if bytes.contains(&0)
        || bytes
            .iter()
            .filter(|&&b| b < 32 && !matches!(b, b'\n' | b'\r' | b'\t' | 0x1b))
            .count()
            > bytes.len() / 100
    {
        return None;
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => Some((text.to_owned(), false)),
        Err(_) if known_text => Some((String::from_utf8_lossy(bytes).into_owned(), true)),
        Err(_) => None,
    }
}

fn document(request: &Request, bytes: Vec<u8>, size: u64, ext: &str) -> Result<Preview> {
    let binary_summary = hex_dump(&bytes[..bytes.len().min(HEX_BYTES)]);
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).context("This document package is damaged")?;
    if archive.len() > 10000 {
        bail!("This document contains too many parts");
    }
    let names: Vec<String> = match ext {
        "docx" => vec!["word/document.xml".into()],
        "odt" | "ods" | "odp" => vec!["content.xml".into()],
        "pptx" => {
            let mut names: Vec<_> = archive
                .file_names()
                .filter(|name| {
                    name.starts_with("ppt/slides/slide")
                        && name.ends_with(".xml")
                        && !name.contains("/_rels/")
                })
                .map(str::to_owned)
                .collect();
            names.sort_by_key(|name| {
                name.trim_start_matches("ppt/slides/slide")
                    .trim_end_matches(".xml")
                    .parse::<usize>()
                    .unwrap_or(usize::MAX)
            });
            names.truncate(1000);
            names
        }

        _ => Vec::new(),
    };
    if names.is_empty() {
        let mut preview = Preview::new(
            request,
            Kind::Binary,
            Content::Binary {
                hex: binary_summary,
            },
            size,
        );
        preview.notes.push(Note::UnsupportedFormat);
        return Ok(preview);
    }
    let mut body = String::new();
    let mut total_xml = 0;
    let mut truncated = false;
    for name in names {
        let part = archive
            .by_name(&name)
            .with_context(|| format!("This document has no {name}"))?;
        if !part.is_file() {
            bail!("This document part is not a regular file");
        }
        let remaining = MAX_DOCUMENT_XML_BYTES.saturating_sub(total_xml);
        if remaining == 0 {
            truncated = true;
            break;
        }
        let mut xml = Vec::new();
        part.take((remaining + 1) as u64).read_to_end(&mut xml)?;
        if xml.len() > remaining {
            bail!("This document exceeds the built-in text preview limit");
        }
        total_xml += xml.len();
        if ext == "pptx" && !body.is_empty() {
            body.push_str("\n\n");
        }
        truncated |= extract_document_text(&xml, ext, &mut body)?;
        if truncated {
            break;
        }
    }
    let mut preview = Preview::new(request, Kind::Document, Content::Text { body }, size);
    preview.notes.push(Note::DocumentTextOnly);
    if truncated {
        preview.notes.push(Note::Truncated);
    }
    Ok(preview)
}

fn append_bounded(body: &mut String, value: &str) -> bool {
    let remaining = MAX_TEXT_BYTES.saturating_sub(body.len());
    if value.len() <= remaining {
        body.push_str(value);
        return false;
    }
    let mut end = remaining.min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    body.push_str(&value[..end]);
    true
}

fn extract_document_text(xml: &[u8], ext: &str, body: &mut String) -> Result<bool> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().check_end_names = true;
    let mut depth = 0usize;
    let mut text_depth = None;
    let odf = matches!(ext, "odt" | "ods" | "odp");
    loop {
        match reader.read_event()? {
            Event::Start(tag) => {
                depth += 1;
                if depth > 128 {
                    bail!("This document is nested too deeply");
                }
                let name = tag.name();
                let name = name.as_ref();
                if (!odf && matches!(name, b"w:t" | b"a:t"))
                    || (odf && matches!(name, b"text:p" | b"text:h"))
                {
                    text_depth = Some(depth);
                }
            }
            Event::End(tag) => {
                let name = tag.name();
                let name = name.as_ref();
                if text_depth == Some(depth) {
                    text_depth = None;
                }
                if matches!(
                    name,
                    b"w:p" | b"a:p" | b"text:p" | b"text:h" | b"table:table-row"
                ) {
                    if append_bounded(body, "\n") {
                        return Ok(true);
                    }
                } else if matches!(name, b"w:tc" | b"table:table-cell")
                    && append_bounded(body, "\t")
                {
                    return Ok(true);
                }
                depth = depth.saturating_sub(1);
            }
            Event::Empty(tag) => {
                let name = tag.name();
                let name = name.as_ref();
                let value = if matches!(name, b"w:br" | b"a:br" | b"text:line-break") {
                    "\n"
                } else if matches!(name, b"w:tab" | b"text:tab") {
                    "\t"
                } else if name == b"text:s" {
                    " "
                } else {
                    ""
                };
                if append_bounded(body, value) {
                    return Ok(true);
                }
            }
            Event::Text(text) if text_depth.is_some() => {
                if append_bounded(body, &text.xml_content()?) {
                    return Ok(true);
                }
            }
            Event::CData(text) if text_depth.is_some() => {
                if append_bounded(body, &text.decode()?) {
                    return Ok(true);
                }
            }
            Event::GeneralRef(reference) if text_depth.is_some() => {
                let name = reference.decode()?;
                let entity = format!("&{name};");
                let decoded = quick_xml::escape::unescape(&entity)?;
                if append_bounded(body, &decoded) {
                    return Ok(true);
                }
            }

            Event::DocType(_) => bail!("Document type declarations are not supported in previews"),
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(false)
}

fn hex_dump(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut result = String::new();
    for (line, chunk) in bytes.chunks(16).enumerate() {
        let _ = write!(result, "{:08x}  ", line * 16);
        for index in 0..16 {
            if let Some(byte) = chunk.get(index) {
                let _ = write!(result, "{byte:02x} ");
            } else {
                result.push_str("   ");
            }
            if index == 7 {
                result.push(' ');
            }
        }
        result.push_str(" │");
        for &byte in chunk {
            result.push(if byte.is_ascii_graphic() || byte == b' ' {
                byte as char
            } else {
                '·'
            });
        }
        result.push_str("│\n");
    }
    result
}

#[cfg(test)]
mod tests;
