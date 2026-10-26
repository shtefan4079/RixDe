use resvg::{tiny_skia, usvg};
use std::path::{Path, PathBuf};
mod icons;
pub use icons::IconResolver;

pub fn resolve(reference: &str) -> Option<PathBuf> {
    let (kind, relative) = match reference.split_once(':') {
        Some(pair) => pair,
        None => return Path::new(reference).is_file().then(|| reference.into()),
    };
    let folder = match kind {
        "plasma" => "plasma/desktoptheme",
        "aurorae" => "aurorae/themes",
        _ => return None,
    };
    if Path::new(relative)
        .components()
        .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return None;
    }
    let user = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")));
    let system =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    user.into_iter()
        .chain(system.split(':').map(PathBuf::from))
        .map(|root| root.join(folder).join(relative))
        .find(|path| path.exists())
}

#[derive(Clone)]
pub struct Tile {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}
pub const FRAME_PARTS: [&str; 9] = [
    "topleft",
    "top",
    "topright",
    "left",
    "center",
    "right",
    "bottomleft",
    "bottom",
    "bottomright",
];
pub fn load_frame(path: &Path, prefix: &str) -> anyhow::Result<Vec<Tile>> {
    load_frame_at_scale(path, prefix, 1)
}
pub fn load_frame_at_scale(path: &Path, prefix: &str, scale: u32) -> anyhow::Result<Vec<Tile>> {
    let data = std::fs::read(path)?;
    let tree = usvg::Tree::from_data(
        &data,
        &usvg::Options {
            resources_dir: path.parent().map(Path::to_path_buf),
            ..Default::default()
        },
    )?;
    FRAME_PARTS
        .iter()
        .map(|part| {
            let id = format!("{prefix}{part}");
            if *part != "center" && find_node(tree.root(), &id, 1.).is_none() {

                Ok(Tile {
                    width: 0,
                    height: 0,
                    pixels: Vec::new(),
                })
            } else {
                render_part_at_scale(&tree, &id, scale)
            }
        })
        .collect()
}

pub fn frame_margins(path: &Path) -> anyhow::Result<[f32; 4]> {
    let tree = usvg::Tree::from_data(&std::fs::read(path)?, &usvg::Options::default())?;
    let mut margins = [0.; 4];
    for (index, side) in ["left", "top", "right", "bottom"].iter().enumerate() {
        let node = find_node(tree.root(), &format!("hint-{side}-margin"), 1.)
            .or_else(|| find_node(tree.root(), side, 1.));
        if let Some((node, _, transform)) = node {
            let Some(bounds) = painted_bounds(node, transform) else {
                continue;
            };
            margins[index] = if index % 2 == 0 {
                bounds.width()
            } else {
                bounds.height()
            };
        }
    }
    Ok(margins)
}
fn find_node<'a>(
    group: &'a usvg::Group,
    id: &str,
    opacity: f32,
) -> Option<(&'a usvg::Node, f32, tiny_skia::Transform)> {
    find_node_with_transform(group, id, opacity, tiny_skia::Transform::identity())
}
fn find_node_with_transform<'a>(
    group: &'a usvg::Group,
    id: &str,
    opacity: f32,
    parent: tiny_skia::Transform,
) -> Option<(&'a usvg::Node, f32, tiny_skia::Transform)> {
    let transform = parent.pre_concat(group.transform());
    for node in group.children() {
        if node.id() == id {
            return Some((node, opacity * group.opacity().get(), transform));
        }
        if let usvg::Node::Group(child) = node {
            if let Some(result) =
                find_node_with_transform(child, id, opacity * group.opacity().get(), transform)
            {
                return Some(result);
            }
        }
    }
    None
}
pub fn render_part(tree: &usvg::Tree, id: &str) -> anyhow::Result<Tile> {
    render_part_at_scale(tree, id, 1)
}
fn painted_bounds(node: &usvg::Node, parent: tiny_skia::Transform) -> Option<tiny_skia::Rect> {
    if let usvg::Node::Group(group) = node {
        let transform = parent.pre_concat(group.transform());
        if !group.filters().is_empty() {
            return group
                .layer_bounding_box()
                .transform(transform)
                .map(|bounds| bounds.to_rect());
        }

        group
            .children()
            .iter()
            .filter_map(|child| painted_bounds(child, transform))
            .reduce(|a, b| a.join(&b).unwrap_or(a))
    } else {
        node.stroke_bounding_box().transform(parent)
    }
}
fn render_part_at_scale(tree: &usvg::Tree, id: &str, scale: u32) -> anyhow::Result<Tile> {
    anyhow::ensure!(
        (1..=4).contains(&scale),
        "SVG frame scale must be between 1 and 4"
    );
    let (node, opacity, parent_transform) = find_node(tree.root(), id, 1.)
        .ok_or_else(|| anyhow::anyhow!("SVG element '{id}' missing"))?;
    let cached_bounds = node
        .abs_layer_bounding_box()
        .ok_or_else(|| anyhow::anyhow!("SVG element '{id}' is empty"))?;

    let bounds = painted_bounds(node, parent_transform)
        .and_then(|bounds| bounds.to_non_zero_rect())
        .ok_or_else(|| anyhow::anyhow!("SVG element '{id}' has invalid bounds"))?;
    let raster_extent = |value: f32| ((value * scale as f32 - 0.0001).ceil().max(1.)) as u32;
    let width = raster_extent(bounds.width());
    let height = raster_extent(bounds.height());
    anyhow::ensure!(
        width <= 4096 && height <= 4096,
        "SVG tile exceeds texture limit"
    );
    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| anyhow::anyhow!("Could not allocate SVG tile"))?;

    resvg::render_node(
        node,
        tiny_skia::Transform::from_scale(
            width as f32 / bounds.width(),
            height as f32 / bounds.height(),
        )
        .pre_translate(-bounds.x(), -bounds.y())
        .pre_concat(parent_transform)

        .pre_translate(cached_bounds.x(), cached_bounds.y()),
        &mut pixmap.as_mut(),
    );
    let mut pixels = pixmap.take();
    for channel in &mut pixels {
        *channel = (*channel as f32 * opacity).round() as u8;
    }
    Ok(Tile {
        width,
        height,
        pixels,
    })
}

