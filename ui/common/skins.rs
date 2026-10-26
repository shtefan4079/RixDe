use crate::{SkinFrame, SkinRenderer};
use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer};
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    path::PathBuf,
    time::SystemTime,
};
struct LoadedFrame {
    skin: SkinFrame,
    tiles: Vec<rix_assets::Tile>,
}
type FrameCache = HashMap<(PathBuf, String, u32, Option<SystemTime>), LoadedFrame>;
#[derive(PartialEq, Eq)]
struct RenderKey {
    id: i32,
    width: u32,
    height: u32,
    scale: u32,
    square: bool,
    corner_radius: Option<u32>,
}
struct RenderedFrame {
    key: RenderKey,
    image: Image,
    bytes: usize,
}
thread_local! {
    static RENDERED: RefCell<VecDeque<RenderedFrame>> = const { RefCell::new(VecDeque::new()) };
    static NEXT_ID: std::cell::Cell<i32> = const { std::cell::Cell::new(1) };
}

pub fn install<C: ComponentHandle + 'static>(renderer: SkinRenderer<'_>, window: slint::Weak<C>) {
    let rounded_window = window.clone();
    renderer.on_render_rounded(move |skin, width, height, square, radius| {
        let scale = rounded_window
            .upgrade()
            .map(|w| w.window().scale_factor())
            .unwrap_or(1.);
        render_with_radius(&skin, width, height, square, scale, Some(radius))
    });
    renderer.on_render(move |skin, width, height, square| {

        let scale = window
            .upgrade()
            .map(|w| w.window().scale_factor())
            .unwrap_or(1.);
        render(&skin, width, height, square, scale)
    });
}

fn render(skin: &SkinFrame, width: f32, height: f32, square: bool, scale: f32) -> Image {
    render_with_radius(skin, width, height, square, scale, None)
}

fn render_with_radius(
    skin: &SkinFrame,
    width: f32,
    height: f32,
    square: bool,
    scale: f32,
    radius: Option<f32>,
) -> Image {
    if !skin.available
        || ![width, height, scale]
            .iter()
            .all(|v| v.is_finite() && *v > 0.)
        || width * scale > 8192.
        || height * scale > 8192.
    {
        return Image::default();
    }
    let key = RenderKey {
        id: skin.cache_key,
        width: (width * scale).ceil() as u32,
        height: (height * scale).ceil() as u32,
        scale: scale.to_bits(),
        square,
        corner_radius: radius
            .filter(|r| r.is_finite())
            .map(|r| r.max(0.).to_bits()),
    };
    RENDERED.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(index) = cache.iter().position(|frame| frame.key == key) {
            if let Some(frame) = cache.remove(index) {

                let image = frame.image.clone();
                cache.push_back(frame);
                return image;
            }
        }
        let image = CACHE
            .with(|source| {
                let source = source.borrow();
                let frame = source
                    .values()
                    .find(|frame| frame.skin.cache_key == skin.cache_key)?;
                compose(
                    frame,
                    key.width,
                    key.height,
                    scale,
                    square,
                    key.corner_radius.map(f32::from_bits),
                )
            })
            .unwrap_or_default();
        let bytes = key.width as usize * key.height as usize * 4;
        const CACHE_BYTES: usize = 32 * 1024 * 1024;

        let mut total: usize = cache.iter().map(|frame| frame.bytes).sum();
        while total + bytes > CACHE_BYTES {
            match cache.pop_front() {
                Some(frame) => total -= frame.bytes,
                None => break,
            }
        }
        if bytes <= CACHE_BYTES {
            cache.push_back(RenderedFrame {
                key,
                image: image.clone(),
                bytes,
            });
        }
        image
    })
}

fn compose(
    frame: &LoadedFrame,
    width: u32,
    height: u32,
    scale: f32,
    square: bool,
    radius: Option<f32>,
) -> Option<Image> {

    if width == 0 || height == 0 || width > 8192 || height > 8192 {
        return None;
    }
    let inset = |value: f32, extent: u32| (value * scale).round().min(extent as f32 / 2.) as u32;
    let borders = radius
        .map(|radius| [inset(radius, width.min(height)); 4])
        .unwrap_or_else(|| {
            [
                inset(frame.skin.left_width, width),
                inset(frame.skin.top_height, height),
                inset(frame.skin.right_width, width),
                inset(frame.skin.bottom_height, height),
            ]
        });
    let mut tiles = frame.tiles.clone();
    if square {
        tiles[0] = tiles[1].clone();
        tiles[2] = tiles[1].clone();
        tiles[6] = tiles[7].clone();
        tiles[8] = tiles[7].clone();
    }
    let pixels = rix_assets::compose_frame_filtered(&tiles, width, height, borders).ok()?;
    Some(Image::from_rgba8_premultiplied(SharedPixelBuffer::<
        Rgba8Pixel,
    >::clone_from_slice(
        &pixels.pixels,
        width,
        height,
    )))
}
thread_local! { static CACHE: RefCell<FrameCache> = RefCell::new(HashMap::new()); }
pub fn load(reference: &str, scale: u32) -> SkinFrame {
    let (reference, prefix) = reference.split_once('#').unwrap_or((reference, ""));
    let Some(path) = rix_assets::resolve(reference) else {
        return SkinFrame::default();
    };
    let key = (
        path.clone(),
        prefix.to_owned(),
        scale,
        path.metadata().and_then(|m| m.modified()).ok(),
    );
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(frame) = cache.get(&key) {
            return frame.skin.clone();
        }
        let tiles = match rix_assets::load_frame_at_scale(&path, prefix, scale) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(path = %path.display(), "Could not load frame: {e}");
                return SkinFrame::default();
            }
        };
        let images: Vec<Image> = tiles
            .iter()
            .map(|tile| {
                if tile.width == 0 || tile.height == 0 {
                    return Image::default();
                }
                Image::from_rgba8_premultiplied(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                    &tile.pixels,
                    tile.width,
                    tile.height,
                ))
            })
            .collect();
        let frame = SkinFrame {
            cache_key: NEXT_ID.with(|next| {
                let id = next.get();
                next.set(id + 1);
                id
            }),
            center_is_solid: solid_color(&tiles[4]).is_some(),
            center_color: solid_color(&tiles[4]).unwrap_or_default(),
            available: true,
            corner_radius: tiles[3].width.min(tiles[1].height) as f32 / scale as f32,
            top_left: images[0].clone(),
            top: images[1].clone(),
            top_right: images[2].clone(),
            left: images[3].clone(),
            center: images[4].clone(),
            right: images[5].clone(),
            bottom_left: images[6].clone(),
            bottom: images[7].clone(),
            bottom_right: images[8].clone(),
            left_width: tiles[3].width as f32 / scale as f32,
            right_width: tiles[5].width as f32 / scale as f32,
            top_height: tiles[1].height as f32 / scale as f32,
            bottom_height: tiles[7].height as f32 / scale as f32,
        };
        cache.retain(|(cached_path, cached_prefix, _, _), _| {
            cached_path != &path || cached_prefix != prefix
        });
        cache.insert(
            key,
            LoadedFrame {
                skin: frame.clone(),
                tiles,
            },
        );
        frame
    })
}

