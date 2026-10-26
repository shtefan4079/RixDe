use crate::state::RixState;
use rix_assets::{raster, svg, Tile};
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
    desktop::Window,
    utils::{Logical, Point, Rectangle, Size},
    wayland::shell::xdg::{decoration::XdgDecorationHandler, ToplevelSurface},
};
use std::{collections::HashMap, sync::Arc};
use wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
use wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge;

#[derive(Clone)]
pub struct DecorationTheme {
    pub height: i32,
    pub left: i32,
    pub right: i32,
    pub bottom: i32,
    button_size: i32,
    spacing: i32,
    padding: i32,
    active: Option<Arc<Vec<Tile>>>,
    inactive: Option<Arc<Vec<Tile>>>,
    buttons: Arc<[[Option<Tile>; 4]; 2]>,
    button_hover: Arc<[Option<Tile>; 4]>,
    button_pressed: Arc<[Option<Tile>; 4]>,
    shadow: [i32; 4],
    raster_scale: u32,
    square: bool,
    hover: String,
    pressed: String,
    close_hover: String,
    background: rix_core::Color,
    foreground: [String; 2],
    font: String,
    font_size: f32,
    radius: f32,
    material: bool,
    opaque: bool,
    material_opacity: f32,
    material_colors: [rix_core::Color; 2],
    effects: rix_config::appearance::MaterialEffects,
    pub animation_duration: u32,
}
impl DecorationTheme {
    pub fn corner_radius(&self) -> f32 {
        self.radius
    }
    pub fn client_corner_radius(&self) -> f64 {
        (self.radius - self.left.max(self.right).max(self.bottom) as f32).max(0.) as f64
    }

    pub fn load(theme: &rix_config::ThemeSettings) -> Self {
        let a = &theme.appearance;
        let directory = rix_assets::resolve(&a.decoration_theme);
        let metric = |key: &str| a.metric(key).round().max(0.) as i32;
        let raster_scale = a.metric("frame_raster_scale").clamp(1., 4.) as u32;
        let options = directory.as_ref().and_then(|d| aurorae_options(d));
        let shadow = options
            .as_deref()
            .map(aurorae_padding_from_options)
            .unwrap_or_default();
        let foreground = a
            .palette
            .get("text_primary")
            .cloned()
            .unwrap_or_else(|| "#ffffff".into());
        Self {
            shadow,
            raster_scale,
            square: false,
            radius: a.metric("radius_md"),
            animation_duration: a.metric("hover_duration").clamp(0., 1000.) as u32,
            height: metric("decoration_title_height").max(1),
            left: metric("decoration_border_left"),
            right: metric("decoration_border_right"),
            bottom: metric("decoration_border_bottom"),
            button_size: metric("decoration_button_size").max(1),
            spacing: metric("decoration_button_spacing"),
            padding: metric("decoration_title_padding"),
            active: directory.as_ref().and_then(|d| {
                rix_assets::load_frame_at_scale(
                    &d.join("decoration.svg"),
                    "decoration-",
                    raster_scale,
                )
                .ok()
                .map(Arc::new)
            }),
            inactive: directory.as_ref().and_then(|d| {
                rix_assets::load_frame_at_scale(
                    &d.join("decoration.svg"),
                    "decoration-inactive-",
                    raster_scale,
                )
                .ok()
                .map(Arc::new)
            }),
            buttons: Arc::new(["active-center", "inactive-center"].map(|state| {
                ["minimize", "maximize", "close", "restore"].map(|name| {
                    directory.as_ref().and_then(|d| {
                        rix_assets::load_part_at_scale(
                            &d.join(format!("{name}.svg")),
                            state,
                            raster_scale,
                        )
                        .ok()
                    })
                })
            })),
            button_hover: Arc::new(["minimize", "maximize", "close", "restore"].map(|name| {
                directory.as_ref().and_then(|d| {
                    rix_assets::load_part_at_scale(
                        &d.join(format!("{name}.svg")),
                        "hover-center",
                        raster_scale,
                    )
                    .ok()
                })
            })),
            button_pressed: Arc::new(["minimize", "maximize", "close", "restore"].map(|name| {
                directory.as_ref().and_then(|d| {
                    rix_assets::load_part_at_scale(
                        &d.join(format!("{name}.svg")),
                        "pressed-center",
                        raster_scale,
                    )
                    .ok()
                })
            })),
            hover: a
                .palette
                .get("surface_hover")
                .cloned()
                .unwrap_or_else(|| "#ffffff20".into()),
            pressed: a
                .palette
                .get("surface_active")
                .cloned()
                .unwrap_or_else(|| "#ffffff38".into()),
            close_hover: a
                .palette
                .get("danger_bg")
                .cloned()
                .unwrap_or_else(|| "#e35b6680".into()),
            background: a
                .palette
                .get("background")
                .and_then(|v| rix_core::Color::from_hex(v))
                .unwrap_or_default(),
            foreground: ["ActiveTextColor", "InactiveTextColor"].map(|name| {
                options
                    .as_deref()
                    .and_then(|text| aurorae_color(text, name))
                    .unwrap_or_else(|| foreground.clone())
            }),
            font: a.font_family.clone(),
            font_size: a.metric("font_size_md"),
            material: false,
            opaque: false,
            material_opacity: 1.,
            material_colors: [a.color("background"); 2],
            effects: theme.into(),
        }
    }

    pub fn load_with_material(settings: &rix_config::ThemeSettings, material: bool) -> Self {
        let mut theme = Self::load(settings);
        theme.opaque = !material;
        if material {
            let a = &settings.appearance;
            let (reference, prefix) = a
                .panel_frame
                .split_once('#')
                .unwrap_or((&a.panel_frame, ""));
            let frame = rix_assets::resolve(reference).and_then(|path| {
                rix_assets::load_frame_at_scale(&path, prefix, theme.raster_scale)
                    .ok()
                    .map(Arc::new)
            });
            theme.active = frame.clone();
            theme.inactive = frame;

            theme.shadow = [0; 4];
            theme.material = true;
            theme.material_opacity = settings.opacity;
            theme.material_colors = if settings.blur_radius > 0 {
                [a.color("glass_start"), a.color("glass_end")]
            } else {
                [a.color("background"); 2]
            };
        }
        theme
    }
    pub fn material_effects(&self) -> rix_config::appearance::MaterialEffects {
        self.effects
    }
    pub fn for_window(&self, window: &Window) -> Self {
        self.with_maximized(window.toplevel().is_some_and(|t| {
            t.current_state()
                .states
                .contains(wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized)
        }))
    }
    pub(crate) fn with_maximized(&self, maximized: bool) -> Self {
        let mut theme = self.clone();
        if maximized {
            theme.left = 0;
            theme.right = 0;
            theme.bottom = 0;
            theme.radius = 0.;
            theme.square = true;
        }
        theme
    }
    pub fn bounds(&self, body: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
        Rectangle::new(
            (body.loc.x - self.left, body.loc.y - self.height).into(),
            (
                body.size.w + self.left + self.right,
                body.size.h + self.height + self.bottom,
            )
                .into(),
        )
    }
    fn frame_padding(&self) -> [i32; 4] {
        if self.square {
            [0; 4]
        } else {
            self.shadow
        }
    }
    pub fn render_bounds(&self, body: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
        let mut bounds = self.bounds(body);
        let [left, top, right, bottom] = self.frame_padding();
        bounds.loc -= Point::from((left, top));
        bounds.size.w += left + right;
        bounds.size.h += top + bottom;
        bounds
    }
    fn painted_button_rect(&self, width: i32, index: usize) -> Rectangle<i32, Logical> {
        let [left, top, right, _] = self.frame_padding();
        let mut rect = self.button_rect(width - left - right, index);
        rect.loc += Point::from((left, top));
        rect
    }
    pub(crate) fn button_rect(&self, width: i32, index: usize) -> Rectangle<i32, Logical> {
        Rectangle::new(
            (
                width
                    - self.padding
                    - self.button_size
                    - (2 - index) as i32 * (self.button_size + self.spacing),
                (self.height - self.button_size) / 2,
            )
                .into(),
            (self.button_size, self.button_size).into(),
        )
    }
    pub fn hit(
        &self,
        body: Rectangle<i32, Logical>,
        position: Point<f64, Logical>,
    ) -> Option<DecorationHit> {
        let outer = self.bounds(body);
        if !outer.to_f64().contains(position) {
            return None;
        }
        if body.to_f64().contains(position) {
            let clip = crate::render::rounded::ClientClip {
                bounds: body.to_f64(),
                radius: self.client_corner_radius(),
            };
            return (!clip.contains(position)).then_some(DecorationHit::Resize(
                if position.x < (body.loc.x + body.size.w / 2) as f64 {
                    ResizeEdge::BottomLeft
                } else {
                    ResizeEdge::BottomRight
                },
            ));
        }
        let local = position - outer.loc.to_f64();
        let left = local.x < self.left as f64;
        let right = local.x >= (outer.size.w - self.right) as f64;
        let top = local.y < self.left.max(self.right).max(self.bottom) as f64;
        let bottom = local.y >= (outer.size.h - self.bottom) as f64;
        let edge = match (left, right, top, bottom) {
            (true, _, true, _) => Some(ResizeEdge::TopLeft),
            (_, true, true, _) => Some(ResizeEdge::TopRight),
            (true, _, _, true) => Some(ResizeEdge::BottomLeft),
            (_, true, _, true) => Some(ResizeEdge::BottomRight),
            (true, _, _, _) => Some(ResizeEdge::Left),
            (_, true, _, _) => Some(ResizeEdge::Right),
            (_, _, true, _) => Some(ResizeEdge::Top),
            (_, _, _, true) => Some(ResizeEdge::Bottom),
            _ => None,
        };
        if let Some(edge) = edge {
            return Some(DecorationHit::Resize(edge));
        }
        if local.y < self.height as f64 {
            for (index, hit) in [
                DecorationHit::Minimize,
                DecorationHit::Maximize,
                DecorationHit::Close,
            ]
            .into_iter()
            .enumerate()
            {
                if self
                    .button_rect(outer.size.w, index)
                    .to_f64()
                    .contains(local)
                {
                    return Some(hit);
                }
            }
            Some(DecorationHit::Move)
        } else {
            None
        }
    }

