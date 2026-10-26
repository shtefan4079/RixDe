use crate::state::RixState;
use smithay::{
    backend::renderer::{
        element::{
            surface::{WaylandSurfaceRenderElement, WaylandSurfaceTexture},
            texture::TextureRenderElement,
            utils::RescaleRenderElement,
            AsRenderElements, Element, Id, Kind, RenderElement,
        },
        gles::{GlesError, GlesFrame, GlesRenderer, GlesTexture},
        utils::CommitCounter,
        Renderer, Texture,
    },
    output::Output,
    utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Transform},
};
use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};
struct Surface {
    id: Id,
    texture: GlesTexture,
    source: Rectangle<f64, Buffer>,
    transform: Transform,
    geometry: Rectangle<i32, Physical>,
    alpha: f32,
    commit: CommitCounter,
    clip: Option<super::rounded::ClientClip>,
}
struct Window {
    bytes: u64,
    decoration: Vec<crate::decoration::DecorationPart>,
    backdrop: Option<crate::decoration::DecorationBackdrop>,
    surfaces: Vec<Surface>,
    center: Point<i32, Physical>,
    born: Instant,
    opening: bool,
    closed: Option<Instant>,
}
#[derive(Default)]
pub struct Motion {
    outputs: HashMap<String, HashMap<u64, Window>>,
}

#[derive(Debug)]
pub struct PixelTexture {
    inner: TextureRenderElement<GlesTexture>,
    geometry: Rectangle<i32, Physical>,
    source: Rectangle<f64, Buffer>,
    commit: CommitCounter,
}
impl PixelTexture {
    pub(super) fn new(
        inner: TextureRenderElement<GlesTexture>,
        geometry: Rectangle<i32, Physical>,
        source: Rectangle<f64, Buffer>,
        commit: CommitCounter,
    ) -> Self {
        Self {
            inner,
            geometry,
            source,
            commit,
        }
    }
}
impl Element for PixelTexture {
    fn id(&self) -> &Id {
        self.inner.id()
    }
    fn current_commit(&self) -> CommitCounter {
        self.commit
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        self.source
    }
    fn transform(&self) -> Transform {
        self.inner.transform()
    }
    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry
    }
    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }
    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}
impl RenderElement<GlesRenderer> for PixelTexture {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        <TextureRenderElement<GlesTexture> as RenderElement<GlesRenderer>>::draw(
            &self.inner,
            frame,
            src,
            dst,
            damage,
            opaque,
        )
    }
}
pub type AnimatedTexture = RescaleRenderElement<PixelTexture>;
#[derive(Default)]
pub struct Frame {
    pub decorations: HashMap<Id, Vec<AnimatedTexture>>,
    pub backdrops: HashMap<Id, crate::decoration::DecorationBackdrop>,
    pub closed_backdrops: Vec<(usize, u64, crate::decoration::DecorationBackdrop)>,
    pub live: HashMap<Id, AnimatedTexture>,
    pub closed: Vec<AnimatedTexture>,
    pub clips: HashMap<Id, super::rounded::ClientClip>,

