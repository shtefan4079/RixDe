mod backdrop_budget;
mod coverage;
mod drag_motion;
mod motion;
pub(crate) mod rounded;
use crate::state::RixState;
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            damage::OutputDamageTracker,
            element::{
                render_elements,
                solid::{SolidColorBuffer, SolidColorRenderElement},
                surface::WaylandSurfaceRenderElement,
                texture::{TextureBuffer, TextureRenderBuffer, TextureRenderElement},
                utils::{Relocate, RelocateRenderElement, RescaleRenderElement},
                AsRenderElements, Element, Id, Kind,
            },
            gles::{
                element::TextureShaderElement, GlesRenderer, GlesTexProgram, GlesTexture, Uniform,
                UniformName, UniformType,
            },
            sync::SyncPoint,
            utils::CommitCounter,
            Bind, Frame, Offscreen, Renderer, Texture,
        },
    },
    desktop::space::{space_render_elements, SpaceRenderElements},
    output::Output,
    utils::{Buffer, Logical, Physical, Point, Rectangle, Size, Transform},
    wayland::{compositor::RectangleKind, shell::wlr_layer::Layer},
};
use std::collections::{HashMap, HashSet};

type BlurRequest = (
    Option<smithay::wayland::compositor::RegionAttributes>,
    Option<Vec<crate::surface_effects::Shape>>,
    rix_config::appearance::MaterialEffects,
);

fn texture_pixel_source(texture: &TextureBuffer<GlesTexture>) -> Rectangle<f64, Logical> {
    element_pixel_source(&TextureRenderElement::from_texture_buffer(
        (0., 0.),
        texture,
        None,
        None,
        None,
        Kind::Unspecified,
    ))
}

fn wallpaper_pixel_source(texture: &TextureRenderBuffer<GlesTexture>) -> Rectangle<f64, Logical> {
    element_pixel_source(&TextureRenderElement::from_texture_render_buffer(
        (0., 0.),
        texture,
        None,
        None,
        None,
        Kind::Unspecified,
    ))
}

fn element_pixel_source(element: &TextureRenderElement<GlesTexture>) -> Rectangle<f64, Logical> {
    let source = element.src();
    Rectangle::new(
        (source.loc.x, source.loc.y).into(),
        (source.size.w, source.size.h).into(),
    )
}

#[cfg(test)]
pub(crate) fn collect_blur_requests(state: &RixState) -> HashMap<Id, BlurRequest> {
    collect_blur_requests_with_effects(state, state.decoration_theme.material_effects())
}
fn collect_blur_requests_with_effects(
    state: &RixState,
    global_effects: rix_config::appearance::MaterialEffects,
) -> HashMap<Id, BlurRequest> {
    let mut requests = HashMap::new();
    let mut visited = HashSet::new();
    for (root, effects) in state
        .layer_surfaces
        .iter()
        .map(|layer| (layer.wl_surface().clone(), global_effects))
        .chain(state.space.elements().filter_map(|window| {
            window.toplevel().map(|surface| {
                (
                    surface.wl_surface().clone(),
                    state.decoration_for_window(window).material_effects(),
                )
            })
        }))
    {
        let roots = std::iter::once(root.clone()).chain(
            smithay::desktop::PopupManager::popups_for_surface(&root)
                .map(|(popup, _)| popup.wl_surface().clone()),
        );
        for root in roots {
            let mut surfaces = Vec::new();
            smithay::wayland::compositor::with_surface_tree_downward(
                &root,
                (),
                |_, _, _| smithay::wayland::compositor::TraversalAction::DoChildren(()),
                |surface, _, _| {
                    if visited.insert(surface.clone()) {
                        surfaces.push(surface.clone());
                    }
                },
                |_, _, _| true,
            );

            for surface in surfaces {
                if let Some(region) = crate::blur::region(&surface) {
                    requests.insert(
                        Id::from_wayland_resource(&surface),
                        (region, crate::surface_effects::shapes(&surface), effects),
                    );
                }
            }
        }
    }
    requests
}

struct BackdropRequest {
    region: Option<smithay::wayland::compositor::RegionAttributes>,
    shapes: Option<Vec<crate::surface_effects::Shape>>,
    geometry: Option<Rectangle<i32, Physical>>,
    effects: rix_config::appearance::MaterialEffects,
    client_clip: Option<(rounded::ClientClip, bool)>,
}
enum PresentedSurface {
    Live(SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>),
    Retained(motion::AnimatedTexture),
}

fn present_retained(
    elements: Vec<SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>>,
    motion: &mut motion::Frame,
    state: &RixState,
    hidden: &HashSet<Id>,
) -> Vec<PresentedSurface> {
    if motion.retained.is_empty() {
        return elements.into_iter().map(PresentedSurface::Live).collect();
    }
    let mut ranks = HashMap::new();
    let mut window_ranks = HashMap::new();
    for (rank, window) in state.space.elements().rev().enumerate() {
        let Some((id, _)) = state
            .windows
            .entries
            .iter()
            .find(|(_, known)| known == window)
        else {
            continue;
        };
        window_ranks.insert(*id, rank);
        let Some(top) = window.toplevel() else {
            continue;
        };
        for root in std::iter::once(top.wl_surface().clone()).chain(
            smithay::desktop::PopupManager::popups_for_surface(top.wl_surface())
                .map(|(popup, _)| popup.wl_surface().clone()),
        ) {
            smithay::wayland::compositor::with_surface_tree_downward(
                &root,
                (),
                |_, _, _| smithay::wayland::compositor::TraversalAction::DoChildren(()),
                |surface, _, _| {
                    ranks.insert(Id::from_wayland_resource(surface), rank);
                },
                |_, _, _| true,
            );
        }
    }
    let mut lower = HashSet::new();
    for layer in state
        .layer_surfaces
        .iter()
        .filter(|layer| matches!(layer.layer(), Layer::Bottom | Layer::Background))
    {
        smithay::wayland::compositor::with_surface_tree_downward(
            layer.wl_surface(),
            (),
            |_, _, _| smithay::wayland::compositor::TraversalAction::DoChildren(()),
            |surface, _, _| {
                lower.insert(Id::from_wayland_resource(surface));
            },
            |_, _, _| true,
        );
    }
    let mut pending: Vec<_> = motion
        .retained
        .drain()
        .filter_map(|(id, surfaces)| {
            let root = state
                .windows
                .entries
                .iter()
                .find(|(window, _)| *window == id)?
                .1
                .toplevel()?
                .wl_surface();
            if hidden.contains(&Id::from_wayland_resource(root)) {
                return None;
            }
            Some((*window_ranks.get(&id)?, surfaces))
        })
        .collect();
    pending.sort_by_key(|(rank, _)| *rank);
    let replaced: HashSet<_> = pending.iter().map(|(rank, _)| *rank).collect();
    let mut pending = pending.into_iter().peekable();
    let mut result = Vec::new();
    for element in elements {
        let rank = ranks.get(element.id()).copied();
        let boundary = rank.unwrap_or(if lower.contains(element.id()) {
            usize::MAX
        } else {
            0
        });
        while pending.peek().is_some_and(|(rank, _)| *rank <= boundary)
            && (rank.is_some() || lower.contains(element.id()))
        {
            let (_, surfaces) = pending.next().unwrap();
            for id in surfaces {
                if let Some(element) = motion.live.remove(&id).filter(|_| !hidden.contains(&id)) {
                    result.push(PresentedSurface::Retained(element));
                }
            }
        }
        if rank.is_none_or(|rank| !replaced.contains(&rank)) {
            result.push(PresentedSurface::Live(element));
        }
    }
    for (_, surfaces) in pending {
        for id in surfaces {
            if let Some(element) = motion.live.remove(&id).filter(|_| !hidden.contains(&id)) {
                result.push(PresentedSurface::Retained(element));
            }
        }
    }
    result
}
fn effects_enabled(effects: &rix_config::appearance::MaterialEffects) -> bool {
    effects.blur_radius > 0
        || effects.noise_opacity > 0.
        || effects.options.brightness != 1.
        || effects.options.contrast != 1.
        || effects.options.saturation != 1.
}

fn request_rectangles(
    request: &BackdropRequest,
    geometry: Rectangle<i32, Physical>,
    scale: f64,
) -> Vec<Rectangle<i32, Physical>> {
    if let Some(shapes) = &request.shapes {
        return shapes
            .iter()
            .filter(|shape| shape.opacity > 0.)
            .filter_map(|shape| {
                let x = geometry.loc.x as f64 + shape.x * scale;
                let y = geometry.loc.y as f64 + shape.y * scale;
                let rectangle = Rectangle::new(
                    (x.floor() as i32, y.floor() as i32).into(),
                    (
                        ((x + shape.width * scale).ceil() - x.floor()).max(0.) as i32,
                        ((y + shape.height * scale).ceil() - y.floor()).max(0.) as i32,
                    )
                        .into(),
                );
                if let Some(clip) = shape.clip {
                    let clip = Rectangle::new(
                        (
                            geometry.loc.x as f64 + clip.x * scale,
                            geometry.loc.y as f64 + clip.y * scale,
                        )
                            .into(),
                        (clip.width * scale, clip.height * scale).into(),
                    )
                    .to_i32_up();
                    rectangle.intersection(clip)
                } else {
                    Some(rectangle)
                }
            })
            .collect();
    }
    let local_bounds: Rectangle<i32, Logical> =
        Rectangle::from_size(geometry.size.to_f64().to_logical(scale).to_i32_round());
    let mut rectangles = Vec::new();
    if let Some(region) = &request.region {
        for (kind, rect) in &region.rects {
            match kind {
                RectangleKind::Add => {
                    if let Some(rect) = rect.intersection(local_bounds) {
                        rectangles.extend(rect.subtract_rects(rectangles.clone()));
                    }
                }
                RectangleKind::Subtract => {
                    rectangles = Rectangle::subtract_rects_many(rectangles, [*rect])
                }
            }
        }
    } else {
        rectangles.push(local_bounds);
    }
    rectangles
        .into_iter()
        .map(|rect| {
            Rectangle::new(
                geometry.loc + rect.loc.to_physical_precise_round(scale),
                rect.size.to_physical_precise_round(scale),
            )
        })
        .collect()
}

fn backdrop_region(
    rectangles: &[Rectangle<i32, Physical>],
    size: Size<i32, Physical>,
    scale: f64,
    effects: rix_config::appearance::MaterialEffects,
) -> Option<Rectangle<i32, Physical>> {
    use rix_config::appearance::BlurAlgorithm;
    let output = Rectangle::from_size(size);
    let visible = rectangles
        .iter()
        .filter_map(|rectangle| rectangle.intersection(output))
        .reduce(|bounds, rectangle| bounds.merge(rectangle))?;
    let (halo, alignment) = if effects.blur_radius == 0 {
        (1, 1)
    } else if effects.options.algorithm == BlurAlgorithm::Gaussian {
        let reduction = effects.options.gaussian_downsample.max(1) as i32;
        (
            ((effects.blur_radius as f64 * scale).min(128. * reduction as f64)
                + 2. * reduction as f64)
                .ceil() as i32,
            reduction,
        )
    } else {
        let alignment = 1i32 << effects.options.iterations.min(6);
        (
            ((2.5 * effects.options.offset as f64 + 2.) * (alignment - 1) as f64 + 2.).ceil()
                as i32,
            alignment,
        )
    };
    let x = visible.loc.x.saturating_sub(halo).max(0) / alignment * alignment;
    let y = visible.loc.y.saturating_sub(halo).max(0) / alignment * alignment;
    let end_x = visible
        .loc
        .x
        .saturating_add(visible.size.w)
        .saturating_add(halo)
        .saturating_add(alignment - 1)
        / alignment
        * alignment;
    let end_y = visible
        .loc
        .y
        .saturating_add(visible.size.h)
        .saturating_add(halo)
        .saturating_add(alignment - 1)
        / alignment
        * alignment;
    Some(Rectangle::new(
        (x, y).into(),
        (end_x.min(size.w) - x, end_y.min(size.h) - y).into(),
    ))
}

fn retained_blur_reduction(effects: rix_config::appearance::MaterialEffects) -> i32 {
    use rix_config::appearance::BlurAlgorithm;
    if effects.blur_radius == 0 {
        return 1;
    }
    match effects.options.algorithm {
        BlurAlgorithm::Gaussian => effects.options.gaussian_downsample.max(1) as i32,
        BlurAlgorithm::DualKawase => 1 << effects.options.iterations.min(2),
    }
}

fn retained_blur_size(
    region: Size<i32, Physical>,
    effects: rix_config::appearance::MaterialEffects,
) -> Size<i32, Physical> {
    let reduction = retained_blur_reduction(effects);
    (
        (region.w.max(0) + reduction - 1) / reduction,
        (region.h.max(0) + reduction - 1) / reduction,
    )
        .into()
}

fn draw_grain(effects: rix_config::appearance::MaterialEffects, scale: f64) -> [f32; 3] {
    [
        effects.noise_opacity,
        effects.options.noise_scale * scale as f32,
        effects.options.noise_bias,
    ]
}