fn solid_color(tile: &rix_assets::Tile) -> Option<slint::Color> {
    let pixel = tile.pixels.get(..4)?;
    if !tile
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .all(|p| p.as_slice() == pixel)
    {
        return None;
    }
    let alpha = pixel[3] as u32;
    let channel = |value: u8| {
        (value as u32 * 255 + alpha / 2)
            .checked_div(alpha)
            .unwrap_or(0)
            .min(255) as u8
    };
    Some(slint::Color::from_argb_u8(
        pixel[3],
        channel(pixel[0]),
        channel(pixel[1]),
        channel(pixel[2]),
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn complete_frame_has_filtered_corners_and_no_internal_seams() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../rix-assets/tests/data/rounded-frame.svg");
        let skin = super::load(path.to_str().unwrap(), 4);
        assert!(skin.available);
        for scale in [1., 1.25, 1.5, 2.] {
            let image = super::render(&skin, 160., 100., false, scale);
            assert_eq!(
                image,
                super::render(&skin, 160., 100., false, scale),
                "Stable geometry must reuse the image"
            );
            let pixels = image.to_rgba8_premultiplied().unwrap();
            let alpha =
                |x: u32, y: u32| pixels.as_bytes()[((y * pixels.width() + x) * 4 + 3) as usize];
            let radius = (8. * scale) as u32;
            let expected = alpha(pixels.width() / 2, pixels.height() / 2);
            assert!(expected > 0 && expected < 255);
            for y in [
                radius / 2,
                radius,
                pixels.height() - radius,
                pixels.height() - radius / 2,
            ] {
                for x in [radius, pixels.width() - radius] {
                    for x in x - 2..=x + 2 {
                        assert_eq!(alpha(x, y), expected, "Seam at {x},{y}, scale {scale}");
                    }
                }
            }
            assert_eq!(alpha(0, 0), 0);
            assert!(
                (0..radius).any(|y| (0..radius).any(|x| {
                    let a = alpha(x, y);
                    a > 0 && a < expected
                })),
                "Curve needs fractional alpha coverage"
            );
            let square = super::render(&skin, 160., 100., true, scale)
                .to_rgba8_premultiplied()
                .unwrap();
            assert_eq!(
                square.as_bytes()[3],
                expected,
                "Square mode must fill corners"
            );
        }
    }

    #[test]
    fn only_uniform_centers_become_solid_fills() {
        let mut tile = rix_assets::Tile {
            width: 2,
            height: 1,
            pixels: vec![32, 16, 8, 64, 32, 16, 8, 64],
        };
        let color = super::solid_color(&tile).unwrap();
        assert_eq!(
            (color.red(), color.green(), color.blue(), color.alpha()),
            (128, 64, 32, 64)
        );
        tile.pixels[4] = 33;
        assert!(super::solid_color(&tile).is_none());
    }

    #[test]
    fn configured_corner_radius_resizes_skin_and_participates_in_cache() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../rix-assets/tests/data/rounded-frame.svg");
        let skin = super::load(path.to_str().unwrap(), 4);
        for scale in [1., 1.5, 2.] {
            let native = super::render(&skin, 160., 100., false, scale);
            let compact = super::render_with_radius(&skin, 160., 100., false, scale, Some(3.));
            assert_eq!(
                compact,
                super::render_with_radius(&skin, 160., 100., false, scale, Some(3.))
            );
            assert_ne!(
                native, compact,
                "Radius changes must invalidate the complete frame cache"
            );
            let native = native.to_rgba8_premultiplied().unwrap();
            let compact = compact.to_rgba8_premultiplied().unwrap();
            let alpha = |pixels: &slint::SharedPixelBuffer<slint::Rgba8Pixel>, x: u32, y: u32| {
                pixels.as_bytes()[((y * pixels.width() + x) * 4 + 3) as usize]
            };
            let probe = (2. * scale) as u32;
            assert!(
                alpha(&compact, probe, 0) > alpha(&native, probe, 0),
                "A smaller corner must actually fill more of the edge"
            );
            assert_eq!(alpha(&native, 80, 50), alpha(&compact, 80, 50));
        }
    }
}