    pub retained: HashMap<u64, Vec<Id>>,
}
fn progress(start: Instant, duration: u32) -> f32 {
    if duration == 0 {
        1.
    } else {
        (start.elapsed().as_secs_f32() * 1000. / duration as f32).min(1.)
    }
}
fn element(
    surface: &Surface,
    center: Point<i32, Physical>,
    renderer: &GlesRenderer,
    alpha: f32,
) -> AnimatedTexture {
    let texture = TextureRenderElement::from_static_texture(
        surface.id.clone(),
        renderer.context_id(),
        surface.geometry.loc.to_f64(),
        surface.texture.clone(),
        1,
        surface.transform,
        Some(alpha * surface.alpha),
        None,
        None,
        None,
        Kind::Unspecified,
    );

    RescaleRenderElement::from_element(
        PixelTexture::new(texture, surface.geometry, surface.source, surface.commit),
        center,
        1.,
    )
}
impl Motion {
    pub fn frame(
        &mut self,
        renderer: &mut GlesRenderer,
        state: &RixState,
        output: &Output,
        decorations: &crate::decoration::DecorationRenderer,
    ) -> Frame {
        let mut frame = Frame::default();
        self.outputs
            .retain(|name, _| state.space.outputs().any(|o| o.name() == *name));
        let windows = self.outputs.entry(output.name()).or_default();
        let scale = output.current_scale().fractional_scale();
        let Some(output_geo) = state.space.output_geometry(output) else {
            return frame;
        };
        let mut alive = HashSet::new();
        for (id, window) in &state.windows.entries {
            let (Some(geometry), Some(location)) = (
                state.space.element_geometry(window),
                state.space.element_location(window),
            ) else {
                continue;
            };
            let placement_pending = state
                .windows
                .pending_placement
                .get(id)
                .is_some_and(|pending| !pending.expired());
            if placement_pending {
                if let Some(entry) = windows
                    .get_mut(id)
                    .filter(|entry| !entry.surfaces.is_empty())
                {

                    entry.closed = None;

                    entry.opening = false;
                    if let Some(toplevel) = window.toplevel() {
                        let root = Id::from_wayland_resource(toplevel.wl_surface());
                        if let Some(backdrop) = &entry.backdrop {
                            frame.backdrops.insert(root.clone(), backdrop.clone());
                        }
                        frame.decorations.insert(
                            root,
                            entry
                                .decoration
                                .iter()
                                .map(|part| decoration(part, output_geo, entry.center, scale, 1.))
                                .collect(),
                        );
                    }
                    for surface in &entry.surfaces {
                        if let Some(clip) = surface.clip {
                            frame.clips.insert(surface.id.clone(), clip);
                        }
                        frame.live.insert(
                            surface.id.clone(),
                            element(surface, entry.center, renderer, 1.),
                        );
                    }
                    frame.retained.insert(
                        *id,
                        entry
                            .surfaces
                            .iter()
                            .map(|surface| surface.id.clone())
                            .collect(),
                    );
                    alive.insert(*id);
                    continue;
                }
            }
            if !geometry.overlaps(output_geo) {
                continue;
            }
            alive.insert(*id);
            let surfaces: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = window.render_elements(
                renderer,

                (location - window.geometry().loc - output_geo.loc)
                    .to_physical_precise_round(scale),
                scale.into(),
                1.,
            );
            let theme = state.decoration_for_window(window);
            let client_clip = (crate::decoration::enabled(window)
                && theme.client_corner_radius() > 0.)
                .then(|| super::rounded::ClientClip {
                    bounds: Rectangle::new(
                        (geometry.loc - output_geo.loc).to_f64(),
                        geometry.size.to_f64(),
                    ),
                    radius: theme.client_corner_radius(),
                });
            let client_ids = client_clip
                .map(|_| super::rounded::client_surface_ids(window))
                .unwrap_or_default();
            let mut retained = Vec::new();
            let mut bytes = 0u64;
            for surface in surfaces {
                if let WaylandSurfaceTexture::Texture(texture) = surface.texture() {
                    bytes += texture.size().w as u64 * texture.size().h as u64 * 4;
                    if bytes > 64 * 1024 * 1024 {
                        retained.clear();
                        break;
                    }
                    retained.push(Surface {
                        id: surface.id().clone(),
                        texture: texture.clone(),
                        source: surface.src(),
                        transform: surface.transform(),
                        geometry: surface.geometry(scale.into()),
                        alpha: surface.alpha(),
                        commit: surface.current_commit(),
                        clip: client_ids
                            .contains(surface.id())
                            .then_some(client_clip)
                            .flatten(),
                    });
                }
            }
            let center = (geometry.loc - output_geo.loc
                + Point::from((geometry.size.w / 2, geometry.size.h / 2)))
            .to_physical_precise_round(scale);
            let entry = windows.entry(*id).or_insert_with(|| Window {
                bytes: 0,
                decoration: Vec::new(),
                backdrop: None,
                surfaces: Vec::new(),
                center,
                born: Instant::now(),
                opening: true,
                closed: None,
            });
            if entry.closed.take().is_some() {
                entry.born = Instant::now();
                entry.opening = true;
            }
            if placement_pending {
                entry.opening = false;
            }
            entry.center = center;
            entry.surfaces = retained;
            entry.decoration = decorations.snapshot(*id);
            entry.backdrop = decorations.backdrop_snapshot(*id);
            entry.bytes = entry
                .surfaces
                .iter()
                .map(|surface| {
                    let size = surface.texture.size();
                    size.w as u64 * size.h as u64 * 4
                })
                .sum::<u64>();
            let t = if entry.opening {
                progress(entry.born, state.animation_duration)
            } else {
                1.
            };
            if t >= 1. {
                entry.opening = false;
            }
            if t < 1. {
                let alpha = 1. - (1. - t).powi(3);
                if let Some(toplevel) = window.toplevel() {
                    if let Some(backdrop) = &entry.backdrop {
                        frame.backdrops.insert(
                            Id::from_wayland_resource(toplevel.wl_surface()),
                            backdrop.clone().transformed(
                                center.to_f64().to_logical(scale).to_i32_round() + output_geo.loc,
                                1.,
                                alpha as f64,
                            ),
                        );
                    }
                    frame.decorations.insert(
                        Id::from_wayland_resource(toplevel.wl_surface()),
                        entry
                            .decoration
                            .iter()
                            .map(|part| decoration(part, output_geo, center, scale, alpha))
                            .collect(),
                    );
                }
                for surface in &entry.surfaces {
                    if let Some(clip) = surface.clip {
                        frame.clips.insert(surface.id.clone(), clip);
                    }
                    frame.live.insert(
                        surface.id.clone(),
                        element(surface, center, renderer, alpha),
                    );
                }
            }
        }
        let mut retiring_bytes = 0u64;
        windows.retain(|id, window| {
            if alive.contains(id) {
                return true;
            }
            let closed = *window.closed.get_or_insert_with(Instant::now);
            let t = progress(closed, state.animation_duration);
            if t >= 1. {
                return false;
            }
            retiring_bytes += window.bytes;

            if retiring_bytes > 256 * 1024 * 1024 {
                return false;
            }
            let alpha = (1. - t).powi(3);
            for surface in &window.surfaces {
                if let Some(clip) = surface.clip {
                    frame.clips.insert(surface.id.clone(), clip);
                }
                frame
                    .closed
                    .push(element(surface, window.center, renderer, alpha));
            }
            for part in &window.decoration {
                frame
                    .closed
                    .push(decoration(part, output_geo, window.center, scale, alpha));
            }
            if let Some(backdrop) = &window.backdrop {
                frame.closed_backdrops.push((
                    frame.closed.len(),
                    *id,
                    backdrop.clone().transformed(
                        window.center.to_f64().to_logical(scale).to_i32_round() + output_geo.loc,
                        1.,
                        alpha as f64,
                    ),
                ));
            }
            true
        });

        if windows.len() > 128 {
            windows.retain(|id, _| alive.contains(id));
        }
        frame
    }
}