    pub fn backdrop(&self, body: Rectangle<i32, Logical>) -> Option<DecorationBackdrop> {
        self.material.then(|| DecorationBackdrop {
            bounds: self.bounds(body),
            body,
            radius: self.radius.max(0.) as f64,
            client_radius: self.client_corner_radius(),
            alpha: 1.,
            effects: self.effects,
        })
    }
}

#[derive(Clone)]
pub struct DecorationBackdrop {
    pub bounds: Rectangle<i32, Logical>,
    pub body: Rectangle<i32, Logical>,
    pub radius: f64,
    pub client_radius: f64,
    pub alpha: f64,
    pub effects: rix_config::appearance::MaterialEffects,
}
impl DecorationBackdrop {
    pub fn transformed(mut self, center: Point<i32, Logical>, factor: f64, alpha: f64) -> Self {
        let transform = |rect: Rectangle<i32, Logical>| {
            Rectangle::new(
                Point::from((
                    (rect.loc.x - center.x) as f64 * factor + center.x as f64,
                    (rect.loc.y - center.y) as f64 * factor + center.y as f64,
                ))
                .to_i32_round(),
                (rect.size.to_f64() * factor).to_i32_round(),
            )
        };
        self.bounds = transform(self.bounds);
        self.body = transform(self.body);
        self.radius *= factor;
        self.client_radius *= factor;
        self.alpha *= alpha;
        self
    }
    pub fn shapes(&self) -> Vec<crate::surface_effects::Shape> {

        vec![crate::surface_effects::Shape {
            x: 0.,
            y: 0.,
            width: self.bounds.size.w as f64,
            height: self.bounds.size.h as f64,
            radius: self.radius,
            opacity: self.alpha,
            clip: None,
        }]
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecorationHit {
    Move,
    Minimize,
    Maximize,
    Close,
    Resize(ResizeEdge),
}
impl DecorationHit {
    pub fn button_index(self) -> Option<usize> {
        match self {
            Self::Minimize => Some(0),
            Self::Maximize => Some(1),
            Self::Close => Some(2),
            _ => None,
        }
    }
}
pub fn enabled(window: &Window) -> bool {
    window.toplevel().is_some_and(|t| {
        t.current_state().decoration_mode == Some(Mode::ServerSide)
            && !t
                .current_state()
                .states
                .contains(wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen)
    })
}

#[derive(Clone)]
pub struct DecorationPart {
    pub texture: TextureBuffer<GlesTexture>,
    pub source: Option<Rectangle<f64, Logical>>,
    pub bounds: Rectangle<i32, Logical>,
    pub alpha: f32,
}
impl DecorationPart {
    pub fn element(
        &self,
        origin: Point<i32, Logical>,
        scale: f64,
        alpha: f32,
    ) -> TextureRenderElement<GlesTexture> {
        TextureRenderElement::from_texture_buffer(
            (self.bounds.loc - origin).to_f64().to_physical(scale),
            &self.texture,
            Some(self.alpha * alpha),
            self.source,
            Some(self.bounds.size),
            Kind::Unspecified,
        )
    }
}

fn frame_slices(size: Size<i32, Logical>, borders: [i32; 4]) -> [Rectangle<i32, Logical>; 9] {
    let [mut l, mut t, mut r, mut b] = borders;
    if l + r > size.w {
        l = size.w * l / (l + r);
        r = size.w - l;
    }
    if t + b > size.h {
        t = size.h * t / (t + b);
        b = size.h - t;
    }
    let xs = [0, l, size.w - r, size.w];
    let ys = [0, t, size.h - b, size.h];
    std::array::from_fn(|i| {
        Rectangle::new(
            (xs[i % 3], ys[i / 3]).into(),
            (xs[i % 3 + 1] - xs[i % 3], ys[i / 3 + 1] - ys[i / 3]).into(),
        )
    })
}
fn frame_borders(theme: &DecorationTheme) -> [i32; 4] {
    let [l, t, r, b] = theme.frame_padding();
    let mut borders = [
        l + theme.left.max(theme.radius.ceil() as i32),
        t + theme.height,
        r + theme.right.max(theme.radius.ceil() as i32),
        b + theme.bottom.max(theme.radius.ceil() as i32),
    ];

    if !theme.square && !theme.material {
        for tiles in theme.active.iter().chain(theme.inactive.iter()) {
            for (border, extent) in borders.iter_mut().zip([
                tiles[3].width,
                tiles[1].height,
                tiles[5].width,
                tiles[7].height,
            ]) {
                *border = (*border).max(extent.div_ceil(theme.raster_scale) as i32);
            }
        }
    }
    borders
}
fn frame_size(theme: &DecorationTheme) -> Size<i32, Logical> {
    let [l, t, r, b] = frame_borders(theme);
    (l + r + 1, t + b + 1).into()
}
struct RasterAssets {
    frame: Tile,
    title: Option<Tile>,
    buttons: [Option<Tile>; 3],
    button_hover: [Option<Tile>; 3],
    button_pressed: [Option<Tile>; 3],
}
#[derive(Clone)]
struct Assets {
    frame: TextureBuffer<GlesTexture>,
    title: Option<(TextureBuffer<GlesTexture>, Size<i32, Logical>)>,
    buttons: [Option<TextureBuffer<GlesTexture>>; 3],
    button_hover: [Option<TextureBuffer<GlesTexture>>; 3],
    button_pressed: [Option<TextureBuffer<GlesTexture>>; 3],
}
impl Assets {
    fn upload(
        renderer: &mut GlesRenderer,
        raster: RasterAssets,
        scale: i32,
    ) -> anyhow::Result<Self> {
        let upload = |renderer: &mut GlesRenderer,
                      tile: Tile|
         -> anyhow::Result<TextureBuffer<GlesTexture>> {
            Ok(TextureBuffer::from_memory(
                renderer,
                &tile.pixels,
                Fourcc::Abgr8888,
                (tile.width as i32, tile.height as i32),
                false,
                scale,
                smithay::utils::Transform::Normal,
                None,
            )?)
        };
        let frame = upload(renderer, raster.frame)?;
        let title = raster
            .title
            .map(|tile| {
                let size = (tile.width as i32 / scale, tile.height as i32 / scale).into();
                upload(renderer, tile).map(|texture| (texture, size))
            })
            .transpose()?;
        let mut buttons = [None, None, None];
        for (slot, tile) in buttons.iter_mut().zip(raster.buttons) {
            *slot = tile.map(|t| upload(renderer, t)).transpose()?;
        }
        let mut button_hover = [None, None, None];
        for (slot, tile) in button_hover.iter_mut().zip(raster.button_hover) {
            *slot = tile.map(|t| upload(renderer, t)).transpose()?;
        }
        let mut button_pressed = [None, None, None];
        for (slot, tile) in button_pressed.iter_mut().zip(raster.button_pressed) {
            *slot = tile.map(|t| upload(renderer, t)).transpose()?;
        }
        Ok(Self {
            frame,
            title,
            buttons,
            button_hover,
            button_pressed,
        })
    }
    fn parts(
        &self,
        theme: &DecorationTheme,
        bounds: Rectangle<i32, Logical>,
        alpha: f32,
        hover: [f32; 3],
        pressed: Option<usize>,
    ) -> Vec<DecorationPart> {
        let mut parts = Vec::new();
        for (source, destination) in frame_slices(frame_size(theme), frame_borders(theme))
            .into_iter()
            .zip(frame_slices(bounds.size, frame_borders(theme)))
        {
            if source.size.w > 0
                && source.size.h > 0
                && destination.size.w > 0
                && destination.size.h > 0
            {
                parts.push(DecorationPart {
                    texture: self.frame.clone(),
                    source: Some(source.to_f64()),
                    bounds: Rectangle::new(bounds.loc + destination.loc, destination.size),
                    alpha,
                });
            }
        }
        for (index, texture) in self.buttons.iter().enumerate() {
            if let Some(texture) = texture {
                let rect = theme.painted_button_rect(bounds.size.w, index);
                let pressed_texture = pressed
                    .filter(|button| *button == index)
                    .and(self.button_pressed[index].as_ref());
                let alternate = pressed_texture.or(self.button_hover[index].as_ref());

                let alternate =
                    alternate.filter(|_| pressed_texture.is_some() || hover[index] > 0.5);
                parts.insert(
                    0,
                    DecorationPart {
                        texture: alternate.unwrap_or(texture).clone(),
                        source: None,
                        bounds: Rectangle::new(bounds.loc + rect.loc, rect.size),
                        alpha,
                    },
                );
            }
        }
        if let Some((texture, size)) = &self.title {
            let [left, top, _, _] = theme.frame_padding();
            let available =
                (theme.painted_button_rect(bounds.size.w, 0).loc.x - left - theme.padding * 2)
                    .max(0);
            let width = size.w.min(available);
            if width > 0 {
                parts.insert(
                    0,
                    DecorationPart {
                        texture: texture.clone(),
                        source: Some(Rectangle::new(
                            ((size.w - width) as f64 / 2., 0.).into(),
                            (width as f64, size.h as f64).into(),
                        )),
                        bounds: Rectangle::new(
                            (
                                bounds.loc.x + left + theme.padding + (available - width) / 2,
                                bounds.loc.y + top,
                            )
                                .into(),
                            (width, size.h).into(),
                        ),
                        alpha,
                    },
                );
            }
        }
        parts
    }
}

#[derive(Clone, PartialEq)]
struct Key {
    width: i32,
    height: i32,
    active: bool,
    title: String,
    scale: i32,
    generation: u64,
    appearance: String,
    square: bool,
}
impl Key {
    fn same_frame(&self, other: &Self) -> bool {
        self.height == other.height
            && self.scale == other.scale
            && self.generation == other.generation
            && self.appearance == other.appearance
            && self.square == other.square
    }
    fn same_caption(&self, other: &Self) -> bool {
        self.title == other.title
            && self.scale == other.scale
            && self.active == other.active
            && self.generation == other.generation
            && self.appearance == other.appearance
    }
}
struct Cached {
    key: Key,
    assets: Assets,
    previous: Option<Assets>,
    complete: bool,
    changed: std::time::Instant,
    hover: [f32; 3],
    hover_tick: std::time::Instant,
}
#[derive(Clone, Hash, PartialEq, Eq)]
struct PlaceholderKey {
    generation: u64,
    appearance: String,
    scale: i32,
    active: bool,
    square: bool,
}
#[derive(Default)]
pub struct DecorationRenderer {
    generation: Option<u64>,
    cache: HashMap<u64, Cached>,
    placeholders: HashMap<PlaceholderKey, Assets>,
    worker: Option<RasterWorker>,
    pending: HashMap<u64, Key>,
    painted: HashMap<u64, Vec<DecorationPart>>,
    backdrops: HashMap<u64, DecorationBackdrop>,
}
impl DecorationRenderer {
    pub fn snapshot(&self, id: u64) -> Vec<DecorationPart> {
        self.painted.get(&id).cloned().unwrap_or_default()
    }
    pub fn backdrop_snapshot(&self, id: u64) -> Option<DecorationBackdrop> {
        self.backdrops.get(&id).cloned()
    }
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        state: &RixState,
        output: &smithay::output::Output,
    ) -> HashMap<smithay::backend::renderer::element::Id, Vec<TextureRenderElement<GlesTexture>>>
    {
        let mut result = HashMap::new();
        if self.generation != Some(state.decoration_generation) {
            self.generation = Some(state.decoration_generation);
            self.cache.clear();
            self.pending.clear();
            self.placeholders.clear();
            self.painted.clear();
            self.backdrops.clear();
        }
        if self.worker.is_none() {
            self.worker = RasterWorker::start().ok();
        }
        loop {
            let ready = self.worker.as_ref().and_then(|w| w.results.try_recv().ok());
            let Some((id, key, tile)) = ready else { break };
            if key.generation != state.decoration_generation || self.pending.get(&id) != Some(&key)
            {
                continue;
            }
            match tile.and_then(|tile| Assets::upload(renderer, tile, key.scale)) {
                Ok(assets) => {
                    self.pending.remove(&id);
                    let previous = self
                        .cache
                        .remove(&id)
                        .filter(|old| {
                            old.key.width == key.width
                                && old.key.height == key.height
                                && old.key.scale == key.scale
                                && old.key.square == key.square
                                && (old.key.active != key.active || !old.complete)
                        })
                        .map(|old| old.assets);
                    self.cache.insert(
                        id,
                        Cached {
                            key,
                            assets,
                            previous,
                            complete: true,
                            changed: std::time::Instant::now(),
                            hover: [0.; 3],
                            hover_tick: std::time::Instant::now(),
                        },
                    );
                }
                Err(error) => {
                    self.pending.remove(&id);
                    tracing::warn!(%error, "Could not rasterize decoration");
                }
            }
        }
        let Some(output_geo) = state.space.output_geometry(output) else {
            return result;
        };
        let scale = output.current_scale().fractional_scale();
        let texture_scale = (scale.ceil() as i32).max(2);
        for (id, window) in &state.windows.entries {
            if !enabled(window) {
                self.painted.remove(id);
                self.backdrops.remove(id);
                continue;
            }
            let Some(body) = state.space.element_geometry(window) else {
                continue;
            };
            let theme = state.decoration_for_window(window);
            if let Some(backdrop) = theme.backdrop(body) {
                self.backdrops.insert(*id, backdrop);
            } else {
                self.backdrops.remove(id);
            }
            let bounds = theme.render_bounds(body);
            if !bounds.overlaps(output_geo) {
                continue;
            }
            let title = window
                .toplevel()
                .map(crate::state::get_toplevel_info)
                .and_then(|(title, _)| title)
                .unwrap_or_default();
            let key = Key {
                width: bounds.size.w,
                height: frame_size(&theme).h,
                active: state.windows.active == Some(*id),
                title,
                scale: texture_scale,
                generation: state.decoration_generation,
                appearance: window
                    .toplevel()
                    .and_then(|surface| crate::state::get_toplevel_info(surface).1)
                    .unwrap_or_default(),
                square: theme.square,
            };
            let previous_caption = self
                .cache
                .get(id)
                .filter(|cached| cached.key.same_caption(&key))
                .and_then(|cached| cached.assets.title.clone());
            if self
                .cache
                .get(id)
                .is_some_and(|cached| !cached.key.same_frame(&key))
            {

                self.cache.remove(id);
            }
            if self.pending.get(id).is_some_and(|pending| pending != &key) {
                self.pending.remove(id);
            }
            if !self
                .cache
                .get(id)
                .is_some_and(|cached| cached.key == key && cached.complete)
                && !self.pending.contains_key(id)
                && self
                    .worker
                    .as_ref()
                    .is_some_and(|w| w.jobs.try_send((*id, key.clone(), theme.clone())).is_ok())
            {
                self.pending.insert(*id, key.clone());
            }
            if !self.cache.contains_key(id) {
                let placeholder_key = PlaceholderKey {
                    generation: key.generation,
                    appearance: key.appearance.clone(),
                    scale: key.scale,
                    active: key.active,
                    square: key.square,
                };
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    self.placeholders.entry(placeholder_key.clone())
                {
                    match Rasterizer::default()
                        .rasterize(&theme, &key, false)
                        .and_then(|raster| Assets::upload(renderer, raster, texture_scale))
                    {
                        Ok(assets) => {
                            entry.insert(assets);
                        }
                        Err(error) => {
                            tracing::warn!(%error, "Could not create immediate decoration frame");
                        }
                    }
                }
                if let Some(mut assets) = self.placeholders.get(&placeholder_key).cloned() {
                    assets.title = previous_caption;
                    self.cache.insert(
                        *id,
                        Cached {
                            key: key.clone(),
                            assets,
                            previous: None,
                            complete: false,
                            changed: std::time::Instant::now(),
                            hover: [0.; 3],
                            hover_tick: std::time::Instant::now(),
                        },
                    );
                }
            }
            if let (Some(cached), Some(toplevel)) = (self.cache.get_mut(id), window.toplevel()) {
                let progress = if theme.animation_duration == 0 {
                    1.
                } else {
                    (cached.changed.elapsed().as_secs_f32() * 1000.
                        / theme.animation_duration as f32)
                        .min(1.)
                };
                let alpha = 1. - (1. - progress).powi(3);
                let position = state.seat.get_pointer().map(|p| p.current_location());
                let hovered = position.and_then(|position| {
                    let (target, hit) = state.decoration_under(position)?;
                    (target == *window).then(|| hit.button_index()).flatten()
                });
                let pressed = state.decoration_button_press.and_then(|(window_id, hit)| {
                    (window_id == *id && hovered == hit.button_index())
                        .then(|| hit.button_index())
                        .flatten()
                });
                let dt = cached.hover_tick.elapsed().as_secs_f32();
                cached.hover_tick = std::time::Instant::now();
                for index in 0..3 {
                    let target = if hovered == Some(index) { 1. } else { 0. };
                    let step = if theme.animation_duration == 0 {
                        1.
                    } else {
                        (dt * 1000. / theme.animation_duration as f32).min(1.)
                    };
                    cached.hover[index] += (target - cached.hover[index]) * (step * 3.).min(1.);
                    if (target - cached.hover[index]).abs() < 0.01 {
                        cached.hover[index] = target;
                    }
                }
                let mut parts = cached.assets.parts(
                    &theme,
                    bounds,
                    if cached.previous.is_some() { alpha } else { 1. },
                    cached.hover,
                    pressed,
                );
                if progress < 1. {
                    if let Some(previous) = &cached.previous {
                        parts.extend(previous.parts(
                            &theme,
                            bounds,
                            1. - alpha,
                            cached.hover,
                            pressed,
                        ));
                    }
                } else {
                    cached.previous = None;
                }
                self.painted.insert(*id, parts.clone());
                let elements = parts
                    .iter()
                    .map(|part| part.element(output_geo.loc, scale, 1.))
                    .collect();
                result.insert(
                    smithay::backend::renderer::element::Id::from_wayland_resource(
                        toplevel.wl_surface(),
                    ),
                    elements,
                );
            }
        }
        self.painted
            .retain(|id, _| state.windows.entries.iter().any(|(win, _)| win == id));
        self.backdrops
            .retain(|id, _| state.windows.entries.iter().any(|(win, _)| win == id));
        self.pending
            .retain(|id, _| state.windows.entries.iter().any(|(win, _)| win == id));
        self.cache
            .retain(|id, _| state.windows.entries.iter().any(|(win, _)| win == id));
        self.placeholders
            .retain(|key, _| key.generation == state.decoration_generation);
        result
    }
}

type RasterResult = (u64, Key, anyhow::Result<RasterAssets>);
struct RasterWorker {
    jobs: std::sync::mpsc::SyncSender<(u64, Key, DecorationTheme)>,
    results: std::sync::mpsc::Receiver<RasterResult>,
}
impl RasterWorker {
    fn start() -> std::io::Result<Self> {
        let (jobs, requests) = std::sync::mpsc::sync_channel::<(u64, Key, DecorationTheme)>(4);
        let (replies, results) = std::sync::mpsc::sync_channel(4);
        std::thread::Builder::new()
            .name("rix-decorations".into())
            .spawn(move || {
                let mut rasterizer = Rasterizer::default();
                rasterizer.prepare_fonts();
                while let Ok((id, key, theme)) = requests.recv() {
                    let tile = rasterizer.rasterize(&theme, &key, true);
                    if replies.send((id, key, tile)).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self { jobs, results })
    }
}

fn rounded_path(x: f32, y: f32, width: f32, height: f32, radius: f32) -> Option<raster::Path> {
    let radius = radius.max(0.).min(width / 2.).min(height / 2.);
    let arc = radius * 0.552_284_8;
    let (right, bottom) = (x + width, y + height);
    let mut path = raster::PathBuilder::new();
    path.move_to(x + radius, y);
    path.line_to(right - radius, y);
    path.cubic_to(
        right - radius + arc,
        y,
        right,
        y + radius - arc,
        right,
        y + radius,
    );
    path.line_to(right, bottom - radius);
    path.cubic_to(
        right,
        bottom - radius + arc,
        right - radius + arc,
        bottom,
        right - radius,
        bottom,
    );
    path.line_to(x + radius, bottom);
    path.cubic_to(
        x + radius - arc,
        bottom,
        x,
        bottom - radius + arc,
        x,
        bottom - radius,
    );
    path.line_to(x, y + radius);
    path.cubic_to(x, y + radius - arc, x + radius - arc, y, x + radius, y);
    path.close();
    path.finish()
}

fn rounded_bottom_path(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    radius: f32,
) -> Option<raster::Path> {
    if width <= 0. || height <= 0. {
        return None;
    }
    let radius = radius.max(0.).min(width / 2.).min(height);
    let arc = radius * 0.552_284_8;
    let (right, bottom) = (x + width, y + height);
    let mut path = raster::PathBuilder::new();
    path.move_to(x, y);
    path.line_to(right, y);
    path.line_to(right, bottom - radius);
    path.cubic_to(
        right,
        bottom - radius + arc,
        right - radius + arc,
        bottom,
        right - radius,
        bottom,
    );
    path.line_to(x + radius, bottom);
    path.cubic_to(
        x + radius - arc,
        bottom,
        x,
        bottom - radius + arc,
        x,
        bottom - radius,
    );
    path.close();
    path.finish()
}

#[derive(Default)]
struct Rasterizer {
    fonts: Option<Arc<svg::fontdb::Database>>,
}
impl Rasterizer {
    fn font_database(&mut self) -> Arc<svg::fontdb::Database> {
        self.fonts
            .get_or_insert_with(|| {
                let mut fonts = svg::fontdb::Database::new();
                fonts.load_system_fonts();
                Arc::new(fonts)
            })
            .clone()
    }

    fn prepare_fonts(&mut self) {
        let _ = self.font_database();
    }

    fn rasterize(
        &mut self,
        theme: &DecorationTheme,
        key: &Key,
        include_title: bool,
    ) -> anyhow::Result<RasterAssets> {
        let actual_width = key.width * key.scale;
        let frame_size = frame_size(theme);

        let factor = key.scale;
        let mut theme = theme.clone();
        theme.height *= factor;
        theme.left *= factor;
        theme.right *= factor;
        theme.bottom *= factor;
        theme.button_size *= factor;
        theme.spacing *= factor;
        theme.padding *= factor;
        theme.font_size *= factor as f32;
        theme.radius *= factor as f32;
        theme.shadow = theme.shadow.map(|v| v * factor);
        let key = Key {
            width: frame_size.w * factor,
            height: frame_size.h * factor,
            active: key.active,
            title: key.title.clone(),
            scale: 1,
            generation: key.generation,
            appearance: key.appearance.clone(),
            square: key.square,
        };
        anyhow::ensure!(
            key.width > 0 && key.height > 0 && key.width <= 32768 && key.height <= 32768,
            "Invalid decoration size"
        );
        anyhow::ensure!(
            key.width as u64 * key.height as u64 * 4 <= 128 * 1024 * 1024,
            "Decoration exceeds memory limit"
        );
        let width = key.width as u32;
        let height = key.height as u32;
        let mut canvas = if let Some(tiles) = if key.active {
            theme.active.as_ref()
        } else {
            theme.inactive.as_ref().or(theme.active.as_ref())
        } {

            let mut squared;
            let tiles = if key.square && !theme.material {
                squared = tiles.as_ref().clone();
                let top = &mut squared[1];
                let skip = (theme.shadow[1] as u32 * theme.raster_scale / factor as u32)
                    .min(top.height.saturating_sub(1));
                top.pixels = top.pixels[(skip * top.width * 4) as usize..].to_vec();
                top.height -= skip;
                &squared
            } else {
                tiles.as_ref()
            };
            let frame = rix_assets::compose_frame_filtered(
                tiles,
                width,
                height,
                [
                    if key.square {
                        0
                    } else {
                        if theme.material {
                            theme.radius.ceil() as u32
                        } else {
                            tiles[3].width * factor as u32 / theme.raster_scale
                        }
                    },
                    if key.square {

                        theme.height as u32
                    } else {
                        if theme.material {
                            theme.radius.ceil() as u32
                        } else {
                            tiles[1].height * factor as u32 / theme.raster_scale
                        }
                    },
                    if key.square {
                        0
                    } else {
                        if theme.material {
                            theme.radius.ceil() as u32
                        } else {
                            tiles[5].width * factor as u32 / theme.raster_scale
                        }
                    },
                    if key.square {
                        0
                    } else {
                        if theme.material {
                            theme.radius.ceil() as u32
                        } else {
                            tiles[7].height * factor as u32 / theme.raster_scale
                        }
                    },
                ],
            )?;
            raster::Pixmap::from_vec(
                frame.pixels,
                raster::IntSize::from_wh(width, height).unwrap(),
            )
            .ok_or_else(|| anyhow::anyhow!("Invalid filtered decoration frame"))?
        } else {
            let mut canvas = raster::Pixmap::new(width, height).unwrap();
            let radius = theme.radius.min(width as f32 / 2.).min(height as f32 / 2.);
            let mut path = raster::PathBuilder::new();
            let (w, h) = (width as f32, height as f32);

            let arc = radius * 0.552_284_8;
            path.move_to(radius, 0.);
            path.line_to(w - radius, 0.);
            path.cubic_to(w - radius + arc, 0., w, radius - arc, w, radius);
            path.line_to(w, h - radius);
            path.cubic_to(w, h - radius + arc, w - radius + arc, h, w - radius, h);
            path.line_to(radius, h);
            path.cubic_to(radius - arc, h, 0., h - radius + arc, 0., h - radius);
            path.line_to(0., radius);
            path.cubic_to(0., radius - arc, radius - arc, 0., radius, 0.);
            path.close();
            let mut paint = raster::Paint::default();
            paint.set_color_rgba8(
                (theme.background.r * 255.) as u8,
                (theme.background.g * 255.) as u8,
                (theme.background.b * 255.) as u8,
                255,
            );
            if let Some(path) = path.finish() {
                canvas.fill_path(
                    &path,
                    &paint,
                    raster::FillRule::Winding,
                    raster::Transform::identity(),
                    None,
                );
            }
            canvas
        };
        if theme.material {
            if theme.active.is_none() {

                for row in 0..height {
                    let t = row as f32 / height.saturating_sub(1).max(1) as f32;
                    let [start, end] = theme.material_colors;
                    let alpha = (start.a + (end.a - start.a) * t) * theme.material_opacity;
                    let channels = [
                        start.r + (end.r - start.r) * t,
                        start.g + (end.g - start.g) * t,
                        start.b + (end.b - start.b) * t,
                    ];
                    for pixel in canvas.data_mut()
                        [(row * width * 4) as usize..((row + 1) * width * 4) as usize]
                        .as_chunks_mut::<4>()
                        .0
                    {
                        let coverage = pixel[3] as f32 / 255.;
                        let a = alpha * coverage;
                        pixel[3] = (a * 255.).round() as u8;
                        for channel in 0..3 {
                            pixel[channel] = (channels[channel] * a * 255.).round() as u8;
                        }
                    }
                }
            } else {
                for channel in canvas.data_mut() {
                    *channel = (*channel as f32 * theme.material_opacity).round() as u8;
                }
                if let Some(path) = rounded_path(0., 0., width as f32, height as f32, theme.radius)
                {
                    let mut mask = raster::Mask::new(width, height).unwrap();
                    mask.fill_path(
                        &path,
                        raster::FillRule::Winding,
                        true,
                        raster::Transform::identity(),
                    );
                    canvas.apply_mask(&mask);
                }
            }
        }
        if theme.opaque {
            let [left, top, right, bottom] = theme.frame_padding();
            let mut opaque = raster::Pixmap::new(width, height).unwrap();
            let mut paint = raster::Paint::default();
            paint.set_color_rgba8(
                (theme.background.r * 255.).round() as u8,
                (theme.background.g * 255.).round() as u8,
                (theme.background.b * 255.).round() as u8,
                255,
            );
            if let Some(path) = rounded_path(
                left as f32,
                top as f32,
                (width as i32 - left - right).max(0) as f32,
                (height as i32 - top - bottom).max(0) as f32,
                theme.radius,
            ) {
                opaque.fill_path(
                    &path,
                    &paint,
                    raster::FillRule::Winding,
                    raster::Transform::identity(),
                    None,
                );
            }
            opaque.draw_pixmap(
                0,
                0,
                canvas.as_ref(),
                &raster::PixmapPaint::default(),
                raster::Transform::identity(),
                None,
            );
            canvas = opaque;
        }

        let [pad_left, pad_top, pad_right, pad_bottom] = theme.frame_padding();
        let client_x = (theme.left + pad_left) as f32;
        let client_y = (theme.height + pad_top) as f32;
        let client_w = width as f32 - client_x - (theme.right + pad_right) as f32;
        let client_h = height as f32 - client_y - (theme.bottom + pad_bottom) as f32;
        if let Some(path) = rounded_bottom_path(
            client_x,
            client_y,
            client_w,
            client_h,
            theme.client_corner_radius() as f32,
        ) {
            let mut mask = raster::Mask::new(width, height).unwrap();
            mask.fill_path(
                &path,
                raster::FillRule::Winding,
                true,
                raster::Transform::identity(),
            );
            mask.invert();
            canvas.apply_mask(&mask);
        }
        let mut buttons: [Option<Tile>; 3] = [None, None, None];
        let mut button_hover: [Option<Tile>; 3] = [None, None, None];
        let mut button_pressed: [Option<Tile>; 3] = [None, None, None];
        let foreground = &theme.foreground[usize::from(!key.active)];
        for (index, slot) in buttons.iter_mut().enumerate() {
            let button = if index == 1 && key.square { 3 } else { index };
            let mut button_canvas =
                raster::Pixmap::new(theme.button_size as u32, theme.button_size as u32).unwrap();
            if let Some(tile) = theme.buttons[usize::from(!key.active)][button]
                .as_ref()
                .or(theme.buttons[0][button].as_ref())
            {
                rix_assets::blit(
                    &mut button_canvas,
                    tile,
                    0,
                    0,
                    theme.button_size as u32,
                    theme.button_size as u32,
                );
            } else {
                let symbol = match index {
                    0 => "M5 17h14",
                    1 if key.square => "M8 5h11v11M5 8h11v11H5Z",
                    1 => "M5 5h14v14H5Z",
                    _ => "M6 6l12 12M18 6 6 18",
                };
                let svg = format!("<svg xmlns='http://www.w3.org/2000/svg' width='{}' height='{}' viewBox='0 0 24 24'><path d='{symbol}' fill='none' stroke='{}' stroke-width='1.5'/></svg>",theme.button_size,theme.button_size,foreground);
                let tile = rix_assets::render_svg(&svg, &Default::default())?;
                rix_assets::blit(
                    &mut button_canvas,
                    &tile,
                    0,
                    0,
                    theme.button_size as u32,
                    theme.button_size as u32,
                );
            }
            *slot = Some(Tile {
                width: button_canvas.width(),
                height: button_canvas.height(),
                pixels: button_canvas.take(),
            });
            for (target, source, fill) in [
                (
                    &mut button_hover[index],
                    &theme.button_hover[button],
                    if index == 2 {
                        &theme.close_hover
                    } else {
                        &theme.hover
                    },
                ),
                (
                    &mut button_pressed[index],
                    &theme.button_pressed[button],
                    &theme.pressed,
                ),
            ] {
                let mut canvas =
                    raster::Pixmap::new(theme.button_size as u32, theme.button_size as u32)
                        .unwrap();
                if let Some(tile) = source {
                    rix_assets::blit(
                        &mut canvas,
                        tile,
                        0,
                        0,
                        theme.button_size as u32,
                        theme.button_size as u32,
                    );
                } else {
                    let svg = format!("<svg xmlns='http://www.w3.org/2000/svg' width='{0}' height='{0}'><rect width='{0}' height='{0}' rx='{1}' fill='{2}'/></svg>", theme.button_size, 6 * factor, fill);
                    let background = rix_assets::render_svg(&svg, &Default::default())?;
                    rix_assets::blit(
                        &mut canvas,
                        &background,
                        0,
                        0,
                        theme.button_size as u32,
                        theme.button_size as u32,
                    );
                    rix_assets::blit(
                        &mut canvas,
                        slot.as_ref().unwrap(),
                        0,
                        0,
                        theme.button_size as u32,
                        theme.button_size as u32,
                    );
                }
                *target = Some(Tile {
                    width: canvas.width(),
                    height: canvas.height(),
                    pixels: canvas.take(),
                });
            }
        }
        let mut title = None;
        let available = (theme.painted_button_rect(actual_width, 0).loc.x
            - pad_left
            - theme.padding * 2)
            .max(0) as u32;
        if include_title && available > 0 {
            let fonts = self.font_database();
            let text=format!("<svg xmlns='http://www.w3.org/2000/svg' width='{available}' height='{}'><text x='50%' y='50%' dominant-baseline='central' text-anchor='middle' font-family='{}' font-size='{}' fill='{}'>{}</text></svg>",theme.height,escape(&theme.font),theme.font_size,foreground,escape(&key.title));
            if let Ok(tile) = rix_assets::render_svg(
                &text,
                &svg::Options {
                    fontdb: fonts,
                    ..Default::default()
                },
            ) {
                title = Some(tile);
            }
        }
        Ok(RasterAssets {
            frame: Tile {
                width,
                height,
                pixels: canvas.take(),
            },
            title,
            buttons,
            button_hover,
            button_pressed,
        })
    }
}
fn aurorae_options(directory: &std::path::Path) -> Option<String> {
    let name = directory.file_name()?;
    std::fs::read_to_string(directory.join(format!("{}rc", name.to_string_lossy())))
        .ok()
        .or_else(|| {

            let metadata = std::fs::read_to_string(directory.join("metadata.desktop")).ok()?;
            let id = metadata.lines().find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == "X-KDE-PluginInfo-Name").then_some(value.trim())
            })?;
            let mut components = std::path::Path::new(id).components();
            if !matches!(components.next(), Some(std::path::Component::Normal(_)))
                || components.next().is_some()
            {
                return None;
            }
            std::fs::read_to_string(directory.join(format!("{id}rc"))).ok()
        })
}
#[cfg(test)]
fn aurorae_padding(directory: &std::path::Path) -> [i32; 4] {
    aurorae_options(directory)
        .as_deref()
        .map(aurorae_padding_from_options)
        .unwrap_or_default()
}
fn aurorae_padding_from_options(text: &str) -> [i32; 4] {
    let mut layout = false;
    let mut padding = [0; 4];
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            layout = line == "[Layout]";
        }
        if !layout {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if let Some(index) = ["PaddingLeft", "PaddingTop", "PaddingRight", "PaddingBottom"]
                .iter()
                .position(|name| *name == key.trim())
            {
                padding[index] = value.trim().parse::<i32>().unwrap_or(0).clamp(0, 128);
            }
        }
    }
    padding
}
fn aurorae_color(text: &str, name: &str) -> Option<String> {
    let mut general = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            general = line == "[General]";
        }
        if !general {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != name {
            continue;
        }
        let value = value.trim();
        if rix_core::Color::from_hex(value).is_some() {
            return Some(value.to_owned());
        }
        let rgb = value
            .split(',')
            .map(|component| component.trim().parse::<u8>())
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        return match rgb.as_slice() {
            [r, g, b] => Some(format!("#{r:02x}{g:02x}{b:02x}")),
            [r, g, b, a] => Some(format!("#{r:02x}{g:02x}{b:02x}{a:02x}")),
            _ => None,
        };
    }
    None
}
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&apos;")
        .replace('"', "&quot;")
}
impl XdgDecorationHandler for RixState {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        let app_id = crate::state::get_toplevel_info(&toplevel)
            .1
            .unwrap_or_default();
        let mode = effective_mode(
            self.server_side_decorations,
            self.decoration_preferences
                .get(toplevel.wl_surface())
                .copied(),
        );
        tracing::debug!(%app_id, ?mode, "Selected initial window decoration mode");
        set_mode(toplevel, mode);
    }
    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: Mode) {
        self.decoration_preferences
            .insert(toplevel.wl_surface().clone(), mode);
        let effective = effective_mode(self.server_side_decorations, Some(mode));
        let app_id = crate::state::get_toplevel_info(&toplevel)
            .1
            .unwrap_or_default();
        tracing::debug!(%app_id, ?mode, ?effective, "Negotiated window decoration mode");
        set_mode(toplevel, effective);
    }
    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.decoration_preferences.remove(toplevel.wl_surface());
        set_mode(toplevel, effective_mode(self.server_side_decorations, None));
    }
}

