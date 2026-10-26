use image::{Rgba, RgbaImage};

pub const MAX_PIXELS: u64 = 33_177_600;
const MAX_EDITS: usize = 64;
pub const MAX_POINTS: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

pub fn selection(a: Point, b: Point, view: (f32, f32), pixels: (u32, u32)) -> Option<Rect> {
    if ![a.x, a.y, b.x, b.y, view.0, view.1]
        .into_iter()
        .all(f32::is_finite)
        || view.0 <= 0.
        || view.1 <= 0.
        || pixels.0 == 0
        || pixels.1 == 0
    {
        return None;
    }

    let map = |coordinate: f32, extent: f32, pixels: u32| {
        f64::from(coordinate.clamp(0., extent)) * f64::from(pixels) / f64::from(extent)
    };
    let x = map(a.x.min(b.x), view.0, pixels.0).floor() as u32;
    let y = map(a.y.min(b.y), view.1, pixels.1).floor() as u32;
    let right = map(a.x.max(b.x), view.0, pixels.0).ceil() as u32;
    let bottom = map(a.y.max(b.y), view.1, pixels.1).ceil() as u32;
    (right > x && bottom > y).then_some(Rect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    })
}

#[derive(Clone, Debug)]
pub enum Edit {
    Pen(Vec<Point>),
    Marker(Vec<Point>),
    Arrow(Point, Point),
    Redact(Rect),
    Blur(Rect),
    Stroke {
        points: Vec<Point>,
        color: Rgba<u8>,
        width: f32,
    },
    Highlight {
        points: Vec<Point>,
        color: Rgba<u8>,
        width: f32,
    },
    StyledArrow {
        a: Point,
        b: Point,
        color: Rgba<u8>,
        width: f32,
    },
    Rectangle {
        rect: Rect,
        color: Rgba<u8>,
        width: f32,
    },
    Crop(Rect),
    Text {
        origin: Point,
        text: String,
        font: ab_glyph::FontArc,
        size: f32,
        color: Rgba<u8>,
    },
}

pub struct Editor {
    source: RgbaImage,
    edits: Vec<Edit>,
    cursor: usize,
}

