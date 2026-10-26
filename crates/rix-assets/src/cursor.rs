#[derive(Clone)]
pub struct Frame {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub hotspot: (u32, u32),
    pub delay: u32,
    pub nominal_size: u32,
}

pub fn load(theme: &str, names: &[&str], requested: u32) -> Option<Vec<Frame>> {
    let theme = xcursor::CursorTheme::load(theme);
    let path = names.iter().find_map(|name| theme.load_icon(name))?;
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut data = Vec::new();
    file.take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut data)
        .ok()?;
    if data.len() > 16 * 1024 * 1024 {
        return None;
    }
    decode(&data, requested)
}
fn decode(data: &[u8], requested: u32) -> Option<Vec<Frame>> {
    let images = xcursor::parser::parse_xcursor(data)?;
    let size = images
        .iter()
        .min_by_key(|i| i.size.abs_diff(requested))?
        .size;
    let frames: Vec<_> = images
        .into_iter()
        .filter(|i| i.size == size)
        .filter(|i| i.width <= 512 && i.height <= 512)
        .map(|mut i| {

            for pixel in i.pixels_rgba.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
            Frame {
                pixels: i.pixels_rgba,
                width: i.width,
                height: i.height,
                hotspot: (i.xhot, i.yhot),
                delay: i.delay.max(1),
                nominal_size: i.size,
            }
        })
        .collect();
    (!frames.is_empty()).then_some(frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn xcursor_keeps_hotspot_channels_animation_and_selects_size() {

        let mut bytes = b"Xcur".to_vec();
        let mut u32le = |v: u32| bytes.extend(v.to_le_bytes());
        for v in [16, 0x10000, 3] {
            u32le(v);
        }
        for (size, offset) in [(24, 52), (24, 96), (48, 140)] {
            for v in [0xfffd0002, size, offset] {
                u32le(v);
            }
        }
        for (size, delay, color) in [
            (24, 80, [10, 20, 30, 128]),
            (24, 120, [40, 50, 60, 128]),
            (48, 80, [70, 80, 90, 128]),
        ] {
            for v in [36u32, 0xfffd0002, size, 1, 2, 1, 1, 0, delay] {
                bytes.extend(v.to_le_bytes());
            }
            bytes.extend(color);
            bytes.extend(color);
        }
        let frames = decode(&bytes, 24).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].hotspot, (1, 0));
        assert_eq!(&frames[0].pixels[..4], &[30, 20, 10, 128]);
        assert_eq!(frames[1].delay, 120);
        assert_eq!(decode(&bytes, 48).unwrap()[0].nominal_size, 48);
    }
}