pub(crate) fn effective_mode(prefer_server_side: bool, requested: Option<Mode>) -> Mode {
    if !prefer_server_side {
        return Mode::ClientSide;
    }
    requested.unwrap_or(Mode::ServerSide)
}

fn set_mode(toplevel: ToplevelSurface, mode: Mode) {
    toplevel.with_pending_state(|state| state.decoration_mode = Some(mode));

    if toplevel.is_initial_configure_sent() {
        toplevel.send_pending_configure();
    }
}
smithay::delegate_xdg_decoration!(RixState);

use smithay::wayland::shell::kde::decoration::{KdeDecorationHandler, KdeDecorationState};
use wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration::{
    Mode as LegacyMode, OrgKdeKwinServerDecoration,
};
use wayland_server::{protocol::wl_surface::WlSurface, WEnum};

pub(crate) fn set_legacy_mode(toplevel: &ToplevelSurface, mode: LegacyMode) {
    let mode = match mode {
        LegacyMode::Server => Some(Mode::ServerSide),
        LegacyMode::Client => Some(Mode::ClientSide),
        _ => None,
    };
    toplevel.with_pending_state(|state| state.decoration_mode = mode);
    if toplevel.is_initial_configure_sent() {
        toplevel.send_pending_configure();
    }
}

