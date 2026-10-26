use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{
                texture::{TextureBuffer, TextureRenderElement},
                Kind,
            },
            gles::{GlesRenderer, GlesTexture},
        },
    },
    input::pointer::{CursorIcon, CursorImageStatus},
    utils::{Logical, Point, Transform},
};
use std::collections::HashMap;

const PRELOAD_ICONS: &[CursorIcon] = &[
    CursorIcon::Default,
    CursorIcon::Text,
    CursorIcon::Pointer,
    CursorIcon::Crosshair,
    CursorIcon::Move,
    CursorIcon::Grab,
    CursorIcon::Grabbing,
    CursorIcon::ZoomIn,
    CursorIcon::ZoomOut,
    CursorIcon::Wait,
    CursorIcon::Progress,
    CursorIcon::NotAllowed,
    CursorIcon::EResize,
    CursorIcon::WResize,
    CursorIcon::NResize,
    CursorIcon::SResize,
    CursorIcon::ColResize,
    CursorIcon::RowResize,
];

struct Frame {
    image: TextureBuffer<GlesTexture>,
    hotspot: Point<f64, Logical>,
    size: smithay::utils::Size<i32, Logical>,
    delay: u64,
}

type AssetFrames = Vec<rix_assets::cursor::Frame>;

fn load_asset(theme: &str, icon: CursorIcon, pixels: u32) -> Option<AssetFrames> {
    let mut names = vec![icon.name()];
    names.extend_from_slice(icon.alt_names());
    rix_assets::cursor::load(theme, &names, pixels)
        .or_else(|| rix_assets::cursor::load(theme, &["default", "left_ptr"], pixels))
        .or_else(|| rix_assets::cursor::load("default", &["default", "left_ptr"], pixels))
}

fn upload_frames(
    renderer: &mut GlesRenderer,
    frames: Option<&[rix_assets::cursor::Frame]>,
    size: u32,
) -> Vec<Frame> {
    frames
        .unwrap_or_default()
        .iter()
        .filter_map(|f| {
            let ratio = size as f64 / f.nominal_size.max(1) as f64;
            TextureBuffer::from_memory(
                renderer,
                &f.pixels,
                Fourcc::Abgr8888,
                (f.width as i32, f.height as i32),
                false,
                1,
                Transform::Normal,
                None,
            )
            .ok()
            .map(|image| Frame {
                image,
                hotspot: (f.hotspot.0 as f64 * ratio, f.hotspot.1 as f64 * ratio).into(),
                size: (
                    (f.width as f64 * ratio).round() as i32,
                    (f.height as f64 * ratio).round() as i32,
                )
                    .into(),
                delay: f.delay as u64,
            })
        })
        .collect()
}

pub struct CursorRenderer {
    theme: String,
    size: u32,
    frames: HashMap<(CursorIcon, u32), Vec<Frame>>,
    assets: HashMap<(CursorIcon, u32), Option<AssetFrames>>,
}
impl CursorRenderer {
    pub fn new(theme: &rix_config::ThemeSettings) -> Self {
        Self {
            theme: theme.cursor_theme.clone(),
            size: theme.cursor_size,
            frames: Default::default(),
            assets: Default::default(),
        }
    }
    pub fn update(&mut self, theme: &rix_config::ThemeSettings) {
        if self.theme != theme.cursor_theme || self.size != theme.cursor_size {
            self.theme.clone_from(&theme.cursor_theme);
            self.size = theme.cursor_size;
            self.frames.clear();
            self.assets.clear();
        }
    }

    pub fn preload(&mut self, scale: f64) {
        let pixels = self.pixels(scale);
        for icon in PRELOAD_ICONS {
            self.assets
                .entry((*icon, pixels))
                .or_insert_with(|| load_asset(&self.theme, *icon, pixels));
        }
    }
    fn pixels(&self, scale: f64) -> u32 {
        (self.size as f64 * scale).round().clamp(1., 512.) as u32
    }
    pub fn element(
        &mut self,
        renderer: &mut GlesRenderer,
        status: &CursorImageStatus,
        location: Point<f64, Logical>,
        scale: f64,
        time: std::time::Duration,
    ) -> Option<TextureRenderElement<GlesTexture>> {
        let icon = match status {
            CursorImageStatus::Named(icon) => *icon,
            _ => CursorIcon::Default,
        };
        let pixels = self.pixels(scale);
        if !self.frames.contains_key(&(icon, pixels)) {
            let frames = {
                let asset = self
                    .assets
                    .entry((icon, pixels))
                    .or_insert_with(|| load_asset(&self.theme, icon, pixels));
                upload_frames(renderer, asset.as_deref(), self.size)
            };
            self.frames.insert((icon, pixels), frames);
        }
        let frames = &self.frames[&(icon, pixels)];
        let total: u64 = frames.iter().map(|f| f.delay).sum();
        if total == 0 {
            return None;
        }
        let mut elapsed = time.as_millis() as u64 % total;
        let frame = frames.iter().find(|f| {
            if elapsed < f.delay {
                true
            } else {
                elapsed -= f.delay;
                false
            }
        })?;
        Some(TextureRenderElement::from_texture_buffer(
            (location - frame.hotspot).to_physical(scale),
            &frame.image,
            Some(1.),
            None,
            Some(frame.size),
            Kind::Cursor,
        ))
    }
}