pub fn load_part(path: &Path, id: &str) -> anyhow::Result<Tile> {
    load_part_at_scale(path, id, 1)
}
pub fn load_part_at_scale(path: &Path, id: &str, scale: u32) -> anyhow::Result<Tile> {
    let tree = usvg::Tree::from_data(&std::fs::read(path)?, &usvg::Options::default())?;
    render_part_at_scale(&tree, id, scale)
}

pub fn compose_frame(
    tiles: &[Tile],
    width: u32,
    height: u32,
    borders: [u32; 4],
) -> anyhow::Result<tiny_skia::Pixmap> {
    let mut canvas = tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| anyhow::anyhow!("Invalid frame size"))?;
    let [left, top, right, bottom] = borders;
    let xs = [0, left, width.saturating_sub(right), width];
    let ys = [0, top, height.saturating_sub(bottom), height];
    for (index, tile) in tiles.iter().enumerate() {
        let x = index % 3;
        let y = index / 3;
        blit(
            &mut canvas,
            tile,
            xs[x],
            ys[y],
            xs[x + 1].saturating_sub(xs[x]),
            ys[y + 1].saturating_sub(ys[y]),
        );
    }
    Ok(canvas)
}

pub fn compose_frame_filtered(
    tiles: &[Tile],
    width: u32,
    height: u32,
    borders: [u32; 4],
) -> anyhow::Result<Tile> {
    anyhow::ensure!(
        width > 0 && height > 0 && width <= 8192 && height <= 8192,
        "Invalid frame size"
    );
    let [left, top, right, bottom] = borders;
    anyhow::ensure!(
        left + right <= width && top + bottom <= height && tiles.len() == 9,
        "Invalid frame borders"
    );
    let xs = [0, left, width - right, width];
    let ys = [0, top, height - bottom, height];
    let mut pixels = vec![0; width as usize * height as usize * 4];
    for (i, tile) in tiles.iter().enumerate() {
        let x = i % 3;
        let y = i / 3;
        let w = xs[x + 1] - xs[x];
        let h = ys[y + 1] - ys[y];
        if w == 0 || h == 0 || tile.width == 0 || tile.height == 0 {
            continue;
        }
        let pixel = tile
            .pixels
            .get(..4)
            .ok_or_else(|| anyhow::anyhow!("Empty frame tile"))?;
        if tile.pixels.chunks_exact(4).all(|p| p == pixel) {
            for row in ys[y]..ys[y + 1] {
                let start = (row as usize * width as usize + xs[x] as usize) * 4;
                for p in pixels[start..start + w as usize * 4].chunks_exact_mut(4) {
                    p.copy_from_slice(pixel);
                }
            }
            continue;
        }
        let source = image::RgbaImage::from_raw(tile.width, tile.height, tile.pixels.clone())
            .ok_or_else(|| anyhow::anyhow!("Invalid frame tile"))?;

        let mut scaled =
            image::imageops::resize(&source, w, h, image::imageops::FilterType::Lanczos3);
        for p in scaled.pixels_mut() {
            for channel in 0..3 {
                p.0[channel] = p.0[channel].min(p.0[3]);
            }
        }
        for row in 0..h {
            let dst = ((ys[y] + row) as usize * width as usize + xs[x] as usize) * 4;
            let src = row as usize * w as usize * 4;
            pixels[dst..dst + w as usize * 4]
                .copy_from_slice(&scaled.as_raw()[src..src + w as usize * 4]);
        }
    }
    Ok(Tile {
        width,
        height,
        pixels,
    })
}