fn decoration(
    part: &crate::decoration::DecorationPart,
    output: Rectangle<i32, Logical>,
    center: Point<i32, Physical>,
    scale: f64,
    alpha: f32,
) -> AnimatedTexture {
    let texture = part.element(output.loc, scale, alpha);
    let geometry = texture.geometry(scale.into());
    let source = texture.src();
    let commit = texture.current_commit();
    RescaleRenderElement::from_element(
        PixelTexture::new(texture, geometry, source, commit),
        center,
        1.,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::tests::TestDesktop;
    use smithay::backend::egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay};
    use smithay::backend::{
        allocator::Fourcc,
        renderer::{damage::OutputDamageTracker, Bind, ExportMem, Offscreen},
    };
    use std::{cell::RefCell, time::Duration};

    fn read<E: RenderElement<GlesRenderer>>(
        renderer: &mut GlesRenderer,
        elements: &[E],
        scale: f64,
    ) -> Vec<u8> {
        let size = (96, 72).into();
        let mut texture: GlesTexture = renderer.create_buffer(Fourcc::Abgr8888, size).unwrap();
        let mut framebuffer = renderer.bind(&mut texture).unwrap();
        OutputDamageTracker::new((96, 72), scale, Transform::Normal)
            .render_output(renderer, &mut framebuffer, 0, elements, [0., 0., 0., 1.])
            .unwrap();
        let mapping = renderer
            .copy_framebuffer(
                &framebuffer,
                Rectangle::from_size((96, 72).into()),
                Fourcc::Abgr8888,
            )
            .unwrap();
        renderer.map_texture(&mapping).unwrap().to_vec()
    }

    #[test]
    #[ignore = "Requires a surfaceless EGL renderer"]
    fn opening_preserves_crisp_pixels_and_geometry_for_fractional_scale_and_viewport_transforms() {
        use wayland_client::protocol::wl_output;
        thread_local! {
            static GPU: RefCell<Option<(GlesRenderer, Motion, crate::decoration::DecorationRenderer)>> = const { RefCell::new(None) };
        }
        for scale in [1., 1.5] {
            for transform in [
                wl_output::Transform::Normal,
                wl_output::Transform::_90,
                wl_output::Transform::Flipped,
                wl_output::Transform::_270,
            ] {
                let mut desktop = TestDesktop::new();
                let window = desktop.window("Crisp opening fixture");
                let mut pixels = vec![0; 20 * 16 * 4];
                for y in 0..16 {
                    for x in 0..20 {
                        let offset = (y * 20 + x) * 4;
                        pixels[offset..offset + 4].copy_from_slice(&[
                            if (x + y) % 2 == 0 { 240 } else { 16 },
                            (x * 11) as u8,
                            (y * 13) as u8,
                            255,
                        ]);
                    }
                }
                let buffer = desktop.pixel_buffer(20, 16, &pixels);
                let viewport = desktop.viewport(&window.0);
                viewport.set_source(2., 3., 11., 7.);
                viewport.set_destination(17, 13);
                window.1.set_window_geometry(3, 2, 12, 10);
                window.0.set_buffer_transform(transform);
                window.0.attach(Some(&buffer), 0, 0);
                window.0.commit();
                desktop.roundtrip();
                desktop.inspect(move |state| {
                    state.animation_duration = 1000;
                    let output = state.space.outputs().next().unwrap().clone();
                    output.change_current_state(
                        Some(smithay::output::Mode {
                            size: (96, 72).into(),
                            refresh: 60_000,
                        }),
                        None,
                        Some(smithay::output::Scale::Fractional(scale)),
                        None,
                    );
                    state
                        .space
                        .map_element(state.windows.entries[0].1.clone(), (5, 7), false);
                    state.space.refresh();
                });
                for tick in [0, 350, 900, 1100] {
                    desktop.inspect(move |state| GPU.with(|gpu| {
                        let mut gpu = gpu.borrow_mut();
                        let (renderer, motion, decorations) = gpu.get_or_insert_with(|| {
                            let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
                            let context = EGLContext::new(&display).unwrap();
                            (unsafe { GlesRenderer::new(context) }.unwrap(), Motion::default(), Default::default())
                        });
                        let output = state.space.outputs().next().unwrap().clone();
                        let (id, window) = &state.windows.entries[0];
                        if let Some(entry) = motion.outputs.get_mut(&output.name()).and_then(|windows| windows.get_mut(id)) {
                            entry.born = Instant::now() - Duration::from_millis(tick);
                        }
                        let frame = motion.frame(renderer, state, &output, decorations);
                        let root = Id::from_wayland_resource(window.toplevel().unwrap().wl_surface());
                        let location = (state.space.element_location(window).unwrap() - window.geometry().loc).to_physical_precise_round(scale);
                        let alpha = frame.live.get(&root).map_or(1., Element::alpha);
                        let live: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = window.render_elements(renderer, location, scale.into(), alpha);
                        let expected = &live[0];
                        if tick < 1000 {
                            let animated = frame.live.get(&root).expect("Opening did not present its GPU snapshot");
                            assert_eq!(animated.geometry(scale.into()), expected.geometry(scale.into()), "Opening moved or rescaled client pixels");
                            assert_eq!(animated.src(), expected.src(), "Snapshot changed the viewport crop");
                            assert_eq!(animated.transform(), expected.transform());
                            assert_eq!(animated.current_commit(), expected.current_commit());
                            let actual_pixels = read(renderer, std::slice::from_ref(animated), scale);
                            let expected_pixels = read(renderer, &live, scale);
                            assert_eq!(actual_pixels, expected_pixels, "Opening resampled text/checkerboard pixels at scale {scale}, transform {transform:?}, tick {tick}");
                        } else {
                            assert!(!frame.live.contains_key(&root), "Settled window continued animating");
                        }
                    }));
                }
            }
        }
    }

    #[test]
    #[ignore = "Requires a surfaceless EGL renderer"]
    fn maximizing_during_opening_never_restarts_fade_or_hides_the_display_scene() {
        thread_local! {
            static GPU: RefCell<Option<(GlesRenderer, super::super::SceneRenderer)>> = const { RefCell::new(None) };
        }
        for scale in [1., 1.5] {
            let mut desktop = TestDesktop::new();
            let window = desktop.window("Maximize while opening fixture");
            let normal = desktop.pixel_buffer(17, 13, &[20, 180, 240, 255].repeat(17 * 13));
            window.0.attach(Some(&normal), 0, 0);
            window.0.commit();
            desktop.roundtrip();
            let id = desktop.inspect(move |state| {
                state.animation_duration = 60_000;
                let output = state.space.outputs().next().unwrap().clone();
                output.change_current_state(
                    Some(smithay::output::Mode {
                        size: (96, 72).into(),
                        refresh: 60_000,
                    }),
                    None,
                    Some(smithay::output::Scale::Fractional(scale)),
                    None,
                );
                state
                    .space
                    .map_element(state.windows.entries[0].1.clone(), (5, 7), false);
                state.space.refresh();
                state.windows.entries[0].0
            });
            let render = move |state: &mut RixState| {
                GPU.with(|gpu| {
                    let mut gpu = gpu.borrow_mut();
                    let (renderer, scene) = gpu.get_or_insert_with(|| {
                        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
                        let context = EGLContext::new(&display).unwrap();
                        (
                            unsafe { GlesRenderer::new(context) }.unwrap(),
                            Default::default(),
                        )
                    });
                    let output = state.space.outputs().next().unwrap().clone();
                    let root = Id::from_wayland_resource(
                        state.windows.entries[0].1.toplevel().unwrap().wl_surface(),
                    );
                    let elements = scene
                        .compose(
                            renderer,
                            state,
                            &output,
                            None,
                            &state.decoration_theme.material_effects(),
                        )
                        .unwrap();
                    let client: Vec<_> = elements
                        .into_iter()
                        .filter(|element| element.id() == &root)
                        .collect();
                    assert_eq!(
                        client.len(),
                        1,
                        "The maximized client disappeared from the display scene"
                    );
                    (
                        client[0].alpha(),
                        client[0].geometry(scale.into()),
                        read(renderer, &client, scale),
                    )
                })
            };
            let opening = desktop.inspect(render);
            assert!(opening.0 < 1., "Fixture did not catch the opening fade");
            desktop.inspect(move |state| {
                state
                    .apply_window_action(rix_ipc::WindowAction::Maximize(id))
                    .unwrap()
            });
            desktop.roundtrip();
            let waiting = desktop.inspect(render);
            assert_eq!(waiting.0, 1.);
            assert_eq!(
                waiting.1, opening.1,
                "Maximize moved the old buffer before a replacement existed"
            );
            assert!(
                waiting.2.chunks_exact(4).any(|pixel| pixel[1] > 170),
                "Saved maximize scene rendered no client pixels"
            );
            let target = desktop.inspect(move |state| state.windows.pending_placement[&id].target);
            let maximized = desktop.pixel_buffer(
                target.size.w,
                target.size.h,
                &[20, 180, 240, 255].repeat((target.size.w * target.size.h) as usize),
            );
            window
                .1
                .set_window_geometry(0, 0, target.size.w, target.size.h);
            window.0.attach(Some(&maximized), 0, 0);
            window.0.commit();
            desktop.roundtrip();
            let shown = desktop.inspect(render);
            assert_eq!(
                shown.0, 1.,
                "Completing maximize restarted the earlier fade and made the window disappear"
            );
            assert_eq!(shown.1, target.to_physical_precise_round(scale));
            assert!(shown.2.chunks_exact(4).any(|pixel| pixel[1] > 170));
            desktop.inspect(move |state| {
                assert!(!state.windows.minimized.contains_key(&id));
                assert_eq!(state.windows.active, Some(id));
                assert!(!state.windows.pending_placement.contains_key(&id));
            });
        }
    }

    #[test]
    #[ignore = "Requires a surfaceless EGL renderer"]
    fn maximize_retains_a_disappearing_subsurface_in_the_actual_display_scene() {
        thread_local! {
            static GPU: RefCell<Option<(GlesRenderer, super::super::SceneRenderer)>> = const { RefCell::new(None) };
        }
        let mut desktop = TestDesktop::new();
        let window = desktop.window("Subsurface resize fixture");
        let (child, child_role) = desktop.subsurface(&window.0);
        child_role.set_position(20, 0);
        child.attach(Some(&desktop.sized_buffer(24, 16)), 0, 0);
        child.commit();
        window.0.commit();
        desktop.roundtrip();
        let id = desktop.inspect(|state| {

            state.animation_duration = 0;
            state.windows.entries[0].0
        });
        let render = move |state: &mut RixState| {
            GPU.with(|gpu| {
                let mut gpu = gpu.borrow_mut();
                let (renderer, scene) = gpu.get_or_insert_with(|| {
                    let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
                    let context = EGLContext::new(&display).unwrap();
                    (
                        unsafe { GlesRenderer::new(context) }.unwrap(),
                        Default::default(),
                    )
                });
                let output = state.space.outputs().next().unwrap().clone();
                let elements = scene
                    .compose(
                        renderer,
                        state,
                        &output,
                        None,
                        &state.decoration_theme.material_effects(),
                    )
                    .unwrap();
                elements
                    .iter()
                    .filter(|element| element.geometry(1.0.into()).size == (24, 16).into())
                    .count()
            })
        };
        assert_eq!(desktop.inspect(render), 1);
        desktop.inspect(move |state| {
            state
                .apply_window_action(rix_ipc::WindowAction::Maximize(id))
                .unwrap()
        });
        desktop.roundtrip();
        child.attach(None, 0, 0);
        child.commit();
        window.0.commit();
        desktop.roundtrip();
        assert_eq!(
            desktop.inspect(render),
            1,
            "The presented client surface disappeared while maximize awaited its buffer"
        );
        let target = desktop.inspect(move |state| state.windows.pending_placement[&id].target);
        window
            .1
            .set_window_geometry(0, 0, target.size.w, target.size.h);
        window.0.attach(
            Some(&desktop.sized_buffer(target.size.w, target.size.h)),
            0,
            0,
        );
        window.0.commit();
        desktop.roundtrip();
        assert_eq!(
            desktop.inspect(render),
            0,
            "The intermediate surface survived the matching maximize commit"
        );
    }

    #[test]
    #[ignore = "Requires a surfaceless EGL renderer"]
    fn pending_resize_keeps_last_gpu_frame_until_the_matching_buffer_commits() {
        thread_local! {
            static GPU: RefCell<Option<(GlesRenderer, Motion, crate::decoration::DecorationRenderer)>> = const { RefCell::new(None) };
        }
        let mut desktop = TestDesktop::new();
        let window = desktop.window("GPU resize fixture");
        let id = desktop.inspect(|state| {
            state.animation_duration = 120;
            state.windows.entries[0].0
        });
        let render = move |state: &mut RixState| {
            GPU.with(|gpu| {
                let mut gpu = gpu.borrow_mut();
                let (renderer, motion, decorations) = gpu.get_or_insert_with(|| {
                    let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
                    let context = EGLContext::new(&display).unwrap();
                    (
                        unsafe { GlesRenderer::new(context) }.unwrap(),
                        Motion::default(),
                        Default::default(),
                    )
                });
                let output = state.space.outputs().next().unwrap().clone();
                decorations.elements(renderer, state, &output);
                let frame = motion.frame(renderer, state, &output, decorations);
                let cached = motion
                    .outputs
                    .get_mut(&output.name())
                    .unwrap()
                    .get_mut(&id)
                    .unwrap();
                cached.born = Instant::now() - Duration::from_secs(1);
                let root = Id::from_wayland_resource(
                    state.windows.entries[0].1.toplevel().unwrap().wl_surface(),
                );
                (
                    cached.surfaces[0].texture.size(),
                    frame
                        .live
                        .get(&root)
                        .map(|element| element.geometry(1.0.into())),
                    cached.surfaces[0].geometry,
                )
            })
        };
        let before = desktop.inspect(render);
        assert_eq!(before.0, (16, 16).into());
        desktop.inspect(move |state| {
            state
                .apply_window_action(rix_ipc::WindowAction::Maximize(id))
                .unwrap();
        });
        desktop.roundtrip();

        let wrong_size = desktop.sized_buffer(32, 32);
        window.0.attach(Some(&wrong_size), 0, 0);
        window.0.commit();
        desktop.roundtrip();
        let waiting = desktop.inspect(render);
        assert_eq!(
            waiting.0, before.0,
            "Pending resize replaced the retained GPU image"
        );
        assert_eq!(
            waiting.1,
            Some(before.2),
            "Pending resize moved or resized the old frame"
        );
        assert_eq!(waiting.2, before.2);

        let target = desktop.inspect(move |state| state.windows.pending_placement[&id].target);
        let matching = desktop.sized_buffer(target.size.w, target.size.h);
        window
            .1
            .set_window_geometry(0, 0, target.size.w, target.size.h);
        window.0.attach(Some(&matching), 0, 0);
        window.0.commit();
        desktop.roundtrip();
        let after = desktop.inspect(render);
        assert_eq!(after.0, target.size.to_buffer(1, Transform::Normal));
        assert_eq!(after.2, target.to_physical(1));
        assert!(
            after.1.is_none(),
            "Completed resize retained the old animation frame"
        );
    }
}