impl KdeDecorationHandler for RixState {
    fn kde_decoration_state(&self) -> &KdeDecorationState {
        &self.kde_decoration
    }

    fn new_decoration(&mut self, surface: &WlSurface, decoration: &OrgKdeKwinServerDecoration) {
        struct LegacyCleanup;
        let first = smithay::wayland::compositor::with_states(surface, |states| {
            states.data_map.insert_if_missing(|| LegacyCleanup)
        });
        if first {
            smithay::wayland::compositor::add_destruction_hook::<Self, _>(
                surface,
                |state, surface| {
                    state.kde_decorations.remove(surface);
                },
            );
        }
        let mode = if self.server_side_decorations {
            LegacyMode::Server
        } else {
            LegacyMode::Client
        };
        KdeDecorationHandler::request_mode(self, surface, decoration, WEnum::Value(mode));
    }

    fn request_mode(
        &mut self,
        surface: &WlSurface,
        decoration: &OrgKdeKwinServerDecoration,
        mode: WEnum<LegacyMode>,
    ) {
        let WEnum::Value(mode) = mode else {
            return;
        };

        self.kde_decorations
            .insert(surface.clone(), (decoration.clone(), mode));
        tracing::debug!(
            ?surface,
            ?mode,
            "Negotiated GTK legacy window decoration mode"
        );
        if let Some(toplevel) = self
            .xdg_shell_state
            .toplevel_surfaces()
            .iter()
            .find(|toplevel| toplevel.wl_surface() == surface)
        {
            set_legacy_mode(toplevel, mode);
        }
        decoration.mode(mode);
    }