pub fn blit(canvas: &mut tiny_skia::Pixmap, tile: &Tile, x: u32, y: u32, width: u32, height: u32) {
    if width == 0 || height == 0 {
        return;
    }
    if let Some(pixmap) = tiny_skia::PixmapRef::from_bytes(&tile.pixels, tile.width, tile.height) {
        canvas.draw_pixmap(
            0,
            0,
            pixmap,
            &tiny_skia::PixmapPaint {
                quality: tiny_skia::FilterQuality::Bilinear,
                ..Default::default()
            },
            tiny_skia::Transform::from_scale(
                width as f32 / tile.width as f32,
                height as f32 / tile.height as f32,
            )
            .post_translate(x as f32, y as f32),
            None,
        );
    }
}
pub use resvg::{tiny_skia as raster, usvg as svg};

pub fn render_svg(data: &str, options: &usvg::Options) -> anyhow::Result<Tile> {
    let tree = usvg::Tree::from_data(data.as_bytes(), options)?;
    let size = tree.size().to_int_size();
    let mut pixmap = tiny_skia::Pixmap::new(size.width(), size.height())
        .ok_or_else(|| anyhow::anyhow!("Invalid text size"))?;
    resvg::render(
        &tree,
        tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    Ok(Tile {
        width: size.width(),
        height: size.height(),
        pixels: pixmap.take(),
    })
}

pub fn rasterize_icon(path: &Path, size: u32) -> anyhow::Result<Tile> {
    anyhow::ensure!((1..=512).contains(&size), "Invalid icon size");
    if !matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("svg" | "svgz")
    ) {
        let mut reader = image::ImageReader::open(path)?.with_guessed_format()?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(4096);
        limits.max_image_height = Some(4096);
        limits.max_alloc = Some(64 * 1024 * 1024);
        reader.limits(limits);
        let mut image = reader.decode()?.to_rgba8();
        for pixel in image.pixels_mut() {
            for channel in 0..3 {
                pixel.0[channel] =
                    ((pixel.0[channel] as u16 * pixel.0[3] as u16 + 127) / 255) as u8;
            }
        }
        return resize_icon(
            Tile {
                width: image.width(),
                height: image.height(),
                pixels: image.into_raw(),
            },
            size,
        );
    }
    let tree = usvg::Tree::from_data(&std::fs::read(path)?, &usvg::Options::default())?;
    let mut canvas =
        tiny_skia::Pixmap::new(size, size).ok_or_else(|| anyhow::anyhow!("Invalid icon size"))?;
    let scale = (size as f32 / tree.size().width()).min(size as f32 / tree.size().height());
    let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(
        (size as f32 - tree.size().width() * scale) / 2.,
        (size as f32 - tree.size().height() * scale) / 2.,
    );
    resvg::render(&tree, transform, &mut canvas.as_mut());
    Ok(Tile {
        width: size,
        height: size,
        pixels: canvas.take(),
    })
}