impl Editor {
    pub fn new(source: RgbaImage) -> anyhow::Result<Self> {
        anyhow::ensure!(
            source.width() > 0
                && source.height() > 0
                && source.width() as u64 * source.height() as u64 <= MAX_PIXELS,
            "Image exceeds the capture limit"
        );
        Ok(Self {
            source,
            edits: Vec::new(),
            cursor: 0,
        })
    }
    pub fn dimensions(&self) -> (u32, u32) {
        self.edits[..self.cursor]
            .iter()
            .fold(self.source.dimensions(), |size, edit| {
                if let Edit::Crop(rect) = edit {
                    clip(*rect, size)
                        .map(|r| (r.width, r.height))
                        .unwrap_or(size)
                } else {
                    size
                }
            })
    }
    pub fn can_undo(&self) -> bool {
        self.cursor > 0
    }
    pub fn can_redo(&self) -> bool {
        self.cursor < self.edits.len()
    }
    pub fn apply(&mut self, edit: Edit) -> anyhow::Result<()> {
        anyhow::ensure!(self.cursor < MAX_EDITS, "The edit limit has been reached");
        if let Edit::Pen(points)
        | Edit::Marker(points)
        | Edit::Stroke { points, .. }
        | Edit::Highlight { points, .. } = &edit
        {
            anyhow::ensure!(
                !points.is_empty()
                    && points.len() <= MAX_POINTS
                    && points.iter().all(|p| p.x.is_finite() && p.y.is_finite()),
                "Invalid stroke"
            );
        }
        match &edit {
            Edit::Text {
                origin, text, size, ..
            } => anyhow::ensure!(
                origin.x.is_finite()
                    && origin.y.is_finite()
                    && !text.is_empty()
                    && text.len() <= 4096
                    && size.is_finite()
                    && *size >= 8.
                    && *size <= 256.,
                "Invalid text annotation"
            ),
            Edit::Arrow(a, b) | Edit::StyledArrow { a, b, .. } => anyhow::ensure!(
                [a.x, a.y, b.x, b.y].into_iter().all(f32::is_finite),
                "Invalid arrow"
            ),
            Edit::Crop(rect) => {
                anyhow::ensure!(clip(*rect, self.dimensions()).is_some(), "Empty crop")
            }
            _ => {}
        }
        if let Edit::Stroke { width, .. }
        | Edit::Highlight { width, .. }
        | Edit::StyledArrow { width, .. }
        | Edit::Rectangle { width, .. } = &edit
        {
            anyhow::ensure!(
                width.is_finite() && *width > 0. && *width <= 128.,
                "Invalid stroke width"
            );
        }
        self.edits.truncate(self.cursor);
        self.edits.push(edit);
        self.cursor += 1;
        Ok(())
    }
    pub fn undo(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }
    pub fn redo(&mut self) {
        self.cursor = (self.cursor + 1).min(self.edits.len());
    }
    pub fn render(&self) -> RgbaImage {
        let mut image = self.source.clone();
        for edit in &self.edits[..self.cursor] {
            match edit {
                Edit::Text {
                    origin,
                    text,
                    font,
                    size,
                    color,
                } => super::text::paint(&mut image, *origin, text, font, *size, *color),
                Edit::Crop(rect) => {
                    if let Some(r) = clip(*rect, image.dimensions()) {
                        image = image::imageops::crop_imm(&image, r.x, r.y, r.width, r.height)
                            .to_image();
                    }
                }
                Edit::Stroke {
                    points,
                    color,
                    width,
                } => stroke(&mut image, points, *width, *color),
                Edit::Highlight {
                    points,
                    color,
                    width,
                } => highlight(
                    &mut image,
                    points,
                    *width,
                    Rgba([color.0[0], color.0[1], color.0[2], 105]),
                ),
                Edit::StyledArrow { a, b, color, width } => {
                    let points = arrow_points(*a, *b);
                    line(&mut image, points[0], points[1], *width, *color);
                    line(&mut image, points[1], points[2], *width, *color);
                    line(&mut image, points[1], points[3], *width, *color);
                }
                Edit::Rectangle { rect, color, width } => {
                    if let Some(r) = clip(*rect, image.dimensions()) {
                        let left = r.x as f32;
                        let top = r.y as f32;
                        let right = (r.x + r.width).saturating_sub(1) as f32;
                        let bottom = (r.y + r.height).saturating_sub(1) as f32;
                        let points = [
                            Point { x: left, y: top },
                            Point { x: right, y: top },
                            Point {
                                x: right,
                                y: bottom,
                            },
                            Point { x: left, y: bottom },
                            Point { x: left, y: top },
                        ];
                        stroke(&mut image, &points, *width, *color);
                    }
                }
                Edit::Pen(points) => stroke(&mut image, points, 3., Rgba([255, 84, 95, 255])),
                Edit::Marker(points) => {
                    highlight(&mut image, points, 14., Rgba([255, 215, 64, 105]));
                }
                Edit::Arrow(a, b) => {
                    line(&mut image, *a, *b, 4., Rgba([255, 84, 95, 255]));
                    let angle = (b.y - a.y).atan2(b.x - a.x);
                    let length = ((b.x - a.x).hypot(b.y - a.y) * 0.25).clamp(8., 24.);
                    for delta in [-0.55, 0.55] {
                        let p = Point {
                            x: b.x - length * (angle + delta).cos(),
                            y: b.y - length * (angle + delta).sin(),
                        };
                        line(&mut image, *b, p, 4., Rgba([255, 84, 95, 255]));
                    }
                }
                Edit::Redact(rect) => {
                    if let Some(r) = clip(*rect, image.dimensions()) {
                        for y in r.y..r.y + r.height {
                            for x in r.x..r.x + r.width {
                                image.put_pixel(x, y, Rgba([24, 27, 36, 255]));
                            }
                        }
                    }
                }
                Edit::Blur(rect) => {
                    if let Some(r) = clip(*rect, image.dimensions()) {
                        let part = image::imageops::crop_imm(&image, r.x, r.y, r.width, r.height)
                            .to_image();
                        let blurred = image::imageops::blur(&part, 12.);
                        image::imageops::replace(&mut image, &blurred, r.x as i64, r.y as i64);
                    }
                }
            }
        }
        image
    }
}