fn retained_blur_bytes(
    region: Size<i32, Physical>,
    effects: rix_config::appearance::MaterialEffects,
) -> (u64, u64) {
    use rix_config::appearance::BlurAlgorithm;
    let retained = retained_blur_size(region, effects);
    let image = retained.w.max(0) as u64 * retained.h.max(0) as u64 * 4;
    let scratch =
        if effects.blur_radius > 0 && effects.options.algorithm == BlurAlgorithm::DualKawase {
            region.w.max(0) as u64 * region.h.max(0) as u64 * 4 * 2
        } else {
            image * 2
        };
    (image, scratch)
}

fn eviction_candidate(candidates: &[(Id, u32, u64)]) -> Option<Id> {
    let mut best: Option<(u32, u64, Id)> = None;
    for (id, tier, last_used) in candidates {
        if *tier == u32::MAX {
            continue;
        }
        let better = best.as_ref().is_none_or(|(btier, blast, _)| {
            *tier < *btier || (*tier == *btier && *last_used < *blast)
        });
        if better {
            best = Some((*tier, *last_used, id.clone()));
        }
    }
    best.map(|(_, _, id)| id)
}

render_elements! {
    pub SceneElement<=GlesRenderer>;
    Surface=SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
    Lock=WaylandSurfaceRenderElement<GlesRenderer>,
    DragIcon=drag_motion::LiveIcon,
    Texture=TextureRenderElement<GlesTexture>,
    Solid=SolidColorRenderElement,
    Animated=motion::AnimatedTexture,
    Rounded=rounded::Clipped<SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>>,
    AnimatedRounded=rounded::Clipped<motion::AnimatedTexture>,
    TextureRounded=rounded::Clipped<TextureRenderElement<GlesTexture>>,
    Masked=coverage::Masked,
    BackdropMasked=backdrop_budget::Budgeted<coverage::Masked>,
    BackdropTexture=backdrop_budget::Budgeted<RescaleRenderElement<TextureShaderElement>>,
    BackdropRounded=backdrop_budget::Budgeted<rounded::Clipped<RescaleRenderElement<TextureRenderElement<GlesTexture>>>>,
    Relocated=RelocateRenderElement<SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>>,
}

#[derive(Clone, PartialEq)]
struct Stamp {
    id: Id,
    commit: CommitCounter,
    geometry: Rectangle<i32, Physical>,
    source: Rectangle<f64, Buffer>,
    transform: Transform,
    alpha: f32,
}
#[derive(Clone, PartialEq)]
struct CacheKey {
    below: Vec<Stamp>,
    size: Size<i32, Physical>,
    region: Rectangle<i32, Physical>,
    scale: f64,
    effects: rix_config::appearance::MaterialEffects,
}
impl CacheKey {

    fn can_retain_for(&self, next: &Self) -> bool {
        self.size == next.size
            && self.region == next.region
            && self.scale == next.scale
            && self.effects == next.effects
            && self.below.len() == next.below.len()
            && self.below.iter().zip(&next.below).all(|(old, new)| {
                old.id == new.id
                    && old.geometry == new.geometry
                    && old.source == new.source
                    && old.transform == new.transform
                    && old.alpha == new.alpha
            })
    }
}
struct CachedBlur {
    key: CacheKey,
    layout: BackdropLayout,
    element_ids: Vec<Id>,
    commit: CommitCounter,
    image: TextureBuffer<GlesTexture>,

    image_texture: GlesTexture,
    coverage: Option<CachedCoverage>,
    lease: std::rc::Rc<backdrop_budget::Lease>,
    last_used: u64,
}

#[derive(PartialEq)]
struct BackdropLayout {
    geometry: Rectangle<i32, Physical>,
    rectangles: Vec<Rectangle<i32, Physical>>,
    shapes: Option<Vec<crate::surface_effects::Shape>>,
    client_clip: Option<(rounded::ClientClip, bool)>,
}
struct CachedCoverage {
    shapes: Vec<crate::surface_effects::Shape>,
    geometry: Rectangle<i32, Physical>,
    element: coverage::Masked,
    client_clip: Option<(rounded::ClientClip, bool)>,
}
#[derive(PartialEq)]
struct SnapPreviewKey {
    output_name: String,
    geometry: Rectangle<i32, Logical>,
    output_size: Size<i32, Physical>,
    scale: f64,
    accent: [u8; 3],
    fill_opacity: f32,
    transition_alpha: f32,
    radius: f64,
}
#[derive(Default)]
pub struct SceneRenderer {

    capture: Option<Box<SceneRenderer>>,
    lock_backgrounds: HashMap<String, SolidColorBuffer>,
    decorations: crate::decoration::DecorationRenderer,
    decoration_anchors: HashMap<u64, SolidColorBuffer>,
    selection: [SolidColorBuffer; 5],
    motion: motion::Motion,
    drag_motion: drag_motion::DragMotion,
    program: Option<GlesTexProgram>,
    coverage_program: Option<GlesTexProgram>,
    client_clip_program: Option<GlesTexProgram>,
    client_clips: rounded::Cache,
    snap_preview_texture: Option<([u8; 3], TextureBuffer<GlesTexture>)>,
    snap_preview_element: Option<(SnapPreviewKey, coverage::Masked)>,
    cache: HashMap<(String, Id), CachedBlur>,
    blur_budget: backdrop_budget::Budget,
    blur_clock: u64,

    blur_skip_warns: HashMap<(String, Id), u64>,
    blur_scratch: Vec<(SyncPoint, std::rc::Rc<backdrop_budget::Lease>)>,

    blur_pools: HashMap<(i32, i32), Vec<GlesTexture>>,
}

const BLUR_SCRATCH_PER_SIZE: usize = 2;
const BLUR_SCRATCH_SIZES: usize = 8;
const BLUR_SCRATCH_TOTAL: usize = 12;

fn retained_scratch(pool_len: usize, distinct_sizes: usize, new_size: bool, total: usize) -> bool {
    total < BLUR_SCRATCH_TOTAL
        && pool_len < BLUR_SCRATCH_PER_SIZE
        && (!new_size || distinct_sizes < BLUR_SCRATCH_SIZES)
}

impl SceneRenderer {
    fn take_scratch(
        &mut self,
        renderer: &mut GlesRenderer,
        size: (i32, i32),
    ) -> anyhow::Result<GlesTexture> {
        if let Some(texture) = self.blur_pools.get_mut(&size).and_then(Vec::pop) {
            return Ok(texture);
        }
        Ok(renderer.create_buffer(Fourcc::Abgr8888, size.into())?)
    }
    fn release_scratch(&mut self, size: (i32, i32), texture: GlesTexture) {
        let total: usize = self.blur_pools.values().map(Vec::len).sum();
        let admitted = retained_scratch(
            self.blur_pools.get(&size).map_or(0, Vec::len),
            self.blur_pools.len(),
            !self.blur_pools.contains_key(&size),
            total,
        );
        if admitted {
            self.blur_pools.entry(size).or_default().push(texture);
        }
    }
    fn release_finished_blur_scratch(&mut self) {
        self.blur_scratch.retain(|(sync, _)| !sync.is_reached());
        if let Some(capture) = &mut self.capture {
            capture.release_finished_blur_scratch();
        }
    }

    fn maybe_warn_blur_skip(&mut self, cache_id: &(String, Id), used: u64, limit: u64) {
        const BLUR_SKIP_WARN_FRAMES: u64 = 120;
        const BLUR_SKIP_WARN_SLOTS: usize = 64;
        let clock = self.blur_clock;

        self.blur_skip_warns
            .retain(|_, last| clock.wrapping_sub(*last) < 4 * BLUR_SKIP_WARN_FRAMES);
        if self
            .blur_skip_warns
            .get(cache_id)
            .is_some_and(|last| clock.wrapping_sub(*last) < BLUR_SKIP_WARN_FRAMES)
        {
            return;
        }
        self.blur_skip_warns.insert(cache_id.clone(), clock);
        while self.blur_skip_warns.len() > BLUR_SKIP_WARN_SLOTS {
            let Some(oldest) = self
                .blur_skip_warns
                .iter()
                .min_by_key(|(_, last)| clock.wrapping_sub(**last))
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.blur_skip_warns.remove(&oldest);
        }
        let (output, id) = cache_id;
        tracing::warn!(
            output = %output,
            element = ?id,
            "Retained-blur budget exhausted ({used} of {limit} bytes): element is rendered sharp until memory frees or its layout stops changing"
        );
    }

    pub fn compose_fullscreen(
        &mut self,
        renderer: &mut GlesRenderer,
        state: &RixState,
        output: &Output,
        wallpaper: Option<&TextureRenderBuffer<GlesTexture>>,
    ) -> anyhow::Result<Option<Vec<SceneElement>>> {
        self.release_finished_blur_scratch();
        self.drag_motion.prune(state);
        if state.lock.active
            || state.selection_start.is_some()
            || state.snap_preview.is_some()
            || state.switcher.active()
            || state.drag.active
            || drag_motion::DragMotion::finishing(&state.drag, state.animation_duration)
        {
            return Ok(None);
        }
        let Some(output_geometry) = state.space.output_geometry(output) else {
            return Ok(None);
        };
        let Some(window) = state.space.elements().next_back() else {
            return Ok(None);
        };
        let fullscreen = window.toplevel().is_some_and(|toplevel| {
            toplevel
                .current_state()
                .states
                .contains(wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen)
        });
        if !fullscreen || state.space.element_geometry(window) != Some(output_geometry) {
            return Ok(None);
        }

        let layer_map = smithay::desktop::layer_map_for_output(output);
        let output_local = Rectangle::from_size(output_geometry.size);
        if layer_map.layers().any(|layer| {
            crate::toplevel_observer::visible(layer.wl_surface())
                && matches!(layer.layer(), Layer::Top | Layer::Overlay)
                && layer_map.layer_geometry(layer).is_some_and(|geometry| {
                    geometry.size.w > 0 && geometry.size.h > 0 && geometry.overlaps(output_local)
                })
        }) {
            return Ok(None);
        }

        self.cache.retain(|(name, _), _| name != &output.name());
        let scale = smithay::utils::Scale::from(output.current_scale().fractional_scale());
        let location = state
            .space
            .element_location(window)
            .unwrap_or(output_geometry.loc)
            - window.geometry().loc
            - output_geometry.loc;
        let mut elements: Vec<_> = window
            .render_elements(
                renderer,
                location.to_physical_precise_round(scale),
                scale,
                1.0,
            )
            .into_iter()
            .map(|element| SceneElement::Surface(SpaceRenderElements::Surface(element)))
            .collect();

        for layer in layer_map.layers().rev().filter(|layer| {
            matches!(layer.layer(), Layer::Background | Layer::Bottom)
                && crate::toplevel_observer::visible(layer.wl_surface())
        }) {
            let Some(geometry) = layer_map.layer_geometry(layer) else {
                continue;
            };
            let location = geometry.loc.to_physical_precise_round(scale);
            elements.extend(
                layer
                    .render_elements(renderer, location, scale, 1.0)
                    .into_iter()
                    .map(|element| SceneElement::Surface(SpaceRenderElements::Surface(element))),
            );
        }

        if let (Some(wallpaper), Some(mode)) = (wallpaper, output.current_mode()) {
            let physical_size = output.current_transform().transform_size(mode.size);
            let logical_size = physical_size.to_f64().to_logical(scale).to_i32_round();
            elements.push(SceneElement::Texture(
                TextureRenderElement::from_texture_render_buffer(
                    (0., 0.),
                    wallpaper,
                    Some(1.),
                    Some(wallpaper_pixel_source(wallpaper)),
                    Some(logical_size),
                    Kind::Unspecified,
                ),
            ));
        }
        Ok(Some(elements))
    }

    pub fn compose(
        &mut self,
        renderer: &mut GlesRenderer,
        state: &RixState,
        output: &Output,
        wallpaper: Option<&TextureRenderBuffer<GlesTexture>>,
        theme: &rix_config::appearance::MaterialEffects,
    ) -> anyhow::Result<Vec<SceneElement>> {
        self.compose_inner(renderer, state, output, wallpaper, theme, None)
    }

    pub fn compose_capture(
        &mut self,
        renderer: &mut GlesRenderer,
        state: &RixState,
        output: &Output,
        wallpaper: Option<&TextureRenderBuffer<GlesTexture>>,
        theme: &rix_config::appearance::MaterialEffects,
    ) -> anyhow::Result<Vec<SceneElement>> {
        let filter = crate::capture_policy::CaptureFilter::for_state(state);
        let blur_budget = self.blur_budget.clone();
        self.capture
            .get_or_insert_with(|| {
                Box::new(Self {
                    blur_budget,
                    ..Self::default()
                })
            })
            .compose_inner(renderer, state, output, wallpaper, theme, Some(&filter))
    }