pub fn resize_icon(tile: Tile, size: u32) -> anyhow::Result<Tile> {
    anyhow::ensure!((1..=512).contains(&size), "Invalid icon size");
    let image = image::RgbaImage::from_raw(tile.width, tile.height, tile.pixels)
        .ok_or_else(|| anyhow::anyhow!("Invalid icon pixels"))?;
    let mut image = image::DynamicImage::ImageRgba8(image)
        .resize(size, size, image::imageops::FilterType::Lanczos3)
        .to_rgba8();

    for pixel in image.pixels_mut() {
        for channel in 0..3 {
            pixel.0[channel] = pixel.0[channel].min(pixel.0[3]);
        }
    }
    Ok(Tile {
        width: image.width(),
        height: image.height(),
        pixels: image.into_raw(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn icon_downsampling_averages_detail_instead_of_aliasing() {
        let mut pixels = Vec::new();
        for y in 0..512 {
            for x in 0..512 {
                let v = if (x + y) % 2 == 0 { 255 } else { 0 };
                pixels.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let tile = resize_icon(
            Tile {
                width: 512,
                height: 512,
                pixels,
            },
            22,
        )
        .unwrap();
        assert_eq!((tile.width, tile.height), (22, 22));
        assert!(tile
            .pixels
            .chunks_exact(4)
            .all(|p| (120..=136).contains(&p[0]) && p[3] == 255));
    }
    #[test]
    fn preserves_ancestor_opacity_in_extracted_svg_parts() {
        let svg=br#"<svg xmlns='http://www.w3.org/2000/svg' width='20' height='20'><g opacity='0.3' transform='translate(50,70) scale(2)'><g id='center' transform='translate(10,20)'><rect width='5' height='5' fill='red'/></g></g></svg>"#;
        let tree = usvg::Tree::from_data(svg, &usvg::Options::default()).unwrap();
        let tile = render_part(&tree, "center").unwrap();
        assert_eq!((tile.width, tile.height), (10, 10));
        assert!(tile
            .pixels
            .chunks_exact(4)
            .all(|pixel| (76..=77).contains(&pixel[3]) && pixel[0] == pixel[3]));
    }
    #[test]
    fn extracts_scaled_use_outside_the_svg_viewport() {
        let svg = br##"<svg xmlns='http://www.w3.org/2000/svg' xmlns:xlink='http://www.w3.org/1999/xlink' width='12' height='118'><defs><g id='base' transform='translate(-23,-51)'><rect x='19' y='-115' width='52' height='44' fill='white'/></g></defs><g transform='translate(-24,-2)' opacity='0.5'><use id='center' xlink:href='#base' transform='matrix(.25,0,0,.25,25,66.5)'/></g></svg>"##;
        let tree = usvg::Tree::from_data(svg, &usvg::Options::default()).unwrap();
        let tile = render_part(&tree, "center").unwrap();
        assert_eq!((tile.width, tile.height), (13, 11));
        assert!(tile
            .pixels
            .chunks_exact(4)
            .all(|p| (127..=128).contains(&p[3])));
        let sharper = render_part_at_scale(&tree, "center", 4).unwrap();
        assert_eq!((sharper.width, sharper.height), (52, 44));
    }
    #[test]
    fn empty_editor_groups_do_not_expand_frame_edges() {
        let svg = br#"<svg xmlns='http://www.w3.org/2000/svg' width='30' height='30'><g id='bottom'><rect x='9' y='14' width='2' height='4' fill='white'/><g id='empty'/></g></svg>"#;
        let tree = usvg::Tree::from_data(svg, &usvg::Options::default()).unwrap();
        let tile = render_part(&tree, "bottom").unwrap();
        assert_eq!((tile.width, tile.height), (2, 4));
        assert!(tile.pixels.chunks_exact(4).all(|p| p[3] == 255));
    }
    #[test]
    fn theme_references_cannot_escape_data_directories() {
        assert!(resolve("plasma:../../etc/passwd").is_none());
        assert!(resolve("aurorae:/etc/passwd").is_none());
    }
}
pub mod cursor;