fn clip(r: Rect, size: (u32, u32)) -> Option<Rect> {
    let x = r.x.min(size.0);
    let y = r.y.min(size.1);
    let right = r.x.saturating_add(r.width).min(size.0);
    let bottom = r.y.saturating_add(r.height).min(size.1);
    (right > x && bottom > y).then_some(Rect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    })
}
fn blend(pixel: &mut Rgba<u8>, color: Rgba<u8>) {
    let a = color.0[3] as u16;
    for c in 0..3 {
        pixel.0[c] = ((pixel.0[c] as u16 * (255 - a) + color.0[c] as u16 * a) / 255) as u8;
    }
    pixel.0[3] = 255;
}
fn segments(points: &[Point]) -> impl Iterator<Item = (Point, Point)> + '_ {
    points
        .iter()
        .copied()
        .zip(points.iter().skip(1).copied().chain(points.last().copied()))
}
fn stroke(image: &mut RgbaImage, points: &[Point], width: f32, color: Rgba<u8>) {
    for (a, b) in segments(points) {
        line(image, a, b, width, color);
    }
}
fn highlight(image: &mut RgbaImage, points: &[Point], width: f32, color: Rgba<u8>) {

    let mut mask = image::GrayImage::new(image.width(), image.height());
    for (a, b) in segments(points) {
        line_mask(&mut mask, a, b, width);
    }
    for (pixel, mask) in image.pixels_mut().zip(mask.pixels()) {
        if mask.0[0] != 0 {
            blend(pixel, color);
        }
    }
}

fn arrow_points(a: Point, b: Point) -> [Point; 4] {
    let angle = (b.y - a.y).atan2(b.x - a.x);
    let length = ((b.x - a.x).hypot(b.y - a.y) * 0.25).clamp(8., 24.);
    let tip = |delta: f32| Point {
        x: b.x - length * (angle + delta).cos(),
        y: b.y - length * (angle + delta).sin(),
    };
    [a, b, tip(-0.55), tip(0.55)]
}

pub fn preview_path(points: &[Point], tool: i32) -> String {
    let Some(a) = points.first().copied() else {
        return String::new();
    };
    let b = points.last().copied().unwrap_or(a);
    if tool == 3 {
        let p = arrow_points(a, b);
        return format!(
            "M {} {} L {} {} L {} {} M {} {} L {} {}",
            a.x, a.y, b.x, b.y, p[2].x, p[2].y, b.x, b.y, p[3].x, p[3].y
        );
    }
    if tool == 6 {
        return format!(
            "M {} {} L {} {} L {} {} L {} {} Z",
            a.x, a.y, b.x, a.y, b.x, b.y, a.x, b.y
        );
    }
    use std::fmt::Write;
    let mut path = format!("M {} {}", a.x, a.y);
    for p in points {
        let _ = write!(path, " L {} {}", p.x, p.y);
    }
    path
}