    fn release(&mut self, decoration: &OrgKdeKwinServerDecoration, surface: &WlSurface) {
        if self
            .kde_decorations
            .get(surface)
            .is_some_and(|(current, _)| current != decoration)
        {
            return;
        }
        self.kde_decorations.remove(surface);
        if let Some(toplevel) = self
            .xdg_shell_state
            .toplevel_surfaces()
            .iter()
            .find(|toplevel| toplevel.wl_surface() == surface)
        {
            set_legacy_mode(toplevel, LegacyMode::None);
        }
    }
}
smithay::delegate_kde_decoration!(RixState);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compositor_prefers_server_decorations_but_respects_client_side_requests() {
        assert_eq!(
            effective_mode(true, Some(Mode::ClientSide)),
            Mode::ClientSide
        );
        assert_eq!(
            effective_mode(false, Some(Mode::ClientSide)),
            Mode::ClientSide
        );
        assert_eq!(
            effective_mode(false, Some(Mode::ServerSide)),
            Mode::ClientSide
        );
        assert_eq!(
            effective_mode(true, Some(Mode::ServerSide)),
            Mode::ServerSide
        );
        assert_eq!(effective_mode(true, None), Mode::ServerSide);
        assert_eq!(effective_mode(false, None), Mode::ClientSide);
    }

    #[test]
    fn every_resize_edge_has_a_directional_cursor() {
        use smithay::input::pointer::CursorIcon;
        let theme = DecorationTheme::load(&rix_config::ThemeSettings::default());
        let body = Rectangle::new((100, 100).into(), (600, 400).into());
        let bounds = theme.bounds(body);
        let l = bounds.loc.x as f64 + 0.5;
        let r = (bounds.loc.x + bounds.size.w) as f64 - 0.5;
        let t = bounds.loc.y as f64 + 0.5;
        let b = (bounds.loc.y + bounds.size.h) as f64 - 0.5;
        for (position, expected) in [
            ((l, t), CursorIcon::NwResize),
            ((r, t), CursorIcon::NeResize),
            ((l, b), CursorIcon::SwResize),
            ((r, b), CursorIcon::SeResize),
            ((400., t), CursorIcon::NResize),
            ((400., b), CursorIcon::SResize),
            ((l, 300.), CursorIcon::WResize),
            ((r, 300.), CursorIcon::EResize),
        ] {
            let Some(DecorationHit::Resize(edge)) = theme.hit(body, position.into()) else {
                panic!("Missing resize edge at {position:?}");
            };
            assert_eq!(crate::grabs::resize_cursor(edge), expected);
        }
    }

    #[test]
    fn frame_parts_follow_resize_without_scaling_borders() {
        let borders = [18, 46, 18, 22];
        for size in [(600, 400), (1400, 900), (1920, 1080)] {
            let slices = frame_slices(size.into(), borders);
            assert_eq!(slices[0].size, Size::from((18, 46)));
            assert_eq!(slices[8].loc, Point::from((size.0 - 18, size.1 - 22)));
            assert_eq!(slices[5].loc.x, size.0 - 18);
            assert_eq!(slices[5].size.h, size.1 - 46 - 22);
        }
        let source = frame_slices((37, 69).into(), borders);
        assert_eq!(source[1].size, Size::from((1, 46)));
    }

    #[test]
    fn decoration_atlas_fits_svg_edges_larger_than_the_hit_border() {
        let mut theme = DecorationTheme::load(&rix_config::ThemeSettings::default());
        theme.raster_scale = 4;
        theme.active = Some(Arc::new(
            (0..9)
                .map(|_| Tile {
                    width: 160,
                    height: 160,
                    pixels: vec![0; 160 * 160 * 4],
                })
                .collect(),
        ));
        let borders = frame_borders(&theme);
        assert!(borders.iter().all(|b| *b >= 40));
        let key = Key {
            width: 600,
            height: 400,
            active: true,
            title: String::new(),
            scale: 2,
            generation: 0,
            appearance: String::new(),
            square: false,
        };
        let raster = Rasterizer::default().rasterize(&theme, &key, true).unwrap();
        assert_eq!(raster.frame.width, frame_size(&theme).w as u32 * 2);
        assert_eq!(raster.frame.height, frame_size(&theme).h as u32 * 2);
    }

    #[test]
    fn maximized_svg_titlebar_fits_the_atlas_at_every_raster_scale() {
        let mut theme = DecorationTheme::load(&rix_config::ThemeSettings::default());
        theme.raster_scale = 4;
        theme.shadow = [12, 6, 12, 16];
        let tiles = Arc::new(
            (0..9)
                .map(|_| Tile {
                    width: 160,
                    height: 160,
                    pixels: vec![0; 160 * 160 * 4],
                })
                .collect(),
        );
        theme.active = Some(Arc::clone(&tiles));
        theme.inactive = Some(tiles);
        let theme = theme.with_maximized(true);
        for scale in 1..=4 {
            for active in [true, false] {
                let key = Key {
                    width: 1920,
                    height: frame_size(&theme).h,
                    active,
                    title: String::new(),
                    scale,
                    generation: 0,
                    appearance: String::new(),
                    square: true,
                };
                let raster = Rasterizer::default()
                    .rasterize(&theme, &key, false)
                    .unwrap();
                assert_eq!(
                    raster.frame.height,
                    (theme.height + 1) as u32 * scale as u32
                );
            }
        }
    }

    #[test]
    fn renamed_aurorae_resources_keep_their_declared_shadow_padding() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("Rix-ChromeOS-dark");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(
            directory.join("metadata.desktop"),
            "[Desktop Entry]\nX-KDE-PluginInfo-Name=ChromeOS-dark\n",
        )
        .unwrap();
        std::fs::write(
            directory.join("ChromeOS-darkrc"),
            "[Layout]\nPaddingLeft=12\nPaddingTop=6\nPaddingRight=12\nPaddingBottom=16\n",
        )
        .unwrap();
        assert_eq!(aurorae_padding(&directory), [12, 6, 12, 16]);

        std::fs::write(
            directory.join("metadata.desktop"),
            "[Desktop Entry]\nX-KDE-PluginInfo-Name=../Rix-ChromeOS-dark/ChromeOS-dark\n",
        )
        .unwrap();
        assert_eq!(aurorae_padding(&directory), [0; 4]);
    }

    #[test]
    fn fairywren_controls_match_the_larger_titlebar_and_hitboxes() {
        let settings: rix_config::ThemeSettings = serde_json::from_str(include_str!(
            "../../../assets/rix/themes/fairywren-dark/theme.json"
        ))
        .unwrap();
        settings.validate().unwrap();
        let mut theme = DecorationTheme::load(&settings);
        theme.shadow = [12, 6, 12, 16];
        assert_eq!(theme.height, 36);
        assert_eq!(theme.button_size, 26);
        let body = Rectangle::new((100, 100).into(), (600, 400).into());
        let bounds = theme.bounds(body);
        let render_bounds = theme.render_bounds(body);
        for (index, expected) in [
            DecorationHit::Minimize,
            DecorationHit::Maximize,
            DecorationHit::Close,
        ]
        .into_iter()
        .enumerate()
        {
            let button = theme.button_rect(bounds.size.w, index);
            let painted = theme.painted_button_rect(render_bounds.size.w, index);
            assert_eq!(bounds.loc + button.loc, render_bounds.loc + painted.loc);
            assert!(button.loc.y >= 0 && button.loc.y + button.size.h <= theme.height);
            let center = bounds.loc + button.loc + Point::from((13, 13));
            let hit = theme.hit(body, center.to_f64()).unwrap();
            assert_eq!(
                std::mem::discriminant(&hit),
                std::mem::discriminant(&expected)
            );
        }
    }

    #[test]
    fn fairywren_button_states_share_physical_dimensions_and_select_restore_artwork() {
        let settings: rix_config::ThemeSettings = serde_json::from_str(include_str!(
            "../../../assets/rix/themes/fairywren-dark/theme.json"
        ))
        .unwrap();
        let mut theme = DecorationTheme::load_with_material(&settings, true);
        let tiles = |channel: usize, size: u32| {
            std::array::from_fn(|index| {
                let mut color = [0, 0, 0, 255];
                color[channel] = 40 + index as u8 * 40;
                Some(Tile {
                    width: size,
                    height: size,
                    pixels: color.repeat((size * size) as usize),
                })
            })
        };
        theme.buttons = Arc::new([tiles(0, 88), tiles(0, 88)]);
        theme.button_hover = Arc::new(tiles(1, 88));
        theme.button_pressed = Arc::new(tiles(2, 88));
        for display_scale in [1_f64, 1.5, 2.] {
            let scale = (display_scale.ceil() as i32).max(2);
            for maximized in [false, true] {
                let theme = theme.with_maximized(maximized);
                let assets = Rasterizer::default()
                    .rasterize(
                        &theme,
                        &Key {
                            width: 800,
                            height: frame_size(&theme).h,
                            active: true,
                            title: String::new(),
                            scale,
                            generation: 0,
                            appearance: String::new(),
                            square: maximized,
                        },
                        false,
                    )
                    .unwrap();
                for (index, states) in assets
                    .buttons
                    .iter()
                    .zip(&assets.button_hover)
                    .zip(&assets.button_pressed)
                    .enumerate()
                {
                    let ((normal, hover), pressed) = states;
                    let artwork_index = if index == 1 && maximized { 3 } else { index };
                    for (channel, tile) in [normal, hover, pressed].into_iter().enumerate() {
                        let tile = tile.as_ref().unwrap();
                        assert_eq!(
                            (tile.width, tile.height),
                            (
                                (theme.button_size * scale) as u32,
                                (theme.button_size * scale) as u32
                            )
                        );
                        let center = ((tile.height / 2 * tile.width + tile.width / 2) * 4) as usize;
                        assert_eq!(tile.pixels[center + channel], 40 + artwork_index as u8 * 40,
                            "wrong artwork at scale {display_scale}, maximized {maximized}, button {index}, state {channel}");
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "requires EGL; verifies one complete sprite per button and pressed precedence"]
    fn gpu_decoration_button_states_replace_glyphs_instead_of_overlaying_them() {
        use smithay::backend::{
            egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay},
            renderer::element::Element,
        };
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        let context = EGLContext::new(&display).unwrap();
        let mut renderer = unsafe { GlesRenderer::new(context) }.unwrap();
        let theme =
            DecorationTheme::load_with_material(&rix_config::ThemeSettings::default(), true);
        let key = Key {
            width: 800,
            height: frame_size(&theme).h,
            active: true,
            title: String::new(),
            scale: 2,
            generation: 0,
            appearance: String::new(),
            square: false,
        };
        let assets = Assets::upload(
            &mut renderer,
            Rasterizer::default()
                .rasterize(&theme, &key, false)
                .unwrap(),
            key.scale,
        )
        .unwrap();
        let bounds = theme.render_bounds(Rectangle::new((100, 100).into(), (800, 400).into()));
        for display_scale in [1., 1.5, 2.] {
            for (hover, pressed) in [([0.; 3], None), ([1.; 3], None), ([1.; 3], Some(1))] {
                let parts = assets.parts(&theme, bounds, 1., hover, pressed);
                let buttons: Vec<_> = parts.iter().filter(|part| part.source.is_none()).collect();
                assert_eq!(buttons.len(), 3, "hover must not add a second glyph layer");
                for (index, hover_value) in hover.iter().enumerate() {
                    let rect = theme.painted_button_rect(bounds.size.w, index);
                    let bounds = Rectangle::new(bounds.loc + rect.loc, rect.size);
                    let part = buttons.iter().find(|part| part.bounds == bounds).unwrap();
                    let expected = if pressed == Some(index) {
                        assets.button_pressed[index].as_ref()
                    } else if *hover_value > 0.5 {
                        assets.button_hover[index].as_ref()
                    } else {
                        assets.buttons[index].as_ref()
                    }
                    .unwrap();
                    let expected = TextureRenderElement::from_texture_buffer(
                        (0., 0.),
                        expected,
                        None,
                        None,
                        Some(rect.size),
                        Kind::Unspecified,
                    );
                    assert_eq!(
                        part.element((0, 0).into(), display_scale, 1.).id(),
                        expected.id()
                    );
                }
            }
        }
    }

    #[test]
    #[ignore = "requires EGL; window state must update while the font/raster worker is stalled"]
    fn gpu_frame_and_buttons_change_immediately_before_worker_completion() {
        use smithay::backend::{
            egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay},
            renderer::element::Element,
        };
        let mut desktop = crate::lifecycle::tests::TestDesktop::new();
        let window = desktop.window("Immediate controls");
        desktop.inspect(|state| {
            state.windows.entries[0]
                .1
                .toplevel()
                .unwrap()
                .with_pending_state(|pending| {
                    pending.decoration_mode = Some(Mode::ServerSide);
                });
            state.windows.entries[0]
                .1
                .toplevel()
                .unwrap()
                .send_configure();
        });
        desktop.roundtrip();
        window.0.commit();
        desktop.roundtrip();
        desktop.inspect_with_timeout(std::time::Duration::from_secs(8), |state| {
            let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
            let context = EGLContext::new(&display).unwrap();
            let mut renderer = unsafe { GlesRenderer::new(context) }.unwrap();
            let output = state.space.outputs().next().unwrap().clone();
            let id = state.windows.entries[0].0;
            let mut theme = rix_config::ThemeSettings::default();
            theme.appearance.panel_frame.clear();
            theme.appearance.decoration_theme.clear();
            state.set_decoration_theme(&theme);
            let (jobs, _stalled_requests) = std::sync::mpsc::sync_channel(4);
            let (_stalled_replies, results) = std::sync::mpsc::sync_channel(4);
            let mut decorations = DecorationRenderer {
                worker: Some(RasterWorker { jobs, results }),
                ..Default::default()
            };
            let texture_id = |texture: &TextureBuffer<GlesTexture>| {
                TextureRenderElement::from_texture_buffer(
                    (0., 0.),
                    texture,
                    None,
                    None,
                    None,
                    Kind::Unspecified,
                )
                .id()
                .clone()
            };
            decorations.elements(&mut renderer, state, &output);
            let original = &decorations.cache[&id];
            let frame = texture_id(&original.assets.frame);
            let maximize = texture_id(original.assets.buttons[1].as_ref().unwrap());
            assert!(!original.complete);
            let caption = TextureBuffer::from_memory(
                &mut renderer,
                &[255; 8 * 8 * 4],
                Fourcc::Abgr8888,
                (8, 8),
                false,
                2,
                smithay::utils::Transform::Normal,
                None,
            )
            .unwrap();
            let caption_id = texture_id(&caption);
            decorations.cache.get_mut(&id).unwrap().assets.title = Some((caption, (4, 4).into()));
            state
                .apply_window_action(rix_ipc::WindowAction::Maximize(id))
                .unwrap();
            decorations.elements(&mut renderer, state, &output);
            assert!(
                !decorations.cache[&id].key.square,
                "The old buffer keeps its old frame until resize completes"
            );

            state.windows.pending_placement.remove(&id);
            decorations.elements(&mut renderer, state, &output);
            let maximized = &decorations.cache[&id];
            assert!(maximized.key.square);
            assert!(!maximized.complete, "no worker result has been delivered");
            assert_ne!(texture_id(&maximized.assets.frame), frame);
            assert_ne!(
                texture_id(maximized.assets.buttons[1].as_ref().unwrap()),
                maximize
            );
            assert_eq!(
                texture_id(&maximized.assets.title.as_ref().unwrap().0),
                caption_id
            );
            assert!(
                decorations.pending[&id] == maximized.key,
                "old pending job must be superseded"
            );
            state
                .apply_window_action(rix_ipc::WindowAction::Maximize(id))
                .unwrap();
            state.windows.pending_placement.remove(&id);
            decorations.elements(&mut renderer, state, &output);
            let restored = &decorations.cache[&id];
            assert!(!restored.key.square);
            assert_eq!(
                texture_id(&restored.assets.frame),
                frame,
                "normal rounded frame is immediate"
            );
            assert_eq!(
                texture_id(restored.assets.buttons[1].as_ref().unwrap()),
                maximize,
                "normal maximize glyph is immediate"
            );
            assert_eq!(
                texture_id(&restored.assets.title.as_ref().unwrap().0),
                caption_id
            );
            output.change_current_state(
                None,
                None,
                Some(smithay::output::Scale::Fractional(3.)),
                None,
            );
            state.reflow_output_layout();
            decorations.elements(&mut renderer, state, &output);
            assert_eq!(decorations.cache[&id].key.scale, 3);
            assert!(
                decorations.cache[&id].assets.title.is_none(),
                "caption from another raster scale cannot be reused"
            );
            theme
                .appearance
                .palette
                .insert("text_primary".into(), "#ff0000".into());
            state.set_decoration_theme(&theme);
            decorations.elements(&mut renderer, state, &output);
            assert!(
                decorations.cache[&id].assets.title.is_none(),
                "old theme/color caption cannot be reused"
            );
        });
    }

    #[test]
    fn immediate_frame_rasterization_skips_the_slow_title_font_path() {
        let theme = DecorationTheme::load(&rix_config::ThemeSettings::default());
        let key = Key {
            width: 1200,
            height: frame_size(&theme).h,
            active: true,
            title: "First window".into(),
            scale: 2,
            generation: 0,
            appearance: String::new(),
            square: false,
        };

        let raster = Rasterizer::default()
            .rasterize(&theme, &key, false)
            .unwrap();

        assert!(raster.frame.width > 0);
        assert!(raster.frame.height > 0);
        assert!(raster.title.is_none());
        assert!(raster.buttons.iter().all(Option::is_some));
    }

    #[test]
    fn maximized_geometry_has_only_a_square_titlebar() {
        let theme =
            DecorationTheme::load(&rix_config::ThemeSettings::default()).with_maximized(true);
        let body = Rectangle::new((0, theme.height).into(), (1920, 1000 - theme.height).into());
        assert_eq!(
            theme.bounds(body),
            Rectangle::new((0, 0).into(), (1920, 1000).into())
        );
        assert!(matches!(
            theme.hit(body, (2., 2.).into()),
            Some(DecorationHit::Move)
        ));
        assert!(theme.hit(body, (1., 500.).into()).is_none());
        assert_eq!(theme.radius, 0.);
    }
    #[test]
    fn decoration_controls_never_cover_client_pixels() {
        let theme = DecorationTheme::load(&rix_config::ThemeSettings::default());
        let body = Rectangle::new((100, 100).into(), (600, 400).into());
        assert!(theme.hit(body, (110., 105.).into()).is_none());
        assert!(matches!(
            theme.hit(body, (200., 85.).into()),
            Some(DecorationHit::Move)
        ));
        let bounds = theme.bounds(body);
        for (i, expected) in [
            DecorationHit::Minimize,
            DecorationHit::Maximize,
            DecorationHit::Close,
        ]
        .iter()
        .enumerate()
        {
            let rect = theme.button_rect(bounds.size.w, i);
            let hit = theme
                .hit(body, (bounds.loc + rect.loc + Point::from((5, 5))).to_f64())
                .unwrap();
            assert_eq!(
                std::mem::discriminant(&hit),
                std::mem::discriminant(expected)
            );
        }
    }
}

#[cfg(test)]
mod material_tests {
    use super::*;
    fn raster(theme: &DecorationTheme, square: bool) -> RasterAssets {
        let theme = theme.with_maximized(square);
        Rasterizer::default()
            .rasterize(
                &theme,
                &Key {
                    width: 800,
                    height: frame_size(&theme).h,
                    active: true,
                    title: String::new(),
                    scale: 1,
                    generation: 0,
                    appearance: String::new(),
                    square,
                },
                false,
            )
            .unwrap()
    }
    fn pixel(frame: &Tile, x: u32, y: u32) -> &[u8] {
        &frame.pixels
            [((y * frame.width + x) * 4) as usize..((y * frame.width + x) * 4 + 4) as usize]
    }
    #[test]
    fn opaque_aurorae_preserves_translucent_shadow_but_fills_frame_without_touching_client() {
        let mut theme =
            DecorationTheme::load_with_material(&rix_config::ThemeSettings::default(), false);
        theme.shadow = [12, 6, 12, 16];
        theme.active = Some(Arc::new(
            (0..9)
                .map(|_| Tile {
                    width: 4,
                    height: 4,
                    pixels: [10, 20, 30, 80].repeat(16),
                })
                .collect(),
        ));
        let frame = raster(&theme, false).frame;
        assert_eq!(
            pixel(&frame, frame.width / 2, 6 + theme.height as u32 / 2)[3],
            255
        );
        assert_eq!(
            pixel(&frame, 0, 0)[3],
            80,
            "external artwork/shadow alpha stays intact"
        );
        assert_eq!(
            pixel(&frame, frame.width / 2, 6 + theme.height as u32),
            [0; 4],
            "client interior stays untouched"
        );
    }
    #[test]
    fn desktop_material_uses_panel_artwork_alpha_and_preserves_controls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("panel.svg");
        let pieces: String = rix_assets::FRAME_PARTS
            .iter()
            .enumerate()
            .map(|(i, id)| {
                format!(
                    "<rect id='{id}' x='{}' y='{}' width='4' height='4' fill='#204060'/>",
                    i % 3 * 4,
                    i / 3 * 4
                )
            })
            .collect();
        std::fs::write(&path, format!("<svg xmlns='http://www.w3.org/2000/svg' width='12' height='12'><g opacity='0.3'>{pieces}</g></svg>")).unwrap();
        let mut settings = rix_config::ThemeSettings::default();
        settings.appearance.panel_frame = path.to_string_lossy().into_owned();
        settings.appearance.decoration_theme.clear();
        settings.opacity = 0.5;
        let theme = DecorationTheme::load_with_material(&settings, true);
        let material = raster(&theme, false);
        let p = pixel(
            &material.frame,
            material.frame.width / 2,
            theme.height as u32 / 2,
        );
        assert!(
            (37..=39).contains(&p[3]),
            "panel alpha must be 0.3 × 0.5: {p:?}"
        );
        assert!(p[..3].iter().all(|c| *c <= p[3]), "premultiplied alpha");
        assert_eq!(
            pixel(
                &material.frame,
                material.frame.width / 2,
                theme.height as u32
            ),
            [0; 4]
        );
        assert!(
            material.buttons.iter().flatten().any(|button| button
                .pixels
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[3] > 200)),
            "glyphs must not inherit material opacity"
        );
        assert!(material.button_hover.iter().all(Option::is_some));
        let original = DecorationTheme::load_with_material(&settings, false);
        assert!(original
            .backdrop(Rectangle::from_size((100, 100).into()))
            .is_none());
        let frame = raster(&original, false).frame;
        assert_eq!(
            pixel(&frame, frame.width / 2, original.height as u32 / 2)[3],
            255
        );
    }
    #[test]
    fn fallback_material_honors_color_alpha_and_maximized_buttons() {
        let mut settings = rix_config::ThemeSettings::default();
        settings.appearance.panel_frame.clear();
        settings.appearance.decoration_theme.clear();
        settings
            .appearance
            .palette
            .insert("glass_start".into(), "#33669980".into());
        settings
            .appearance
            .palette
            .insert("glass_end".into(), "#33669980".into());
        settings.opacity = 0.5;
        let theme = DecorationTheme::load_with_material(&settings, true);
        for square in [false, true] {
            let frame = raster(&theme, square);
            assert_eq!(
                pixel(&frame.frame, frame.frame.width / 2, theme.height as u32 / 2)[3],
                64
            );
            assert!(frame.buttons.iter().all(Option::is_some));
        }
    }
    #[test]
    fn material_rounding_uses_theme_radius_and_preserves_lower_corner_material() {
        let settings = rix_config::ThemeSettings::default();
        let theme = DecorationTheme::load_with_material(&settings, true);
        assert_eq!(
            theme.corner_radius(),
            settings.appearance.metric("radius_md")
        );
        assert_eq!(
            theme.client_corner_radius(),
            (theme.radius - theme.left.max(theme.right).max(theme.bottom) as f32).max(0.) as f64
        );
        let frame = raster(&theme, false).frame;
        let x = theme.left as u32;
        let y = frame.height - theme.bottom as u32 - 1;
        assert!(
            pixel(&frame, x, y)[3] > 0,
            "frame material must fill the client's cut lower corner"
        );
        assert_eq!(
            pixel(&frame, frame.width / 2, y)[3],
            0,
            "client interior remains transparent in the frame atlas"
        );
        assert_eq!(
            pixel(&frame, x, theme.height as u32)[3],
            0,
            "content's upper edge stays square"
        );
        assert_eq!(theme.with_maximized(true).client_corner_radius(), 0.);
    }
    #[test]
    fn server_backdrop_covers_only_frame_and_follows_animation() {
        let settings = rix_config::ThemeSettings::default();
        let theme = DecorationTheme::load_with_material(&settings, true);
        let body = Rectangle::new((100, 100).into(), (600, 400).into());
        for square in [false, true] {
            let theme = theme.with_maximized(square);
            let backdrop = theme.backdrop(body).unwrap();
            let shapes = backdrop.shapes();
            assert!(!shapes.is_empty());
            assert_eq!(shapes.len(), 1);
            assert!(shapes[0].clip.is_none());
            assert_eq!(shapes[0].width, backdrop.bounds.size.w as f64);
            assert_eq!(shapes[0].height, backdrop.bounds.size.h as f64);
            assert_eq!(
                shapes[0].radius,
                if square { 0. } else { theme.radius as f64 }
            );
            assert_eq!(backdrop.client_radius, theme.client_corner_radius());
            let client = crate::render::rounded::ClientClip {
                bounds: body.to_f64(),
                radius: backdrop.client_radius,
            };
            assert!(client.contains((400., 300.).into()));
            assert_eq!(client.contains((100.1, 499.9).into()), square);
            let moved = backdrop.clone().transformed((400, 300).into(), 0.5, 0.25);
            assert_eq!(moved.alpha, 0.25);
            assert_eq!(moved.radius, backdrop.radius * 0.5);
            assert_eq!(moved.client_radius, backdrop.client_radius * 0.5);
            assert_eq!(moved.body.size, Size::from((300, 200)));
            assert_eq!(moved.body.loc, Point::from((250, 200)));
        }
    }
    #[test]
    fn aurorae_text_colors_support_rgb_alpha_without_path_or_style_injection() {
        let options = "[General]\nActiveTextColor=1,2,3\nInactiveTextColor=4,5,6,127\n";
        assert_eq!(
            aurorae_color(options, "ActiveTextColor").as_deref(),
            Some("#010203")
        );
        assert_eq!(
            aurorae_color(options, "InactiveTextColor").as_deref(),
            Some("#0405067f")
        );
        assert!(
            aurorae_color("[General]\nActiveTextColor='><script>\n", "ActiveTextColor").is_none()
        );
    }
}