    fn compose_inner(
        &mut self,
        renderer: &mut GlesRenderer,
        state: &RixState,
        output: &Output,
        wallpaper: Option<&TextureRenderBuffer<GlesTexture>>,
        theme: &rix_config::appearance::MaterialEffects,
        capture: Option<&crate::capture_policy::CaptureFilter>,
    ) -> anyhow::Result<Vec<SceneElement>> {
        self.release_finished_blur_scratch();
        self.cache
            .retain(|(name, _), _| state.space.outputs().any(|output| output.name() == *name));
        let scale = output.current_scale().fractional_scale();
        let mode = output
            .current_mode()
            .ok_or_else(|| anyhow::anyhow!("Output has no mode"))?;
        let size = output.current_transform().transform_size(mode.size);
        if state.lock.active {
            self.cache.clear();
            self.drag_motion.prune(state);
            let mut elements: Vec<_> = state
                .lock
                .surfaces
                .iter()
                .filter(|(o, surface)| o == output && surface.alive())
                .flat_map(|(_, surface)| {
                    smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                        renderer,
                        surface.wl_surface(),
                        (0, 0),
                        scale,
                        1.,
                        Kind::Unspecified,
                    )
                })
                .filter(|element| capture.is_none_or(|filter| filter.allows(element)))
                .map(SceneElement::Lock)
                .collect();
            let background = self.lock_backgrounds.entry(output.name()).or_default();
            background.update(
                size.to_f64().to_logical(scale).to_i32_ceil(),
                [0., 0., 0., 1.],
            );
            elements.push(SceneElement::Solid(SolidColorRenderElement::from_buffer(
                background,
                (0, 0),
                scale,
                1.,
                Kind::Unspecified,
            )));
            return Ok(elements);
        }
        if state.window_snap_settings.maximize_at_top_edge {
            let accent = [
                (state.selection_color[0].clamp(0.0, 1.0) * 255.0).round() as u8,
                (state.selection_color[1].clamp(0.0, 1.0) * 255.0).round() as u8,
                (state.selection_color[2].clamp(0.0, 1.0) * 255.0).round() as u8,
            ];
            if self
                .snap_preview_texture
                .as_ref()
                .is_none_or(|(color, _)| *color != accent)
            {
                self.snap_preview_texture = Some((
                    accent,
                    TextureBuffer::from_memory(
                        renderer,
                        &[accent[0], accent[1], accent[2], 255],
                        Fourcc::Abgr8888,
                        (1, 1),
                        false,
                        1,
                        Transform::Normal,
                        None,
                    )?,
                ));
            }
            if self.coverage_program.is_none() {
                self.coverage_program = Some(coverage::program(renderer)?);
            }
        }
        let mut layer_offsets = HashMap::new();
        for layer in &state.layer_surfaces {
            let offset = crate::layer_layout::offset(layer);
            if offset == (0, 0).into() {
                continue;
            }
            smithay::wayland::compositor::with_surface_tree_downward(
                layer.wl_surface(),
                (),
                |_, _, _| smithay::wayland::compositor::TraversalAction::DoChildren(()),
                |surface, _, _| {
                    layer_offsets.insert(
                        Id::from_wayland_resource(surface),
                        offset.to_physical_precise_round(scale),
                    );
                },
                |_, _, _| true,
            );
        }
        let mut decorations = self.decorations.elements(renderer, state, output);
        let mut decoration_backdrops: HashMap<_, _> = state
            .windows
            .entries
            .iter()
            .filter_map(|(id, window)| {
                let surface = window.toplevel()?.wl_surface();
                Some((
                    Id::from_wayland_resource(surface),
                    (*id, self.decorations.backdrop_snapshot(*id)?),
                ))
            })
            .collect();
        let mut own_requests = HashMap::new();
        let output_origin = state.space.output_geometry(output).unwrap().loc;
        let mut client_clips = HashMap::new();
        for window in state
            .space
            .elements()
            .filter(|window| crate::decoration::enabled(window))
        {
            let Some(body) = state.space.element_geometry(window) else {
                continue;
            };
            let radius = state.decoration_for_window(window).client_corner_radius();
            if radius <= 0. {
                continue;
            }
            let clip = rounded::ClientClip {
                bounds: Rectangle::new((body.loc - output_origin).to_f64(), body.size.to_f64()),
                radius,
            };
            client_clips.extend(
                rounded::client_surface_ids(window)
                    .into_iter()
                    .map(|id| (id, clip)),
            );
        }
        let hidden = crate::toplevel_observer::hidden_elements(state);
        let mut motion = if capture.is_some() {
            motion::Frame::default()
        } else {
            self.motion
                .frame(renderer, state, output, &self.decorations)
        };
        client_clips.extend(motion.clips.iter().map(|(id, clip)| (id.clone(), *clip)));
        if !client_clips.is_empty() && self.client_clip_program.is_none() {
            self.client_clip_program = Some(rounded::program(renderer)?);
        }
        let elements = space_render_elements(renderer, [&state.space], output, 1.)?
            .into_iter()
            .filter(|element| !hidden.contains(element.id()))
            .filter(|element| capture.is_none_or(|filter| filter.allows(element)))
            .collect();
        let mut scene: Vec<_> = present_retained(elements, &mut motion, state, &hidden)
            .into_iter()
            .map(|presented| {
                let element = match presented {
                    PresentedSurface::Retained(animated) => {
                        return if let Some(clip) = client_clips.get(animated.id()) {
                            SceneElement::AnimatedRounded(self.client_clips.wrap(
                                animated,
                                *clip,
                                self.client_clip_program.as_ref().unwrap(),
                                scale,
                            ))
                        } else {
                            SceneElement::Animated(animated)
                        };
                    }
                    PresentedSurface::Live(element) => element,
                };
                if let Some(animated) = motion.live.remove(element.id()) {
                    if let Some(clip) = client_clips.get(animated.id()) {
                        SceneElement::AnimatedRounded(self.client_clips.wrap(
                            animated,
                            *clip,
                            self.client_clip_program.as_ref().unwrap(),
                            scale,
                        ))
                    } else {
                        SceneElement::Animated(animated)
                    }
                } else if let Some(offset) = layer_offsets.get(element.id()) {
                    SceneElement::Relocated(RelocateRenderElement::from_element(
                        element,
                        *offset,
                        Relocate::Relative,
                    ))
                } else {
                    if let Some(clip) = client_clips.get(element.id()) {
                        SceneElement::Rounded(self.client_clips.wrap(
                            element,
                            *clip,
                            self.client_clip_program.as_ref().unwrap(),
                            scale,
                        ))
                    } else {
                        SceneElement::Surface(element)
                    }
                }
            })
            .collect();
        let closed_index = scene
            .iter()
            .position(|element| {
                matches!(
                    element,
                    SceneElement::Surface(SpaceRenderElements::Element(_))
                        | SceneElement::Animated(_)
                        | SceneElement::Rounded(_)
                        | SceneElement::AnimatedRounded(_)
                )
            })
            .unwrap_or(scene.len());
        let mut closed: Vec<_> = motion
            .closed
            .into_iter()
            .map(|element| {
                if let Some(clip) = client_clips.get(element.id()) {
                    SceneElement::AnimatedRounded(self.client_clips.wrap(
                        element,
                        *clip,
                        self.client_clip_program.as_ref().unwrap(),
                        scale,
                    ))
                } else {
                    SceneElement::Animated(element)
                }
            })
            .collect();
        let mut retiring = HashSet::new();
        for (index, id, backdrop) in motion.closed_backdrops.into_iter().rev() {
            retiring.insert(id);
            let marker = self.decoration_anchor(
                id,
                &backdrop,
                state.space.output_geometry(output).unwrap().loc,
                scale,
                &mut own_requests,
            );
            closed.insert(index, marker);
        }
        scene.splice(closed_index..closed_index, closed);
        for index in (0..scene.len()).rev() {
            let mut parts: Vec<_> =
                if let Some(decoration) = motion.decorations.remove(scene[index].id()) {
                    decoration.into_iter().map(SceneElement::Animated).collect()
                } else if let Some(decoration) = decorations.remove(scene[index].id()) {
                    decoration.into_iter().map(SceneElement::Texture).collect()
                } else {
                    continue;
                };
            if let Some((id, backdrop)) = decoration_backdrops.remove(scene[index].id()) {
                let backdrop = motion
                    .backdrops
                    .remove(scene[index].id())
                    .unwrap_or(backdrop);
                parts.push(self.decoration_anchor(
                    id,
                    &backdrop,
                    state.space.output_geometry(output).unwrap().loc,
                    scale,
                    &mut own_requests,
                ));
            }
            scene.splice(index + 1..index + 1, parts);
        }
        self.decoration_anchors.retain(|id, _| {
            retiring.contains(id) || state.windows.entries.iter().any(|(window, _)| window == id)
        });
        if let Some(preview) = state
            .snap_preview
            .as_ref()
            .filter(|preview| preview.output_name == output.name())
        {
            let duration = state.animation_duration;
            let progress = if duration == 0 {
                1.0
            } else {
                (preview.transition_started.elapsed().as_secs_f32() * 1000. / duration as f32)
                    .clamp(0.0, 1.0)
            };
            let eased = progress * progress * (3.0 - 2.0 * progress);
            let transition_alpha = if preview.visible { eased } else { 1.0 - eased };
            let opacity = state.window_snap_settings.preview_opacity * transition_alpha;
            if opacity > 0.001 {
                if let Some(output_geometry) = state.space.output_geometry(output) {
                    let radius = state.decoration_theme.corner_radius().max(0.0) as f64;
                    let accent = self
                        .snap_preview_texture
                        .as_ref()
                        .map(|(accent, _)| *accent)
                        .unwrap_or_default();
                    let key = SnapPreviewKey {
                        output_name: preview.output_name.clone(),
                        geometry: preview.geometry,
                        output_size: size,
                        scale,
                        accent,
                        fill_opacity: state.window_snap_settings.preview_opacity,
                        transition_alpha,
                        radius,
                    };
                    if self
                        .snap_preview_element
                        .as_ref()
                        .is_none_or(|(cached_key, _)| *cached_key != key)
                    {
                        let rectangle = &preview.geometry;
                        let shape = crate::surface_effects::Shape {
                            x: (rectangle.loc.x - output_geometry.loc.x) as f64,
                            y: (rectangle.loc.y - output_geometry.loc.y) as f64,
                            width: rectangle.size.w as f64,
                            height: rectangle.size.h as f64,
                            radius,
                            opacity: opacity as f64,
                            clip: None,
                        };
                        let top_edge = crate::surface_effects::Shape {
                            x: shape.x + 2.0,
                            y: shape.y,
                            width: (shape.width - 4.0).max(1.0),
                            height: 2.0,
                            radius: 1.0,
                            opacity: (transition_alpha * 0.72).min(0.82) as f64,
                            clip: None,
                        };
                        if let Some((_, texture)) = &self.snap_preview_texture {
                            let element = coverage::element(
                                texture,
                                self.coverage_program.as_ref().unwrap(),
                                &[shape, top_edge],
                                Rectangle::from_size(size),
                                size,
                                scale,
                            );
                            self.snap_preview_element = Some((key, element));
                        }
                    }
                    if let Some((_, element)) = &self.snap_preview_element {
                        scene.insert(0, SceneElement::Masked(element.clone()));
                    }
                }
            }
        }
        if let (Some(start), Some(pointer), Some(output_geo)) = (
            state.selection_start,
            state.seat.get_pointer(),
            state.space.output_geometry(output),
        ) {
            let end = pointer.current_location();
            let x = start.x.min(end.x).round() as i32;
            let y = start.y.min(end.y).round() as i32;
            let width = (start.x - end.x).abs().round().max(1.) as i32;
            let height = (start.y - end.y).abs().round().max(1.) as i32;
            for (i, (x, y, w, h)) in [
                (x, y, width, 1),
                (x, y + height - 1, width, 1),
                (x, y, 1, height),
                (x + width - 1, y, 1, height),
                (x, y, width, height),
            ]
            .into_iter()
            .enumerate()
            {
                self.selection[i].update((w, h), state.selection_color);
                scene.push(SceneElement::Solid(SolidColorRenderElement::from_buffer(
                    &self.selection[i],
                    (smithay::utils::Point::from((x, y)) - output_geo.loc)
                        .to_physical_precise_round(scale),
                    scale,
                    if i == 4 { 0.16 } else { 0.85 },
                    Kind::Unspecified,
                )));
            }
        }
        if let Some(wallpaper) = wallpaper {
            scene.push(SceneElement::Texture(
                TextureRenderElement::from_texture_render_buffer(
                    (0., 0.),
                    wallpaper,
                    Some(1.),
                    Some(wallpaper_pixel_source(wallpaper)),
                    Some(size.to_f64().to_logical(scale).to_i32_round()),
                    Kind::Unspecified,
                ),
            ));
        }
        let mut requests: HashMap<_, _> = collect_blur_requests_with_effects(state, *theme)
            .into_iter()
            .map(|(id, (region, shapes, effects))| {
                let client_clip = client_clips.get(&id).map(|clip| (*clip, false));
                (
                    id,
                    BackdropRequest {
                        region,
                        shapes,
                        geometry: None,
                        effects,
                        client_clip,
                    },
                )
            })
            .collect();
        requests.extend(own_requests);
        let mut scene =
            self.apply_backdrops(renderer, scene, requests, size, scale, &output.name())?;
        if capture.is_some() {
            scene.splice(
                0..0,
                crate::drag_drop::icon_elements(renderer, state, output, true)
                    .into_iter()
                    .map(SceneElement::Lock),
            );
        } else {
            scene.splice(
                0..0,
                self.drag_motion
                    .frame(renderer, state, output)
                    .into_iter()
                    .map(|element| match element {
                        drag_motion::IconElement::Live(element) => SceneElement::DragIcon(*element),
                        drag_motion::IconElement::Retained(element) => {
                            SceneElement::Animated(*element)
                        }
                    }),
            );
        }
        self.client_clips
            .retain(&scene.iter().map(|element| element.id().clone()).collect());
        Ok(scene)
    }

    fn reserve_blur(
        &mut self,
        image_bytes: u64,
        scratch_bytes: u64,
        output_name: &str,
        current: &Id,
        preserve: &HashSet<Id>,
    ) -> Option<(
        std::rc::Rc<backdrop_budget::Lease>,
        std::rc::Rc<backdrop_budget::Lease>,
    )> {
        self.release_finished_blur_scratch();
        loop {
            if let Some(image) = self.blur_budget.reserve(image_bytes) {

                if let Some(scratch) = self.blur_budget.reserve(scratch_bytes) {
                    return Some((image, scratch));
                }
            }

            let candidates: Vec<_> = self
                .cache
                .iter()
                .map(|((name, id), entry)| {
                    let tier = if name != output_name {
                        2
                    } else if id == current {
                        u32::MAX
                    } else if preserve.contains(id) {
                        1
                    } else {
                        0
                    };
                    (id.clone(), tier, entry.last_used)
                })
                .collect();
            let oldest = eviction_candidate(&candidates)?;
            self.cache.remove(&(output_name.to_owned(), oldest));
        }
    }

    fn apply_backdrops(
        &mut self,
        renderer: &mut GlesRenderer,
        mut scene: Vec<SceneElement>,
        mut requests: HashMap<Id, BackdropRequest>,
        size: Size<i32, Physical>,
        scale: f64,
        output_name: &str,
    ) -> anyhow::Result<Vec<SceneElement>> {
        self.release_finished_blur_scratch();
        requests.retain(|_, request| effects_enabled(&request.effects));
        if requests.is_empty() {
            self.cache.retain(|(name, _), _| name != output_name);
            return Ok(scene);
        }

        let preserve = requests.keys().cloned().collect();
        let mut used = HashSet::new();

        for index in (0..scene.len()).rev() {
            let Some(request) = requests.get(scene[index].id()) else {
                continue;
            };
            let geometry = request
                .geometry
                .unwrap_or_else(|| scene[index].geometry(scale.into()));
            let rectangles = request_rectangles(request, geometry, scale);
            let Some(region) = backdrop_region(&rectangles, size, scale, request.effects) else {
                continue;
            };

            if scene[..index].iter().any(|element| {
                let origin = element.geometry(scale.into()).loc;
                element.opaque_regions(scale.into()).iter().any(|opaque| {
                    Rectangle::new(opaque.loc + origin, opaque.size).contains_rect(region)
                })
            }) {
                continue;
            }
            let cache_id = (output_name.to_owned(), scene[index].id().clone());
            let layout = BackdropLayout {
                geometry,
                rectangles: rectangles.clone(),
                shapes: request.shapes.clone(),
                client_clip: request.client_clip,
            };
            let key = CacheKey {
                below: scene[index + 1..]
                    .iter()
                    .filter(|element| element.geometry(scale.into()).overlaps(region))
                    .map(|element| Stamp {
                        id: element.id().clone(),
                        commit: element.current_commit(),
                        geometry: element.geometry(scale.into()),
                        source: element.src(),
                        transform: element.transform(),
                        alpha: element.alpha(),
                    })
                    .collect(),
                size,
                region,
                scale,
                effects: request.effects,
            };
            self.blur_clock = self.blur_clock.wrapping_add(1);
            if !self
                .cache
                .get(&cache_id)
                .is_some_and(|entry| entry.key == key && entry.layout == layout)
            {
                let retain = self
                    .cache
                    .get(&cache_id)
                    .is_some_and(|entry| entry.key.can_retain_for(&key) && entry.layout == layout);
                let reuse = if !retain {
                    self.cache.remove(&cache_id).map(|entry| entry.image_texture)
                } else {
                    self.cache
                        .get(&cache_id)
                        .map(|entry| entry.image_texture.clone())
                };
                let (image_bytes, scratch_bytes) =
                    retained_blur_bytes(region.size, request.effects);
                if let Some((lease, scratch)) = self.reserve_blur(
                    image_bytes,
                    scratch_bytes,
                    output_name,
                    scene[index].id(),
                    &preserve,
                ) {

                    let blurred = self.blur(renderer, &scene[index + 1..], &key, reuse);
                    let (image, image_texture, sync) = match blurred {
                        Ok(value) => value,
                        Err(error) => {
                            self.cache.remove(&cache_id);
                            return Err(error);
                        }
                    };

                    if !sync.is_reached() {
                        self.blur_scratch.push((sync, scratch));
                    }

                    let (element_ids, mut commit) = self
                        .cache
                        .get(&cache_id)
                        .filter(|_| retain)
                        .map(|entry| (entry.element_ids.clone(), entry.commit))
                        .unwrap_or_else(|| {
                            let parts = if layout.shapes.is_some() {
                                1
                            } else {
                                layout.rectangles.len()
                            };
                            (
                                (0..parts).map(|_| Id::new()).collect(),
                                CommitCounter::default(),
                            )
                        });
                    commit.increment();
                    self.cache.insert(
                        cache_id.clone(),
                        CachedBlur {
                            key,
                            layout,
                            element_ids,
                            commit,
                            image,
                            image_texture,
                            coverage: None,
                            lease,
                            last_used: self.blur_clock,
                        },
                    );
                } else if !retain {

                    let used = self.blur_budget.used();
                    let limit = self.blur_budget.limit();
                    self.maybe_warn_blur_skip(&cache_id, used, limit);

                    continue;
                }

            }
            let cached = self.cache.get_mut(&cache_id).unwrap();
            cached.last_used = self.blur_clock;
            used.insert(cache_id.clone());
            if let Some(shapes) = &request.shapes {
                if self.coverage_program.is_none() {
                    self.coverage_program = Some(coverage::program(renderer)?);
                }
                if cached.coverage.as_ref().is_none_or(|old| {
                    old.shapes != *shapes
                        || old.geometry != geometry
                        || old.client_clip != request.client_clip
                }) {
                    let element = coverage::element_for_region(
                        &cached.image,
                        self.coverage_program.as_ref().unwrap(),
                        shapes,
                        geometry,
                        size,
                        region,
                        scale,
                        request.client_clip,
                        draw_grain(request.effects, scale),
                    );
                    cached.coverage = Some(CachedCoverage {
                        shapes: shapes.clone(),
                        geometry,
                        element,
                        client_clip: request.client_clip,
                    });
                }
                scene.insert(
                    index + 1,
                    SceneElement::BackdropMasked(backdrop_budget::Budgeted::new(
                        cached.coverage.as_ref().unwrap().element.clone(),
                        cached.element_ids[0].clone(),
                        cached.commit,
                        cached.lease.clone(),
                    )),
                );
                continue;
            }
            let image = &cached.image;
            let lease = cached.lease.clone();
            let commit = cached.commit;

            let reduction = retained_blur_reduction(request.effects) as f64;

            let grain = draw_grain(request.effects, scale);
            let finish_uniforms = vec![
                Uniform::new("step_size", [0., 0.]),
                Uniform::new("grain", grain),
                Uniform::new("grain_origin", [0., 0.]),
                Uniform::new("correction", [1., 1., 1.]),
                Uniform::new("pass_mode", 0.),
                Uniform::new("radius", 0.),
                Uniform::new("half_texel", [0., 0.]),
            ];
            let effects: Vec<_> = rectangles
                .into_iter()
                .enumerate()
                .filter_map(|(part, rectangle)| {
                    let clipped = rectangle.intersection(Rectangle::from_size(size))?;
                    let element = TextureRenderElement::from_texture_buffer(
                        clipped.loc.to_f64(),
                        image,
                        Some(1.),
                        Some(Rectangle::new(
                            (
                                (clipped.loc.x - region.loc.x) as f64 / reduction,
                                (clipped.loc.y - region.loc.y) as f64 / reduction,
                            )
                                .into(),
                            (
                                clipped.size.w as f64 / reduction,
                                clipped.size.h as f64 / reduction,
                            )
                                .into(),
                        )),
                        Some(clipped.size.to_f64().to_logical(scale).to_i32_round()),
                        Kind::Unspecified,
                    );
                    Some(if let Some((clip, false)) = request.client_clip {

                        let extent = element.geometry(scale.into()).size;
                        let element = RescaleRenderElement::from_element(
                            element,
                            clipped.loc,
                            smithay::utils::Scale::from((
                                clipped.size.w as f64 / extent.w.max(1) as f64,
                                clipped.size.h as f64 / extent.h.max(1) as f64,
                            )),
                        );
                        SceneElement::BackdropRounded(backdrop_budget::Budgeted::new(
                            self.client_clips
                                .wrap(
                                    element,
                                    clip,
                                    self.client_clip_program.as_ref().unwrap(),
                                    scale,
                                )
                                .with_grain(grain),
                            cached.element_ids[part].clone(),
                            commit,
                            lease.clone(),
                        ))
                    } else {
                        let program = self
                            .program
                            .as_ref()
                            .expect("a cached image is always blurred by the shared program");
                        let element = TextureShaderElement::new(
                            element,
                            program.clone(),
                            finish_uniforms.clone(),
                        );
                        let extent = element.geometry(scale.into()).size;
                        let element = RescaleRenderElement::from_element(
                            element,
                            clipped.loc,
                            smithay::utils::Scale::from((
                                clipped.size.w as f64 / extent.w.max(1) as f64,
                                clipped.size.h as f64 / extent.h.max(1) as f64,
                            )),
                        );
                        SceneElement::BackdropTexture(backdrop_budget::Budgeted::new(
                            element,
                            cached.element_ids[part].clone(),
                            commit,
                            lease.clone(),
                        ))
                    })
                })
                .collect();
            scene.splice(index + 1..index + 1, effects);
        }
        self.cache.retain(|(name, id), _| {
            name != output_name || used.contains(&(name.clone(), id.clone()))
        });
        Ok(scene)
    }
    fn decoration_anchor(
        &mut self,
        id: u64,
        backdrop: &crate::decoration::DecorationBackdrop,
        output_origin: smithay::utils::Point<i32, Logical>,
        scale: f64,
        requests: &mut HashMap<Id, BackdropRequest>,
    ) -> SceneElement {
        let buffer = self.decoration_anchors.entry(id).or_default();
        buffer.update(backdrop.bounds.size, [0.; 4]);
        let marker = SolidColorRenderElement::from_buffer(
            buffer,
            (backdrop.bounds.loc - output_origin).to_physical_precise_round(scale),
            scale,
            1.,
            Kind::Unspecified,
        );
        requests.insert(
            marker.id().clone(),
            BackdropRequest {
                region: None,
                shapes: Some(backdrop.shapes()),
                geometry: Some(marker.geometry(scale.into())),
                effects: backdrop.effects,
                client_clip: Some((
                    rounded::ClientClip {
                        bounds: Rectangle::new(
                            (backdrop.body.loc - output_origin).to_f64(),
                            backdrop.body.size.to_f64(),
                        ),
                        radius: backdrop.client_radius,
                    },
                    true,
                )),
            },
        );
        SceneElement::Solid(marker)
    }
    fn blur(
        &mut self,
        renderer: &mut GlesRenderer,
        below: &[SceneElement],
        key: &CacheKey,
        reuse: Option<GlesTexture>,
    ) -> anyhow::Result<(TextureBuffer<GlesTexture>, GlesTexture, SyncPoint)> {
        if self.program.is_none() {
            self.program = Some(renderer.compile_custom_texture_shader(
                include_str!("blur.frag"),
                &[
                    UniformName::new("step_size", UniformType::_2f),
                    UniformName::new("grain", UniformType::_3f),
                    UniformName::new("grain_origin", UniformType::_2f),
                    UniformName::new("correction", UniformType::_3f),
                    UniformName::new("pass_mode", UniformType::_1f),
                    UniformName::new("radius", UniformType::_1f),
                    UniformName::new("half_texel", UniformType::_2f),
                ],
            )?);
        }
        use rix_config::appearance::BlurAlgorithm;
        let effects = key.effects;
        let options = effects.options;
        let gaussian = options.algorithm == BlurAlgorithm::Gaussian;
        let reduction = if gaussian && effects.blur_radius > 0 {
            options.gaussian_downsample
        } else {
            1
        };
        let capture: Size<i32, Physical> = (
            (key.region.size.w + reduction as i32 - 1) / reduction as i32,
            (key.region.size.h + reduction as i32 - 1) / reduction as i32,
        )
            .into();
        let capture_size = (capture.w, capture.h);
        let mut source: GlesTexture = self.take_scratch(renderer, capture_size)?;
        {
            let mut fb = renderer.bind(&mut source)?;
            let reduced: Vec<_> = below
                .iter()
                .map(|e| {
                    RescaleRenderElement::from_element(
                        RelocateRenderElement::from_element(
                            e,
                            Point::from((-key.region.loc.x, -key.region.loc.y)),
                            Relocate::Relative,
                        ),
                        (0, 0).into(),
                        1. / reduction as f64,
                    )
                })
                .collect();
            let mut damage = OutputDamageTracker::new(capture, key.scale, Transform::Normal);
            damage.render_output(renderer, &mut fb, 0, &reduced, [0.08, 0.08, 0.1, 1.])?;
        }

        let program = self.program.clone().unwrap();

        if effects.blur_radius > 0 {
            if gaussian {
                let radius =
                    (effects.blur_radius as f32 * key.scale as f32 / reduction as f32).min(128.);
                for step in [[1. / capture.w as f32, 0.], [0., 1. / capture.h as f32]] {
                    let mut target = self.take_scratch(renderer, capture_size)?;
                    let _ = Self::pass(
                        renderer,
                        &source,
                        &mut target,
                        &program,
                        Pass {
                            mode: 1.,
                            step,
                            radius,
                            ..Pass::default()
                        },
                    )?;
                    self.release_scratch(capture_size, source);
                    source = target;
                }
            } else {
                let mut sizes = Vec::new();
                for _ in 0..options.iterations {
                    let size = source.size();
                    let size = (size.w, size.h);
                    if size.0 <= 1 || size.1 <= 1 {
                        break;
                    }
                    sizes.push(size);
                    let mut target =
                        self.take_scratch(renderer, ((size.0 / 2).max(1), (size.1 / 2).max(1)))?;
                    let _ = Self::pass(
                        renderer,
                        &source,
                        &mut target,
                        &program,
                        Pass {
                            mode: 2.,
                            radius: options.offset,
                            ..Pass::default()
                        },
                    )?;
                    self.release_scratch(size, source);
                    source = target;
                }
                for size in sizes.into_iter().rev() {
                    let mut target = self.take_scratch(renderer, size)?;
                    let _ = Self::pass(
                        renderer,
                        &source,
                        &mut target,
                        &program,
                        Pass {
                            mode: 3.,
                            radius: options.offset,
                            ..Pass::default()
                        },
                    )?;
                    let previous = source.size();
                    self.release_scratch((previous.w, previous.h), source);
                    source = target;
                }
            }
        }
        let retained = retained_blur_size(key.region.size, effects);
        let final_size = (retained.w, retained.h);
        let mut final_image = match reuse {
            Some(texture)
                if texture.size().w == final_size.0 && texture.size().h == final_size.1 =>
            {
                texture
            }
            _ => renderer.create_buffer(Fourcc::Abgr8888, final_size.into())?,
        };

        let sync = Self::pass(
            renderer,
            &source,
            &mut final_image,
            &program,
            Pass {
                correction: [options.brightness, options.contrast, options.saturation],
                ..Pass::default()
            },
        )?;
        let used = source.size();
        self.release_scratch((used.w, used.h), source);
        Ok((
            TextureBuffer::from_texture(renderer, final_image.clone(), 1, Transform::Normal, None),
            final_image,
            sync,
        ))
    }
    fn pass(
        renderer: &mut GlesRenderer,
        source: &GlesTexture,
        target: &mut GlesTexture,
        program: &GlesTexProgram,
        pass: Pass,
    ) -> anyhow::Result<SyncPoint> {
        let size: Size<i32, Physical> = (target.size().w, target.size().h).into();
        let mut fb = renderer.bind(target)?;
        let mut frame = renderer.render(&mut fb, size, Transform::Normal)?;
        let damage = [Rectangle::from_size(size)];
        frame.clear([0., 0., 0., 1.].into(), &damage)?;
        frame.render_texture_from_to(
            source,
            Rectangle::from_size(source.size().to_f64()),
            Rectangle::from_size(size),
            &damage,
            &[],
            Transform::Normal,
            1.,
            Some(program),
            &[
                Uniform::new("step_size", pass.step),
                Uniform::new("grain", pass.grain),
                Uniform::new("grain_origin", pass.grain_origin),
                Uniform::new("correction", pass.correction),
                Uniform::new("pass_mode", pass.mode),
                Uniform::new("radius", pass.radius),
                Uniform::new(
                    "half_texel",
                    [0.5 / source.width() as f32, 0.5 / source.height() as f32],
                ),
            ],
        )?;

        Ok(frame.finish()?)
    }
}

