use ab_glyph::{point, Font, FontArc, PxScale, ScaleFont};
use anyhow::{Context, Result};
use image::{Rgba, RgbaImage};
use std::sync::{Mutex, OnceLock};

pub fn font(family: &str) -> Result<FontArc> {
    static FONTS: OnceLock<Mutex<fontdb::Database>> = OnceLock::new();
    let fonts = FONTS
        .get_or_init(|| {
            let mut fonts = fontdb::Database::new();
            fonts.load_system_fonts();
            Mutex::new(fonts)
        })
        .lock()
        .unwrap();
    let id = fonts
        .query(&fontdb::Query {
            families: &[
                fontdb::Family::Name(family),
                fontdb::Family::Name("Noto Sans"),
                fontdb::Family::Name("DejaVu Sans"),
                fontdb::Family::SansSerif,
            ],
            ..Default::default()
        })
        .context("Font not found for caption")?;
    fonts
        .with_face_data(id, |bytes, index| {
            ab_glyph::FontVec::try_from_vec_and_index(bytes.to_vec(), index).map(FontArc::new)
        })
        .context("Failed to read font")?
        .map_err(Into::into)
}

pub fn paint(
    image: &mut RgbaImage,
    origin: super::editor::Point,
    text: &str,
    font: &FontArc,
    size: f32,
    color: Rgba<u8>,
) {
    let scaled = font.as_scaled(PxScale::from(size));
    let mut cursor = point(origin.x, origin.y + scaled.ascent());
    let mut previous = None;
    for character in text.chars() {
        if character == '\n' {
            cursor.x = origin.x;
            cursor.y += scaled.height() + scaled.line_gap();
            previous = None;
            continue;
        }
        let id = scaled.glyph_id(character);
        if let Some(previous) = previous {
            cursor.x += scaled.kern(previous, id);
        }
        let glyph = id.with_scale_and_position(size, cursor);
        cursor.x += scaled.h_advance(id);
        previous = Some(id);
        if let Some(glyph) = font.outline_glyph(glyph) {
            let bounds = glyph.px_bounds();
            glyph.draw(|x, y, coverage| {
                let x = bounds.min.x as i64 + i64::from(x);
                let y = bounds.min.y as i64 + i64::from(y);
                if x >= 0 && y >= 0 && x < i64::from(image.width()) && y < i64::from(image.height())
                {
                    let pixel = image.get_pixel_mut(x as u32, y as u32);
                    let alpha = (coverage * f32::from(color.0[3])).round() as u16;
                    for c in 0..3 {
                        pixel.0[c] = ((u16::from(pixel.0[c]) * (255 - alpha)
                            + u16::from(color.0[c]) * alpha)
                            / 255) as u8;
                    }
                    pixel.0[3] = 255;
                }
            });
        }
    }
}