fn cover(size: (u32, u32), a: Point, b: Point, width: f32, mut put: impl FnMut(u32, u32)) {
    let radius = width / 2.;
    let left = (a.x.min(b.x) - radius).max(0.).min(size.0 as f32) as u32;
    let top = (a.y.min(b.y) - radius).max(0.).min(size.1 as f32) as u32;
    let right = (a.x.max(b.x) + radius + 1.).max(0.).min(size.0 as f32) as u32;
    let bottom = (a.y.max(b.y) + radius + 1.).max(0.).min(size.1 as f32) as u32;
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let length = dx * dx + dy * dy;
    for y in top..bottom {
        for x in left..right {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let t = if length > 0. {
                (((px - a.x) * dx + (py - a.y) * dy) / length).clamp(0., 1.)
            } else {
                0.
            };
            if (px - a.x - t * dx).powi(2) + (py - a.y - t * dy).powi(2) <= radius * radius {
                put(x, y);
            }
        }
    }
}
fn line(image: &mut RgbaImage, a: Point, b: Point, width: f32, color: Rgba<u8>) {
    cover(image.dimensions(), a, b, width, |x, y| {
        blend(image.get_pixel_mut(x, y), color)
    });
}
fn line_mask(image: &mut image::GrayImage, a: Point, b: Point, width: f32) {
    cover(image.dimensions(), a, b, width, |x, y| {
        image.put_pixel(x, y, image::Luma([255]))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_handles_reverse_fractional_scale_edges_and_empty_drag() {
        assert_eq!(
            selection(
                Point { x: 250., y: 210. },
                Point { x: 920., y: 610. },
                (1280., 800.),
                (1280, 800)
            ),
            Some(Rect {
                x: 250,
                y: 210,
                width: 670,
                height: 400
            }),
            "Integer native boundaries must not acquire a spurious extra row"
        );
        assert_eq!(
            selection(
                Point { x: 100.2, y: 50.2 },
                Point { x: -20., y: 10. },
                (800., 600.),
                (1000, 750)
            ),
            Some(Rect {
                x: 0,
                y: 12,
                width: 126,
                height: 51
            })
        );
        assert!(selection(
            Point { x: 0., y: 0. },
            Point { x: 0., y: 0. },
            (800., 600.),
            (1000, 750)
        )
        .is_none());
        assert!(selection(
            Point { x: f32::NAN, y: 0. },
            Point { x: 1., y: 2. },
            (10., 10.),
            (10, 10)
        )
        .is_none());
    }
    #[test]
    fn redaction_is_baked_into_export_and_undo_redo_is_exact() {
        let source = RgbaImage::from_pixel(20, 20, Rgba([250, 230, 210, 255]));
        let mut editor = Editor::new(source.clone()).unwrap();
        editor
            .apply(Edit::Redact(Rect {
                x: 3,
                y: 4,
                width: 8,
                height: 9,
            }))
            .unwrap();
        let redacted = editor.render();
        assert_eq!(*redacted.get_pixel(5, 5), Rgba([24, 27, 36, 255]));
        editor.undo();
        assert_eq!(editor.render(), source);
        editor.redo();
        assert_eq!(editor.render(), redacted);
        editor.undo();
        editor
            .apply(Edit::Arrow(
                Point { x: 1., y: 1. },
                Point { x: 15., y: 15. },
            ))
            .unwrap();
        assert!(!editor.can_redo());
        assert_ne!(editor.render(), source);
    }
    #[test]
    fn marker_intersections_do_not_accumulate_opacity_and_edit_count_is_bounded() {
        let white = RgbaImage::from_pixel(32, 32, Rgba([255, 255, 255, 255]));
        let mut editor = Editor::new(white).unwrap();
        editor
            .apply(Edit::Marker(vec![
                Point { x: 5., y: 10. },
                Point { x: 20., y: 10. },
                Point { x: 5., y: 10. },
            ]))
            .unwrap();
        let image = editor.render();
        assert_eq!(*image.get_pixel(10, 10), *image.get_pixel(15, 10));
        for _ in 1..MAX_EDITS {
            editor
                .apply(Edit::Redact(Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                }))
                .unwrap();
        }
        assert!(editor
            .apply(Edit::Pen(vec![Point { x: 0., y: 0. }]))
            .is_err());
    }
}