struct Pass {
    mode: f32,
    step: [f32; 2],
    radius: f32,
    grain: [f32; 3],
    grain_origin: [f32; 2],
    correction: [f32; 3],
}
impl Default for Pass {
    fn default() -> Self {
        Self {
            mode: 0.,
            step: [0., 0.],
            radius: 0.,
            grain: [0., 1., 0.5],
            grain_origin: [0.; 2],
            correction: [1.; 3],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::backend::{
        egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay},
        renderer::{ExportMem, ImportMem},
    };

    #[test]
    fn blur_eviction_prefers_stale_then_preserved_then_other_outputs() {
        let current = Id::new();

        let stale = Id::new();
        let preserved = Id::new();
        let picked = eviction_candidate(&[
            (stale.clone(), 0, 9),
            (preserved.clone(), 1, 1),
            (current.clone(), u32::MAX, 0),
        ]);
        assert_eq!(picked, Some(stale));

        let older = Id::new();
        let newer = Id::new();
        let picked = eviction_candidate(&[
            (older.clone(), 1, 4),
            (newer.clone(), 1, 2),
            (current.clone(), u32::MAX, 0),
        ]);
        assert_eq!(picked, Some(newer));

        let foreign = Id::new();
        let picked = eviction_candidate(&[
            (preserved.clone(), 1, 9),
            (foreign.clone(), 2, 1),
            (current.clone(), u32::MAX, 0),
        ]);
        assert_eq!(picked, Some(preserved));
        let picked = eviction_candidate(&[(foreign.clone(), 2, 1)]);
        assert_eq!(picked, Some(foreign));

        let picked = eviction_candidate(&[(current.clone(), u32::MAX, 0)]);
        assert_eq!(picked, None);
        assert_eq!(eviction_candidate(&[]), None);
    }

    #[test]
    fn retained_backdrop_accepts_only_live_content_updates_with_unchanged_scene() {
        let key = CacheKey {
            below: vec![Stamp {
                id: Id::new(),
                commit: 1.into(),
                geometry: Rectangle::new((0, 0).into(), (32, 32).into()),
                source: Rectangle::new((0., 0.).into(), (32., 32.).into()),
                transform: Transform::Normal,
                alpha: 1.,
            }],
            size: (32, 32).into(),
            region: Rectangle::new((4, 4).into(), (24, 24).into()),
            scale: 1.,
            effects: rix_config::appearance::MaterialEffects::from(
                &rix_config::ThemeSettings::default(),
            ),
        };
        let mut updated = key.clone();
        updated.below[0].commit = 2.into();
        assert!(key.can_retain_for(&updated));
        assert!(
            key != updated,
            "A retained image must still request refresh"
        );
        for mutation in 0..10 {
            let mut changed = updated.clone();
            match mutation {
                0 => changed.below.clear(),
                1 => changed.below[0].id = Id::new(),
                2 => changed.below[0].geometry.loc.x += 1,
                3 => changed.below[0].source.loc.x += 1.,
                4 => changed.below[0].transform = Transform::_90,
                5 => changed.below[0].alpha = 0.5,
                6 => changed.region.loc.x += 1,
                7 => changed.size.w += 1,
                8 => changed.scale = 1.5,
                9 => changed.effects.blur_radius += 1,
                _ => unreachable!(),
            }
            assert!(
                !key.can_retain_for(&changed),
                "Scene/theme/transform mutation {mutation} reused stale pixels"
            );
        }
    }

    #[test]
    fn animated_backdrop_refreshes_under_a_stable_identity_and_rejects_reallocation() {

        let key = CacheKey {
            below: vec![Stamp {
                id: Id::new(),
                commit: 1.into(),
                geometry: Rectangle::new((0, 0).into(), (64, 64).into()),
                source: Rectangle::new((0., 0.).into(), (64., 64.).into()),
                transform: Transform::Normal,
                alpha: 1.,
            }],
            size: (64, 64).into(),
            region: Rectangle::new((8, 8).into(), (32, 32).into()),
            scale: 1.,
            effects: rix_config::appearance::MaterialEffects::from(
                &rix_config::ThemeSettings::default(),
            ),
        };
        let mut next_frame = key.clone();
        next_frame.below[0].commit = 2.into();
        assert!(key.can_retain_for(&next_frame));
        assert!(key != next_frame, "Changed pixels still refresh the blur");

        let mut reimported = next_frame.clone();
        reimported.below[0].id = Id::new();
        assert!(!key.can_retain_for(&reimported));
    }

    #[test]
    fn blur_scratch_reuse_is_bounded_per_size_and_in_total() {
        assert!(retained_scratch(0, 3, true, 5));
        assert!(!retained_scratch(BLUR_SCRATCH_PER_SIZE, 3, false, 5));
        assert!(
            !retained_scratch(0, BLUR_SCRATCH_SIZES, true, 5),
            "New sizes yield once the distinct-size bound is reached"
        );
        assert!(retained_scratch(0, BLUR_SCRATCH_SIZES, false, 5));
        assert!(
            !retained_scratch(0, 3, true, BLUR_SCRATCH_TOTAL),
            "The total texture bound is never exceeded"
        );
    }

    #[test]
    fn retained_blur_images_use_the_effect_working_resolution() {
        use rix_config::appearance::{BackdropSettings, BlurAlgorithm, MaterialEffects};
        let effects = |algorithm, blur_radius| MaterialEffects {
            blur_radius,
            noise_opacity: 0.,
            options: BackdropSettings {
                algorithm,
                ..Default::default()
            },
        };
        let region: Size<i32, Physical> = (1920, 1080).into();
        for algorithm in [BlurAlgorithm::Gaussian, BlurAlgorithm::DualKawase] {
            let retained = retained_blur_size(region, effects(algorithm, 16));
            assert_eq!(
                retained,
                (480, 270).into(),
                "{algorithm:?} must retain a quarter of the crop"
            );
            let (image, scratch) = retained_blur_bytes(region, effects(algorithm, 16));
            assert_eq!(image, 480 * 270 * 4);
            match algorithm {
                BlurAlgorithm::Gaussian => assert_eq!(scratch, image * 2),
                BlurAlgorithm::DualKawase => {
                    assert_eq!(scratch, 1920 * 1080 * 4 * 2, "Kawase builds at crop size")
                }
            }
        }

        assert_eq!(
            retained_blur_size(region, effects(BlurAlgorithm::Gaussian, 0)),
            region
        );
        let (image, _) = retained_blur_bytes(region, effects(BlurAlgorithm::Gaussian, 0));
        assert_eq!(image, 1920 * 1080 * 4);
    }

    #[test]
    #[ignore = "requires EGL; verifies real retained pixels under GPU budget pressure"]
    fn backdrop_pressure_keeps_completed_pixels_and_refreshes_after_release() {
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        let context = EGLContext::new(&display).unwrap();
        let mut renderer = unsafe { GlesRenderer::new(context) }.unwrap();
        let mut scene_renderer = SceneRenderer::default();
        const LIMIT: u64 = 64 * 1024;
        scene_renderer.blur_budget = backdrop_budget::Budget::with_limit(LIMIT);
        let size: Size<i32, Physical> = (64, 40).into();
        let markers = [Id::new(), Id::new()];
        let background = Id::new();
        let mut effects =
            rix_config::appearance::MaterialEffects::from(&rix_config::ThemeSettings::default());
        effects.options.algorithm = rix_config::appearance::BlurAlgorithm::Gaussian;
        effects.options.gaussian_downsample = 1;
        effects.options.brightness = 0.8;
        effects.options.contrast = 1.;
        effects.options.saturation = 1.;
        effects.blur_radius = 2;
        effects.noise_opacity = 0.;
        let create_scene = |commit: usize, color: [f32; 4], background: Id| {
            let mut scene = Vec::new();
            let mut requests = HashMap::new();
            for (marker, x) in markers.iter().zip([4, 36]) {
                let geometry = Rectangle::new((x, 10).into(), (24, 20).into());
                scene.push(SceneElement::Solid(SolidColorRenderElement::new(
                    marker.clone(),
                    geometry,
                    CommitCounter::default(),
                    [0.; 4],
                    Kind::Unspecified,
                )));
                requests.insert(
                    marker.clone(),
                    BackdropRequest {
                        region: None,
                        geometry: Some(geometry),
                        client_clip: None,
                        effects,
                        shapes: Some(vec![crate::surface_effects::Shape {
                            x: 0.,
                            y: 0.,
                            width: 24.,
                            height: 20.,
                            radius: 2.,
                            opacity: 1.,
                            clip: None,
                        }]),
                    },
                );
            }
            scene.push(SceneElement::Solid(SolidColorRenderElement::new(
                background,
                Rectangle::from_size(size),
                CommitCounter::from(commit),
                color,
                Kind::Unspecified,
            )));
            (scene, requests)
        };
        let read = |renderer: &mut GlesRenderer, elements: &[SceneElement]| {
            let mut texture: GlesTexture = renderer
                .create_buffer(Fourcc::Abgr8888, (size.w, size.h).into())
                .unwrap();
            let mut fb = renderer.bind(&mut texture).unwrap();
            OutputDamageTracker::new(size, 1., Transform::Normal)
                .render_output(renderer, &mut fb, 0, elements, [0., 0., 0., 1.])
                .unwrap();
            let mapping = renderer
                .copy_framebuffer(
                    &fb,
                    Rectangle::from_size((size.w, size.h).into()),
                    Fourcc::Abgr8888,
                )
                .unwrap();
            renderer.map_texture(&mapping).unwrap().to_vec()
        };
        let centers = [16, 48].map(|x| ((20 * size.w + x) * 4) as usize);
        let (input, requests) = create_scene(1, [1., 0., 0., 1.], background.clone());
        let first = scene_renderer
            .apply_backdrops(&mut renderer, input, requests, size, 1., "display")
            .unwrap();
        let initial = read(&mut renderer, &first);
        for center in centers {
            assert!(initial[center] > 150 && initial[center] < 230);
            assert_eq!(initial[center + 1], 0);
        }
        scene_renderer.release_finished_blur_scratch();
        let pressure = scene_renderer
            .blur_budget
            .reserve(LIMIT - scene_renderer.blur_budget.used())
            .unwrap();

        let (input, requests) = create_scene(2, [0., 1., 0., 1.], background.clone());
        let held = scene_renderer
            .apply_backdrops(&mut renderer, input, requests, size, 1., "display")
            .unwrap();
        let held_pixels = read(&mut renderer, &held);
        for (marker, center) in markers.iter().zip(centers) {
            assert_eq!(
                &held_pixels[center..center + 4],
                &initial[center..center + 4],
                "Budget pressure made one of the panels alternate between blurred and sharp pixels"
            );
            assert_eq!(
                scene_renderer.cache[&("display".to_string(), marker.clone())]
                    .key
                    .below
                    .iter()
                    .find(|stamp| stamp.id == background)
                    .unwrap()
                    .commit,
                CommitCounter::from(1),
                "Holding a completed image must not mark newer content as rendered"
            );
        }
        assert_eq!(scene_renderer.blur_budget.used(), LIMIT);
        drop(pressure);

        let (input, requests) = create_scene(2, [0., 1., 0., 1.], background);
        let refreshed = scene_renderer
            .apply_backdrops(&mut renderer, input, requests, size, 1., "display")
            .unwrap();
        let refreshed_pixels = read(&mut renderer, &refreshed);
        for center in centers {
            assert_eq!(refreshed_pixels[center], 0);
            assert!(refreshed_pixels[center + 1] > 150 && refreshed_pixels[center + 1] < 230);
        }
        assert!(scene_renderer.blur_budget.used() <= LIMIT);
        scene_renderer.release_finished_blur_scratch();

        let pressure = scene_renderer
            .blur_budget
            .reserve(LIMIT - scene_renderer.blur_budget.used())
            .unwrap();
        let (input, requests) = create_scene(3, [0., 0., 1., 1.], Id::new());
        let changed = scene_renderer
            .apply_backdrops(&mut renderer, input, requests, size, 1., "display")
            .unwrap();
        assert!(!changed
            .iter()
            .any(|element| matches!(element, SceneElement::BackdropMasked(_))));
        for marker in markers {
            assert!(!scene_renderer
                .cache
                .contains_key(&("display".to_string(), marker)));
        }
        assert!(scene_renderer.blur_budget.used() <= LIMIT);
        drop(pressure);
    }

    #[test]
    #[ignore = "requires EGL; verifies overlapping blur refreshes under a bounded budget"]
    fn overlapping_backdrops_keep_identity_pixels_and_damage_on_refresh() {
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        let context = EGLContext::new(&display).unwrap();
        let mut renderer = unsafe { GlesRenderer::new(context) }.unwrap();
        let mut scene_renderer = SceneRenderer::default();
        const LIMIT: u64 = 64 * 1024;
        scene_renderer.blur_budget = backdrop_budget::Budget::with_limit(LIMIT);
        let size: Size<i32, Physical> = (48, 40).into();
        let geometry = Rectangle::new((8, 8).into(), (32, 24).into());
        let markers = [Id::new(), Id::new()];
        let background = Id::new();
        let mut effects =
            rix_config::appearance::MaterialEffects::from(&rix_config::ThemeSettings::default());
        effects.options.algorithm = rix_config::appearance::BlurAlgorithm::Gaussian;
        effects.options.gaussian_downsample = 1;
        effects.options.brightness = 0.8;
        effects.options.contrast = 1.;
        effects.options.saturation = 1.;
        effects.blur_radius = 2;
        effects.noise_opacity = 0.;
        let create_scene = |commit: usize, color: [f32; 4], lower_radius: f64| {
            let mut scene = Vec::new();
            let mut requests = HashMap::new();
            for (part, marker) in markers.iter().enumerate() {
                scene.push(SceneElement::Solid(SolidColorRenderElement::new(
                    marker.clone(),
                    geometry,
                    CommitCounter::default(),
                    [0.; 4],
                    Kind::Unspecified,
                )));
                requests.insert(
                    marker.clone(),
                    BackdropRequest {
                        region: None,
                        geometry: Some(geometry),
                        client_clip: None,
                        effects,
                        shapes: Some(vec![crate::surface_effects::Shape {
                            x: 0.,
                            y: 0.,
                            width: 32.,
                            height: 24.,
                            radius: if part == 1 { lower_radius } else { 2. },
                            opacity: 1.,
                            clip: None,
                        }]),
                    },
                );
            }
            scene.push(SceneElement::Solid(SolidColorRenderElement::new(
                background.clone(),
                Rectangle::from_size(size),
                CommitCounter::from(commit),
                color,
                Kind::Unspecified,
            )));
            (scene, requests)
        };
        let mut output: GlesTexture = renderer
            .create_buffer(Fourcc::Abgr8888, (size.w, size.h).into())
            .unwrap();
        let mut damage = OutputDamageTracker::new(size, 1., Transform::Normal);
        let mut read = |renderer: &mut GlesRenderer, elements: &[SceneElement], age: usize| {
            let mut fb = renderer.bind(&mut output).unwrap();
            damage
                .render_output(renderer, &mut fb, age, elements, [0., 0., 0., 1.])
                .unwrap();
            let mapping = renderer
                .copy_framebuffer(
                    &fb,
                    Rectangle::from_size((size.w, size.h).into()),
                    Fourcc::Abgr8888,
                )
                .unwrap();
            renderer.map_texture(&mapping).unwrap().to_vec()
        };
        let key = |marker: &Id| ("display".to_string(), marker.clone());
        let center = ((20 * size.w + 24) * 4) as usize;
        let (input, requests) = create_scene(1, [1., 0., 0., 1.], 2.);
        let initial = scene_renderer
            .apply_backdrops(&mut renderer, input, requests, size, 1., "display")
            .unwrap();
        let initial_pixels = read(&mut renderer, &initial, 0);
        assert!(initial_pixels[center] > 140 && initial_pixels[center] < 180);
        let ids: Vec<_> = markers
            .iter()
            .map(|marker| scene_renderer.cache[&key(marker)].element_ids[0].clone())
            .collect();
        let initial_commits: Vec<_> = markers
            .iter()
            .map(|marker| scene_renderer.cache[&key(marker)].commit)
            .collect();
        scene_renderer.release_finished_blur_scratch();
        let crop = scene_renderer.cache[&key(&markers[1])].key.region.size;
        let lower_bytes = crop.w as u64 * crop.h as u64 * 4;

        let pressure = scene_renderer
            .blur_budget
            .reserve(LIMIT - scene_renderer.blur_budget.used() - lower_bytes * 3)
            .unwrap();
        for frame in 0..4 {
            let (input, requests) = create_scene(2, [0., 1., 0., 1.], 2.);
            let held = scene_renderer
                .apply_backdrops(&mut renderer, input, requests, size, 1., "display")
                .unwrap();
            assert_eq!(
                held.iter()
                    .filter(|element| matches!(element, SceneElement::BackdropMasked(_)))
                    .count(),
                2,
                "Frame {frame} lost an overlapping backdrop when the lower texture changed"
            );
            let pixels = read(&mut renderer, &held, 1);
            assert_eq!(
                &pixels[center..center + 4],
                &initial_pixels[center..center + 4],
                "An upper window alternated between completed blur and sharp live pixels"
            );
            for (part, marker) in markers.iter().enumerate() {
                assert_eq!(scene_renderer.cache[&key(marker)].element_ids[0], ids[part]);
            }
            assert!(scene_renderer.cache[&key(&markers[1])].commit > initial_commits[1]);
            assert_eq!(
                scene_renderer.cache[&key(&markers[0])].commit,
                initial_commits[0]
            );
            assert!(scene_renderer.blur_budget.used() <= LIMIT);
        }
        drop(pressure);
        let (input, requests) = create_scene(2, [0., 1., 0., 1.], 2.);
        let refreshed = scene_renderer
            .apply_backdrops(&mut renderer, input, requests, size, 1., "display")
            .unwrap();
        let refreshed_pixels = read(&mut renderer, &refreshed, 1);
        assert_eq!(refreshed_pixels[center], 0);
        assert!(refreshed_pixels[center + 1] > 140 && refreshed_pixels[center + 1] < 180);
        for (part, marker) in markers.iter().enumerate() {
            let cached = &scene_renderer.cache[&key(marker)];
            assert_eq!(cached.element_ids[0], ids[part]);
            assert!(cached.commit > initial_commits[part]);
            let element = refreshed
                .iter()
                .find(|element| element.id() == &ids[part])
                .unwrap();
            assert!(!element
                .damage_since(1.0.into(), Some(initial_commits[part]))
                .is_empty());
            assert!(element
                .damage_since(1.0.into(), Some(cached.commit))
                .is_empty());
        }

        let (input, requests) = create_scene(2, [0., 1., 0., 1.], 6.);
        let changed = scene_renderer
            .apply_backdrops(&mut renderer, input, requests, size, 1., "display")
            .unwrap();
        read(&mut renderer, &changed, 1);
        for (part, marker) in markers.iter().enumerate() {
            assert_ne!(scene_renderer.cache[&key(marker)].element_ids[0], ids[part]);
        }
        assert!(scene_renderer.blur_budget.used() <= LIMIT);
    }

    #[test]
    #[ignore = "requires EGL; verifies hidden blur work is culled before allocation"]
    fn opaque_foreground_skips_hidden_blur_but_translucent_and_partial_cover_do_not() {
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        let context = EGLContext::new(&display).unwrap();
        let mut renderer = unsafe { GlesRenderer::new(context) }.unwrap();
        let mut scene_renderer = SceneRenderer::default();
        let size: Size<i32, Physical> = (48, 40).into();
        let geometry = Rectangle::new((12, 10).into(), (24, 20).into());
        let mut effects =
            rix_config::appearance::MaterialEffects::from(&rix_config::ThemeSettings::default());
        effects.blur_radius = 2;
        effects.noise_opacity = 0.;
        for (foreground, color, expected) in [
            (Rectangle::from_size(size), [0., 1., 0., 1.], 0),
            (Rectangle::from_size(size), [0., 0.5, 0., 0.5], 1),
            (
                Rectangle::new((0, 0).into(), (24, 40).into()),
                [0., 1., 0., 1.],
                1,
            ),
        ] {
            let marker = Id::new();
            let input = vec![
                SceneElement::Solid(SolidColorRenderElement::new(
                    Id::new(),
                    foreground,
                    CommitCounter::default(),
                    color,
                    Kind::Unspecified,
                )),
                SceneElement::Solid(SolidColorRenderElement::new(
                    marker.clone(),
                    geometry,
                    CommitCounter::default(),
                    [0.; 4],
                    Kind::Unspecified,
                )),
                SceneElement::Solid(SolidColorRenderElement::new(
                    Id::new(),
                    Rectangle::from_size(size),
                    CommitCounter::default(),
                    [1., 0., 0., 1.],
                    Kind::Unspecified,
                )),
            ];
            let requests = HashMap::from([(
                marker,
                BackdropRequest {
                    region: None,
                    geometry: Some(geometry),
                    client_clip: None,
                    effects,
                    shapes: None,
                },
            )]);
            let scene = scene_renderer
                .apply_backdrops(&mut renderer, input, requests, size, 1., "display")
                .unwrap();
            assert_eq!(
                scene
                    .iter()
                    .filter(|element| matches!(element, SceneElement::BackdropTexture(_)))
                    .count(),
                expected
            );
            if expected == 0 {
                assert!(scene_renderer.cache.is_empty());
                assert_eq!(scene_renderer.blur_budget.used(), 0);
            }
        }
    }

    #[test]
    #[ignore = "requires EGL; verifies a fullscreen output preserves another output's live blur"]
    fn fullscreen_scene_does_not_discard_another_outputs_backdrops() {
        let mut desktop = crate::lifecycle::tests::TestDesktop::new();
        let game = desktop.window("Fullscreen blur isolation fixture");
        game.2.set_fullscreen(None);
        desktop.roundtrip();
        let buffer = desktop.sized_buffer(1000, 700);
        game.0.attach(Some(&buffer), 0, 0);
        game.0.commit();
        desktop.roundtrip();
        desktop.inspect(|state| {
            let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
            let context = EGLContext::new(&display).unwrap();
            let mut renderer = unsafe { GlesRenderer::new(context) }.unwrap();
            let mut scene_renderer = SceneRenderer::default();
            let first = state.space.outputs().next().unwrap().clone();
            let second = Output::new(
                "second".into(),
                smithay::output::PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: smithay::output::Subpixel::Unknown,
                    make: "test".into(),
                    model: "test".into(),
                },
            );
            second.change_current_state(
                Some(smithay::output::Mode {
                    size: (1000, 700).into(),
                    refresh: 60_000,
                }),
                None,
                None,
                Some((1000, 0).into()),
            );
            state.space.map_output(&second, (1000, 0));
            let marker = Id::new();
            let geometry = Rectangle::new((8, 8).into(), (24, 20).into());
            let mut effects = rix_config::appearance::MaterialEffects::from(
                &rix_config::ThemeSettings::default(),
            );
            effects.blur_radius = 2;
            effects.noise_opacity = 0.;
            for output in [&first, &second] {
                let input = vec![
                    SceneElement::Solid(SolidColorRenderElement::new(
                        marker.clone(),
                        geometry,
                        CommitCounter::default(),
                        [0.; 4],
                        Kind::Unspecified,
                    )),
                    SceneElement::Solid(SolidColorRenderElement::new(
                        Id::new(),
                        Rectangle::from_size((48, 40).into()),
                        CommitCounter::default(),
                        [0., 0., 1., 1.],
                        Kind::Unspecified,
                    )),
                ];
                let requests = HashMap::from([(
                    marker.clone(),
                    BackdropRequest {
                        region: None,
                        geometry: Some(geometry),
                        client_clip: None,
                        effects,
                        shapes: None,
                    },
                )]);
                scene_renderer
                    .apply_backdrops(
                        &mut renderer,
                        input,
                        requests,
                        (48, 40).into(),
                        1.,
                        &output.name(),
                    )
                    .unwrap();
            }
            assert_eq!(scene_renderer.cache.len(), 2);
            assert!(scene_renderer
                .compose_fullscreen(&mut renderer, state, &first, None)
                .unwrap()
                .is_some());
            assert!(!scene_renderer
                .cache
                .contains_key(&(first.name(), marker.clone())));
            assert!(
                scene_renderer
                    .cache
                    .contains_key(&(second.name(), marker.clone())),
                "A fullscreen game discarded blur on the other monitor"
            );
            assert_eq!(scene_renderer.cache.len(), 1);
            assert!(scene_renderer
                .compose_fullscreen(&mut renderer, state, &first, None)
                .unwrap()
                .is_some());
            assert!(scene_renderer.cache.contains_key(&(second.name(), marker)));
        });
    }

