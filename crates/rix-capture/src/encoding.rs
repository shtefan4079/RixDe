use anyhow::Result;
use image::RgbaImage;
use std::io::Cursor;

pub fn encode_png(image: &RgbaImage) -> Result<Vec<u8>> {
    let mut buffer = Cursor::new(Vec::new());
    image.write_to(&mut buffer, image::ImageFormat::Png)?;

    Ok(buffer.into_inner())
}