    #[test]
    #[ignore = "requires EGL; verifies real cache eviction and inflight frame accounting"]
    fn backdrop_budget_survives_cache_eviction_and_is_shared_with_capture() {
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        let context = EGLContext::new(&display).unwrap();
        let mut renderer = unsafe { GlesRenderer::new(context) }.unwrap();
        let mut scene_renderer = SceneRenderer {
            blur_budget: backdrop_budget::Budget::with_limit(8192),
            ..Default::default()
        };
        let mut effects =
            rix_config::appearance::MaterialEffects::from(&rix_config::ThemeSettings::default());
        effects.blur_radius = 0;
        effects.noise_opacity = 0.;
        effects.options.brightness = 0.8;
        let create_scene = || {
            let mut scene = Vec::new();
            let mut requests = HashMap::new();
            for y in 0..4 {
                for x in 0..8 {
                    let geometry = Rectangle::new((x * 16 + 1, y * 16 + 1).into(), (14, 14).into());
                    let marker = SolidColorRenderElement::new(
                        Id::new(),
                        geometry,
                        CommitCounter::default(),
                        [0.; 4],
                        Kind::Unspecified,
                    );
                    requests.insert(
                        marker.id().clone(),
                        BackdropRequest {
                            region: None,
                            geometry: Some(geometry),
                            client_clip: None,
                            effects,
                            shapes: Some(vec![crate::surface_effects::Shape {
                                x: 0.,
                                y: 0.,
                                width: 14.,
                                height: 14.,
                                radius: 2.,
                                opacity: 1.,
                                clip: None,
                            }]),
                        },
                    );
                    scene.push(SceneElement::Solid(marker));
                }
            }
            (scene, requests)
        };
        let (input, requests) = create_scene();
        let scene = scene_renderer
            .apply_backdrops(
                &mut renderer,
                input,
                requests,
                (128, 64).into(),
                1.,
                "display",
            )
            .unwrap();
        let count = scene
            .iter()
            .filter(|element| matches!(element, SceneElement::BackdropMasked(_)))
            .count();
        assert!(
            count > 0 && count < 32,
            "Effects yield to the explicit budget instead of allocating indefinitely"
        );
        assert!(scene_renderer.blur_budget.used() <= 8192);
        let mut target: GlesTexture = renderer
            .create_buffer(Fourcc::Abgr8888, (128, 64).into())
            .unwrap();
        {
            let mut fb = renderer.bind(&mut target).unwrap();
            OutputDamageTracker::new((128, 64), 1., Transform::Normal)
                .render_output(&mut renderer, &mut fb, 0, &scene, [0., 0., 0., 1.])
                .unwrap();
            let mapping = renderer
                .copy_framebuffer(
                    &fb,
                    Rectangle::from_size((128, 64).into()),
                    Fourcc::Abgr8888,
                )
                .unwrap();
            renderer.map_texture(&mapping).unwrap();
        }
        scene_renderer.release_finished_blur_scratch();
        let retained = scene_renderer.blur_budget.used();
        scene_renderer.cache.clear();
        assert_eq!(
            scene_renderer.blur_budget.used(),
            retained,
            "The returned frame still owns the texture reservations"
        );
        let mut capture = SceneRenderer {
            blur_budget: scene_renderer.blur_budget.clone(),
            ..SceneRenderer::default()
        };
        let (input, requests) = create_scene();
        let captured = capture
            .apply_backdrops(
                &mut renderer,
                input,
                requests,
                (128, 64).into(),
                1.,
                "capture",
            )
            .unwrap();
        assert!(scene_renderer.blur_budget.used() <= 8192);
        drop(scene);
        drop(captured);
        capture.cache.clear();
        capture.release_finished_blur_scratch();

        assert!(scene_renderer.blur_budget.used() <= 8192);
        drop(capture);
        assert_eq!(scene_renderer.blur_budget.used(), 0);
    }

    #[test]
    fn backdrop_crop_tracks_visible_shape_and_filter_halo_at_fractional_scale() {
        let output: Size<i32, Physical> = (3840, 2160).into();
        let mut effects =
            rix_config::appearance::MaterialEffects::from(&rix_config::ThemeSettings::default());
        effects.options.algorithm = rix_config::appearance::BlurAlgorithm::Gaussian;
        effects.options.gaussian_downsample = 2;
        effects.blur_radius = 12;
        let request = BackdropRequest {
            region: None,
            geometry: None,
            client_clip: None,
            effects,
            shapes: Some(vec![crate::surface_effects::Shape {
                x: 4.,
                y: 6.,
                width: 100.,
                height: 80.,
                radius: 8.,
                opacity: 1.,
                clip: Some(crate::surface_effects::Clip {
                    x: 4.,
                    y: 6.,
                    width: 72.,
                    height: 60.,
                }),
            }]),
        };
        let geometry = Rectangle::new((1000, 600).into(), (180, 160).into());
        let rectangles = request_rectangles(&request, geometry, 1.5);
        assert_eq!(
            rectangles,
            [Rectangle::new((1006, 609).into(), (108, 90).into())]
        );
        let region = backdrop_region(&rectangles, output, 1.5, effects).unwrap();
        assert!(region.loc.x <= 1006 - 22 && region.loc.y <= 609 - 22);
        assert!(region.loc.x + region.size.w >= 1114 + 22);
        assert!(region.loc.y + region.size.h >= 699 + 22);
        assert_eq!(region.loc.x % 2, 0);
        assert_eq!(region.size.w % 2, 0);
        assert!(region.size.w * region.size.h < output.w * output.h / 100);
        assert!(backdrop_region(
            &[Rectangle::new((5000, 10).into(), (100, 100).into())],
            output,
            1.5,
            effects
        )
        .is_none());
        let edge = backdrop_region(
            &[Rectangle::new((-10, -10).into(), (40, 50).into())],
            output,
            1.5,
            effects,
        )
        .unwrap();
        assert_eq!(edge.loc, (0, 0).into());
    }

    #[test]
    #[ignore = "requires EGL; compares cropped blur and masks against full-output pixels"]
    fn cropped_backdrops_match_full_output_filters_at_output_edges_and_scaled_masks() {
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        let context = EGLContext::new(&display).unwrap();
        let mut renderer = unsafe { GlesRenderer::new(context) }.unwrap();
        let size: Size<i32, Physical> = (384, 256).into();
        let mut pixels = vec![0; (size.w * size.h * 4) as usize];
        for y in 0..size.h {
            for x in 0..size.w {
                let index = ((y * size.w + x) * 4) as usize;
                pixels[index..index + 4].copy_from_slice(&[
                    if (x / 7 + y / 9) % 2 == 0 { 220 } else { 20 },
                    (y * 3 % 256) as u8,
                    (x * 5 % 256) as u8,
                    255,
                ]);
            }
        }
        let source = TextureBuffer::from_memory(
            &mut renderer,
            &pixels,
            Fourcc::Abgr8888,
            (size.w, size.h),
            false,
            1,
            Transform::Normal,
            None,
        )
        .unwrap();
        let read = |renderer: &mut GlesRenderer, elements: &[SceneElement], scale: f64| {
            let mut texture: GlesTexture = renderer
                .create_buffer(Fourcc::Abgr8888, (size.w, size.h).into())
                .unwrap();
            let mut fb = renderer.bind(&mut texture).unwrap();
            OutputDamageTracker::new(size, scale, Transform::Normal)
                .render_output(renderer, &mut fb, 0, elements, [0., 0., 0., 1.])
                .unwrap();
            let mapping = renderer
                .copy_framebuffer(
                    &fb,
                    Rectangle::from_size((size.w, size.h).into()),
                    Fourcc::Abgr8888,
                )
                .unwrap();
            renderer.map_texture(&mapping).unwrap().to_vec()
        };
        let program = coverage::program(&mut renderer).unwrap();
        let mut scene_renderer = SceneRenderer::default();
        for scale in [1., 1.5] {
            let background = SceneElement::Texture(TextureRenderElement::from_texture_buffer(
                (0., 0.),
                &source,
                None,
                Some(texture_pixel_source(&source)),
                Some(size.to_f64().to_logical(scale).to_i32_round()),
                Kind::Unspecified,
            ));
            for algorithm in [
                rix_config::appearance::BlurAlgorithm::Gaussian,
                rix_config::appearance::BlurAlgorithm::DualKawase,
            ] {
                let mut effects = rix_config::appearance::MaterialEffects::from(
                    &rix_config::ThemeSettings::default(),
                );
                effects.options.algorithm = algorithm;
                effects.options.gaussian_downsample = 2;
                effects.options.iterations = 3;
                effects.options.offset = 1.5;
                effects.options.brightness = 1.;
                effects.options.contrast = 1.;
                effects.options.saturation = 1.;
                effects.blur_radius = 9;
                effects.noise_opacity = 0.;
                let (full, _, _) = scene_renderer
                    .blur(
                        &mut renderer,
                        std::slice::from_ref(&background),
                        &CacheKey {
                            below: vec![],
                            size,
                            region: Rectangle::from_size(size),
                            scale,
                            effects,
                        },
                        None,
                    )
                    .unwrap();
                for geometry in [
                    Rectangle::new((112, 72).into(), (60, 64).into()),
                    Rectangle::new((-6, 22).into(), (54, 54).into()),
                ] {
                    let region = backdrop_region(&[geometry], size, scale, effects).unwrap();
                    assert!(region.size.w < size.w && region.size.h < size.h);
                    let (crop, _, _) = scene_renderer
                        .blur(
                            &mut renderer,
                            std::slice::from_ref(&background),
                            &CacheKey {
                                below: vec![],
                                size,
                                region,
                                scale,
                                effects,
                            },
                            None,
                        )
                        .unwrap();
                    let shape = crate::surface_effects::Shape {
                        x: 0.,
                        y: 0.,
                        width: geometry.size.w as f64 / scale,
                        height: geometry.size.h as f64 / scale,
                        radius: 6.,
                        opacity: 0.75,
                        clip: Some(crate::surface_effects::Clip {
                            x: 0.,
                            y: 2.,
                            width: geometry.size.w as f64 / scale,
                            height: geometry.size.h as f64 / scale - 4.,
                        }),
                    };
                    let full_scene = [SceneElement::Masked(coverage::element(
                        &full,
                        &program,
                        &[shape],
                        geometry,
                        size,
                        scale,
                    ))];
                    let crop_scene = [SceneElement::Masked(coverage::element_for_region(
                        &crop,
                        &program,
                        &[shape],
                        geometry,
                        size,
                        region,
                        scale,
                        None,
                        [0., 1., 0.5],
                    ))];
                    let expected = read(&mut renderer, &full_scene, scale);
                    let actual = read(&mut renderer, &crop_scene, scale);
                    for (index, (actual, expected)) in actual.iter().zip(&expected).enumerate() {
                        assert!((*actual as i32 - *expected as i32).abs() <= 3,
                            "ROI changed a mask/filter pixel at byte {index}, scale {scale}, algorithm {algorithm:?}, region {region:?}: {actual} vs {expected}");
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "requires EGL; verifies server decoration backdrop pixels"]
    fn decoration_backdrop_blurs_only_background_and_never_client_or_controls() {
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        let context = EGLContext::new(&display).unwrap();
        let mut renderer = unsafe { GlesRenderer::new(context) }.unwrap();
        let size: Size<i32, Physical> = (64, 64).into();
        let mut pixels = vec![0; 64 * 64 * 4];
        for y in 0..64 {
            for x in 0..64 {
                let red = (x / 4 + y / 4) % 2 == 0;
                pixels[(y * 64 + x) * 4..(y * 64 + x) * 4 + 4].copy_from_slice(if red {
                    &[255, 0, 0, 255]
                } else {
                    &[0, 0, 255, 255]
                });
            }
        }
        let texture = TextureBuffer::from_memory(
            &mut renderer,
            &pixels,
            Fourcc::Abgr8888,
            (64, 64),
            false,
            1,
            Transform::Normal,
            None,
        )
        .unwrap();
        let background = SceneElement::Texture(TextureRenderElement::from_texture_buffer(
            (0., 0.),
            &texture,
            None,
            None,
            None,
            Kind::Unspecified,
        ));
        let solid = |loc: (i32, i32), extent: (i32, i32), color: [f32; 4]| {
            SceneElement::Solid(SolidColorRenderElement::new(
                Id::new(),
                Rectangle::new(loc.into(), extent.into()),
                CommitCounter::default(),
                color,
                Kind::Unspecified,
            ))
        };
        let mut scene_renderer = SceneRenderer::default();
        let mut effects =
            rix_config::appearance::MaterialEffects::from(&rix_config::ThemeSettings::default());
        effects.blur_radius = 7;
        effects.noise_opacity = 0.;
        effects.options.algorithm = rix_config::appearance::BlurAlgorithm::Gaussian;
        effects.options.gaussian_downsample = 1;
        effects.options.brightness = 1.;
        effects.options.contrast = 1.;
        effects.options.saturation = 1.;
        let backdrop = crate::decoration::DecorationBackdrop {
            bounds: Rectangle::new((8, 8).into(), (48, 48).into()),
            body: Rectangle::new((12, 24).into(), (40, 28).into()),
            radius: 4.,
            client_radius: 0.,
            alpha: 1.,
            effects,
        };
        let mut requests = HashMap::new();
        let marker =
            scene_renderer.decoration_anchor(7, &backdrop, (0, 0).into(), 1., &mut requests);
        let scene = scene_renderer
            .apply_backdrops(
                &mut renderer,
                vec![
                    solid((20, 12), (2, 2), [1.; 4]),
                    solid((12, 24), (40, 28), [0., 0.5, 0., 0.5]),
                    solid((8, 8), (48, 16), [0., 0., 0., 0.3]),
                    marker,
                    background,
                ],
                requests,
                size,
                1.,
                "material-test",
            )
            .unwrap();
        let mut target: GlesTexture = renderer
            .create_buffer(Fourcc::Abgr8888, (64, 64).into())
            .unwrap();
        let memory = {
            let mut fb = renderer.bind(&mut target).unwrap();
            OutputDamageTracker::new(size, 1., Transform::Normal)
                .render_output(&mut renderer, &mut fb, 0, &scene, [0., 0., 0., 1.])
                .unwrap();
            renderer
                .copy_framebuffer(&fb, Rectangle::from_size((64, 64).into()), Fourcc::Abgr8888)
                .unwrap()
        };
        let bytes = renderer.map_texture(&memory).unwrap();
        let sample = |x: usize, y: usize| &bytes[(y * 64 + x) * 4..(y * 64 + x) * 4 + 4];
        let title = sample(25, 18);
        assert!(
            title[0] > 20 && title[2] > 20,
            "checkerboard should be blurred: {title:?}"
        );
        assert_eq!(
            title[1], 0,
            "client and white controls must not contaminate the backdrop"
        );
        assert_eq!(sample(20, 12), [255; 4], "foreground controls stay sharp");
        let client = sample(18, 30);
        assert!((126..=129).contains(&client[1]));
        assert!(
            client[0].min(client[2]) <= 1,
            "client interior must retain unblurred checkerboard: {client:?}"
        );
        assert_eq!(
            sample(2, 2),
            [255, 0, 0, 255],
            "backdrop must not affect the desktop outside its bounds"
        );
    }

    #[test]
    #[ignore = "requires EGL; run with --ignored"]
    fn blur_preserves_constant_color_and_orientation() {
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        let context = EGLContext::new(&display).unwrap();
        let mut renderer = unsafe { GlesRenderer::new(context) }.unwrap();
        let size = (1280, 800);
        let mut pixels = vec![0u8; (size.0 * size.1 * 4) as usize];
        for y in 0..size.1 {
            for x in 0..size.0 {
                let index = ((y * size.0 + x) * 4) as usize;
                pixels[index..index + 4].copy_from_slice(&[
                    100 + (x / 16) as u8,
                    60 + (y / 8) as u8,
                    180,
                    255,
                ]);
            }
        }
        let source = renderer
            .import_memory(&pixels, Fourcc::Abgr8888, size.into(), false)
            .unwrap();
        let source = TextureBuffer::from_texture(&renderer, source, 1, Transform::Normal, None);
        let scene = [SceneElement::Texture(
            TextureRenderElement::from_texture_buffer(
                (0., 0.),
                &source,
                None,
                None,
                None,
                Kind::Unspecified,
            ),
        )];
        let mut scene_renderer = SceneRenderer::default();
        for algorithm in [
            rix_config::appearance::BlurAlgorithm::Gaussian,
            rix_config::appearance::BlurAlgorithm::DualKawase,
        ] {
            let effects = rix_config::appearance::MaterialEffects {
                blur_radius: 28,
                noise_opacity: 0.02,
                options: rix_config::appearance::BackdropSettings {
                    algorithm,
                    ..Default::default()
                },
            };
            let (result, _, _) = scene_renderer
                .blur(
                    &mut renderer,
                    &scene,
                    &CacheKey {
                        below: vec![],
                        size: size.into(),
                        region: Rectangle::from_size(size.into()),
                        scale: 1.,
                        effects,
                    },
                    None,
                )
                .unwrap();
            let mut texture: GlesTexture = renderer
                .create_buffer(Fourcc::Abgr8888, size.into())
                .unwrap();
            let mut fb = renderer.bind(&mut texture).unwrap();

            let retained: Size<i32, Physical> = retained_blur_size(size.into(), effects);
            let row = retained.h as f64 / size.1 as f64;
            let elements: Vec<_> = (0..size.1)
                .map(|y| {
                    TextureRenderElement::from_texture_buffer(
                        (0., y as f64),
                        &result,
                        None,
                        Some(Rectangle::new(
                            (0., (y as f64 * row).floor()).into(),
                            (retained.w as f64, 1.).into(),
                        )),
                        Some((size.0, 1).into()),
                        Kind::Unspecified,
                    )
                })
                .collect();
            let mut damage = OutputDamageTracker::new(size, 1., Transform::Normal);
            damage
                .render_output(&mut renderer, &mut fb, 0, &elements, [0., 0., 0., 1.])
                .unwrap();
            let mapping = renderer
                .copy_framebuffer(&fb, Rectangle::from_size(size.into()), Fourcc::Abgr8888)
                .unwrap();
            let actual = renderer.map_texture(&mapping).unwrap();
            for y in 128..size.1 - 128 {
                for x in 128..size.0 - 128 {
                    let index = ((y * size.0 + x) * 4) as usize;
                    for channel in 0..4 {
                        assert!(
                            (actual[index + channel] as i32 - pixels[index + channel] as i32).abs()
                                <= 6,
                            "pixel ({x},{y}) channel {channel}: got {}, expected {}",
                            actual[index + channel],
                            pixels[index + channel]
                        );
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "requires EGL; run with --ignored"]
    fn downsampled_backdrops_keep_crisp_draw_time_grain() {
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        let context = EGLContext::new(&display).unwrap();
        let mut renderer = unsafe { GlesRenderer::new(context) }.unwrap();
        let size: Size<i32, Physical> = (64, 48).into();
        let geometry = Rectangle::new((16, 12).into(), (32, 24).into());
        let mut effects =
            rix_config::appearance::MaterialEffects::from(&rix_config::ThemeSettings::default());
        effects.blur_radius = 8;
        effects.noise_opacity = 0.5;
        let marker = Id::new();
        let input = vec![
            SceneElement::Solid(SolidColorRenderElement::new(
                marker.clone(),
                geometry,
                CommitCounter::default(),
                [0.; 4],
                Kind::Unspecified,
            )),
            SceneElement::Solid(SolidColorRenderElement::new(
                Id::new(),
                Rectangle::from_size(size),
                CommitCounter::default(),
                [0.5, 0.5, 0.5, 1.],
                Kind::Unspecified,
            )),
        ];
        let requests = HashMap::from([(
            marker.clone(),
            BackdropRequest {
                region: None,
                geometry: Some(geometry),
                client_clip: None,
                effects,
                shapes: Some(vec![crate::surface_effects::Shape {
                    x: 0.,
                    y: 0.,
                    width: 32.,
                    height: 24.,
                    radius: 0.,
                    opacity: 1.,
                    clip: None,
                }]),
            },
        )]);
        let mut scene_renderer = SceneRenderer::default();
        let scene = scene_renderer
            .apply_backdrops(&mut renderer, input, requests, size, 1., "display")
            .unwrap();

        let cached = &scene_renderer.cache[&("display".to_string(), marker)];
        let retained = retained_blur_size(cached.key.region.size, effects);
        let image_size = cached.image_texture.size();
        assert_eq!((image_size.w, image_size.h), (retained.w, retained.h));

        let baked = {
            let mut texture = cached.image_texture.clone();
            let fb = renderer.bind(&mut texture).unwrap();
            let mapping = renderer
                .copy_framebuffer(&fb, Rectangle::from_size(image_size), Fourcc::Abgr8888)
                .unwrap();
            let baked = renderer.map_texture(&mapping).unwrap().to_vec();
            drop(mapping);
            drop(fb);
            baked
        };
        for (index, pixel) in baked.chunks_exact(4).enumerate() {
            for (channel, value) in pixel.iter().enumerate() {
                assert!(
                    (*value as i32 - baked[channel] as i32).abs() <= 1,
                    "retained blur baked grain: pixel {index} channel {channel} is {value} vs {}",
                    baked[channel]
                );
            }
        }
        let mut output: GlesTexture = renderer
            .create_buffer(Fourcc::Abgr8888, (64, 48).into())
            .unwrap();
        let mut damage = OutputDamageTracker::new(size, 1., Transform::Normal);
        let mut fb = renderer.bind(&mut output).unwrap();
        damage
            .render_output(&mut renderer, &mut fb, 0, &scene, [0., 0., 0., 1.])
            .unwrap();
        let mapping = renderer
            .copy_framebuffer(&fb, Rectangle::from_size((64, 48).into()), Fourcc::Abgr8888)
            .unwrap();
        let drawn = renderer.map_texture(&mapping).unwrap().to_vec();
        let value = |x: i32, y: i32| drawn[((y * size.w + x) * 4) as usize] as i64;
        let (mut adjacent, mut quarter, mut count) = (0i64, 0i64, 0i64);
        for y in 16..30 {
            for x in 18..42 {
                adjacent += (value(x, y) - value(x + 1, y)).abs();
                quarter += (value(x, y) - value(x + 4, y)).abs();
                count += 1;
            }
        }
        let adjacent = adjacent as f64 / count as f64;
        let quarter = quarter as f64 / count as f64;
        assert!(
            quarter > 20.,
            "grain must be visible inside the mask: {quarter} levels between pixels four apart"
        );
        assert!(
            adjacent > quarter * 0.6,
            "grain must stay at output resolution: adjacent {adjacent}, four-apart {quarter}"
        );
    }
}
