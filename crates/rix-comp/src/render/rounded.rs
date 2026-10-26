use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement},
        gles::{
            GlesError, GlesFrame, GlesRenderer, GlesTexProgram, Uniform, UniformName, UniformType,
        },
        utils::{CommitCounter, OpaqueRegions},
    },
    desktop::Window,
    utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Transform},
};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClientClip {
    pub bounds: Rectangle<f64, Logical>,
    pub radius: f64,
}
impl ClientClip {
    #[cfg(test)]
    pub fn transformed(self, center: Point<f64, Logical>, factor: f64) -> Self {
        Self {
            bounds: Rectangle::new(
                (
                    (self.bounds.loc.x - center.x) * factor + center.x,
                    (self.bounds.loc.y - center.y) * factor + center.y,
                )
                    .into(),
                self.bounds.size * factor,
            ),
            radius: self.radius * factor,
        }
    }
    pub fn contains(self, point: Point<f64, Logical>) -> bool {
        if !self.bounds.contains(point) {
            return false;
        }
        let r = self
            .radius
            .min(self.bounds.size.w / 2.)
            .min(self.bounds.size.h)
            .max(0.);
        let local = point - self.bounds.loc;
        if local.y <= self.bounds.size.h - r {
            return true;
        }
        let dx = (local.x - self.bounds.size.w / 2.).abs() - (self.bounds.size.w / 2. - r);
        let dy = local.y - (self.bounds.size.h - r);
        dx.max(0.).hypot(dy) <= r
    }
}

pub fn client_surface_ids(window: &Window) -> HashSet<Id> {
    let mut ids = HashSet::new();
    if let Some(toplevel) = window.toplevel() {
        smithay::wayland::compositor::with_surface_tree_downward(
            toplevel.wl_surface(),
            (),
            |_, _, _| smithay::wayland::compositor::TraversalAction::DoChildren(()),
            |surface, _, _| {
                ids.insert(Id::from_wayland_resource(surface));
            },
            |_, _, _| true,
        );
    }
    ids
}

pub fn program(renderer: &mut GlesRenderer) -> Result<GlesTexProgram, GlesError> {
    renderer.compile_custom_texture_shader(
        include_str!("rounded.frag"),
        &[
            UniformName::new("clip_rect", UniformType::_4f),
            UniformName::new("clip_radius", UniformType::_1f),
            UniformName::new("clip_scale", UniformType::_2f),
            UniformName::new("grain", UniformType::_3f),
        ],
    )
}

#[derive(Default)]
pub struct Cache(HashMap<Id, (ClientClip, CommitCounter, CommitCounter)>);
impl Cache {
    pub fn wrap<E: Element>(
        &mut self,
        inner: E,
        clip: ClientClip,
        program: &GlesTexProgram,
        scale: f64,
    ) -> Clipped<E> {
        let source_commit = inner.current_commit();
        let state = self.0.entry(inner.id().clone()).or_insert_with(|| {
            let mut commit = source_commit;
            commit.increment();
            (clip, source_commit, commit)
        });
        if state.0 != clip || state.1 != source_commit {
            state.0 = clip;
            state.1 = source_commit;
            state.2.increment();
        }
        Clipped {
            geometry: inner.geometry(scale.into()),
            inner,
            clip,
            program: program.clone(),
            commit: state.2,
            scale,
            grain: [0., 1., 0.5],
        }
    }
    pub fn retain(&mut self, ids: &HashSet<Id>) {
        self.0.retain(|id, _| ids.contains(id));
    }
}

#[derive(Debug)]
pub struct Clipped<E> {
    inner: E,
    clip: ClientClip,
    program: GlesTexProgram,
    commit: CommitCounter,
    geometry: Rectangle<i32, Physical>,
    scale: f64,
    grain: [f32; 3],
}
impl<E> Clipped<E> {

    pub fn with_grain(mut self, grain: [f32; 3]) -> Self {
        self.grain = grain;
        self
    }
}
impl<E: Element> Element for Clipped<E> {
    fn id(&self) -> &Id {
        self.inner.id()
    }
    fn current_commit(&self) -> CommitCounter {
        self.commit
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }
    fn transform(&self) -> Transform {
        self.inner.transform()
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }
    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }
    fn kind(&self) -> Kind {
        self.inner.kind()
    }
    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        let rect: Rectangle<i32, Physical> = self.clip.bounds.to_physical_precise_round(self.scale);
        let r = (self.clip.radius * self.scale + 1.).ceil() as i32;
        let safe = [
            Rectangle::new(rect.loc, (rect.size.w, (rect.size.h - r).max(0)).into()),
            Rectangle::new(
                (rect.loc.x + r, rect.loc.y).into(),
                ((rect.size.w - 2 * r).max(0), rect.size.h).into(),
            ),
        ];
        let mut regions = Vec::new();
        for opaque in self.inner.opaque_regions(scale) {
            let global = Rectangle::new(opaque.loc + self.geometry.loc, opaque.size);
            for safe in safe {
                if let Some(rect) = global.intersection(safe) {
                    regions.push(Rectangle::new(rect.loc - self.geometry.loc, rect.size));
                }
            }
        }
        OpaqueRegions::from_slice(&regions)
    }
}
impl<E: RenderElement<GlesRenderer>> RenderElement<GlesRenderer> for Clipped<E> {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {

        let sx = dst.size.w as f64 / self.geometry.size.w.max(1) as f64;
        let sy = dst.size.h as f64 / self.geometry.size.h.max(1) as f64;
        let rect = self.clip.bounds;
        frame.override_default_tex_program(
            self.program.clone(),
            vec![
                Uniform::new(
                    "clip_rect",
                    [
                        (rect.loc.x * self.scale - self.geometry.loc.x as f64) as f32,
                        (rect.loc.y * self.scale - self.geometry.loc.y as f64) as f32,
                        (rect.size.w * self.scale) as f32,
                        (rect.size.h * self.scale) as f32,
                    ],
                ),
                Uniform::new("clip_radius", (self.clip.radius * self.scale) as f32),
                Uniform::new("clip_scale", [sx as f32, sy as f32]),
                Uniform::new("grain", self.grain),
            ],
        );
        let result = self.inner.draw(frame, src, dst, damage, opaque);
        frame.clear_tex_program_override();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_lower_content_corners_are_cut_and_motion_keeps_the_contour() {
        let clip = ClientClip {
            bounds: Rectangle::new((10., 20.).into(), (80., 60.).into()),
            radius: 8.,
        };
        assert!(clip.contains((10.1, 20.1).into()));
        assert!(clip.contains((50., 79.9).into()));
        assert!(!clip.contains((10.1, 79.9).into()));
        assert!(!clip.contains((89.9, 79.9).into()));
        let center = (50., 50.).into();
        for factor in [0.97, 1., 1.5, 2.] {
            let moved = clip.transformed(center, factor);
            for point in [(10.1, 79.9), (50., 79.9), (10.1, 20.1)] {
                let point: Point<f64, Logical> = point.into();
                assert_eq!(
                    clip.contains(point),
                    moved.contains(
                        (
                            (point.x - center.x) * factor + center.x,
                            (point.y - center.y) * factor + center.y
                        )
                            .into()
                    )
                );
            }
        }
        let square = ClientClip { radius: 0., ..clip };
        assert!(square.contains((10.1, 79.9).into()));
    }

    #[test]
    #[ignore = "requires EGL; verifies client texture clipping at output scales and transforms"]
    fn gpu_client_clip_matches_frame_contour_without_cutting_popups_or_maximized_content() {
        use super::super::{coverage, SceneElement};
        use smithay::backend::{
            allocator::Fourcc,
            egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay},
            renderer::{
                damage::OutputDamageTracker,
                element::texture::{TextureBuffer, TextureRenderElement},
                Bind, ExportMem, Offscreen,
            },
        };
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        let context = EGLContext::new(&display).unwrap();
        let mut renderer = unsafe { GlesRenderer::new(context) }.unwrap();
        let program = program(&mut renderer).unwrap();
        let coverage_program = coverage::program(&mut renderer).unwrap();
        let mut cache = Cache::default();
        let clip = ClientClip {
            bounds: Rectangle::new((20., 20.).into(), (48., 48.).into()),
            radius: 8.,
        };
        for scale in [1., 1.5, 2.] {
            let size: smithay::utils::Size<i32, Physical> =
                ((96. * scale) as i32, (96. * scale) as i32).into();
            let texture = TextureBuffer::from_memory(
                &mut renderer,
                &[0, 255, 0, 255].repeat(48 * 48),
                Fourcc::Abgr8888,
                (48, 48),
                false,
                1,
                Transform::Normal,
                Some(vec![Rectangle::from_size((48, 48).into())]),
            )
            .unwrap();
            let white = TextureBuffer::from_memory(
                &mut renderer,
                &vec![255; (size.w * size.h * 4) as usize],
                Fourcc::Abgr8888,
                (size.w, size.h),
                false,
                1,
                Transform::Normal,
                None,
            )
            .unwrap();
            let red = TextureBuffer::from_memory(
                &mut renderer,
                &[255, 0, 0, 255].repeat(8 * 8),
                Fourcc::Abgr8888,
                (8, 8),
                false,
                1,
                Transform::Normal,
                None,
            )
            .unwrap();
            let draw = |renderer: &mut GlesRenderer, scene: &[SceneElement], transform| {
                let mut target: smithay::backend::renderer::gles::GlesTexture = renderer
                    .create_buffer(Fourcc::Abgr8888, (size.w, size.h).into())
                    .unwrap();
                let memory = {
                    let mut fb = renderer.bind(&mut target).unwrap();
                    OutputDamageTracker::new(size, scale, transform)
                        .render_output(renderer, &mut fb, 0, scene, [0., 0., 0., 1.])
                        .unwrap();
                    renderer
                        .copy_framebuffer(
                            &fb,
                            Rectangle::from_size((size.w, size.h).into()),
                            Fourcc::Abgr8888,
                        )
                        .unwrap()
                };
                renderer.map_texture(&memory).unwrap().to_vec()
            };
            for transform in [Transform::Normal, Transform::_90, Transform::Flipped180] {
                let transformed = TextureBuffer::from_memory(
                    &mut renderer,
                    &[0, 255, 0, 255].repeat(48 * 48),
                    Fourcc::Abgr8888,
                    (48, 48),
                    false,
                    1,
                    transform,
                    None,
                )
                .unwrap();
                let source = TextureRenderElement::from_texture_buffer(
                    (20. * scale, 20. * scale),
                    &transformed,
                    None,
                    Some(Rectangle::new((4., 6.).into(), (32., 30.).into())),
                    Some((48, 48).into()),
                    Kind::Unspecified,
                );
                let clipped =
                    SceneElement::TextureRounded(cache.wrap(source, clip, &program, scale));
                let actual = draw(&mut renderer, &[clipped], transform);
                let shape = crate::surface_effects::Shape {
                    x: 0.,
                    y: 0.,
                    width: 96.,
                    height: 96.,
                    radius: 0.,
                    opacity: 1.,
                    clip: None,
                };
                let reference = SceneElement::Masked(coverage::element_with_client_clip(
                    &white,
                    &coverage_program,
                    &[shape],
                    Rectangle::from_size(size),
                    size,
                    scale,
                    Some((clip, false)),
                ));
                let expected = draw(&mut renderer, &[reference], transform);
                for (index, (actual, expected)) in actual
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(expected.as_chunks::<4>().0)
                    .enumerate()
                {
                    assert!((actual[1] as i16 - expected[1] as i16).abs() <= 2, "GPU clip scale {scale}, transform {transform:?}, pixel {index}: {actual:?} vs {expected:?}");
                }
            }
            let source = || {
                TextureRenderElement::from_texture_buffer(
                    (20. * scale, 20. * scale),
                    &texture,
                    None,
                    None,
                    Some((48, 48).into()),
                    Kind::Unspecified,
                )
            };
            let popup = SceneElement::Texture(TextureRenderElement::from_texture_buffer(
                (16. * scale, 60. * scale),
                &red,
                None,
                None,
                Some((8, 8).into()),
                Kind::Unspecified,
            ));
            let actual = draw(
                &mut renderer,
                &[
                    popup,
                    SceneElement::TextureRounded(cache.wrap(source(), clip, &program, scale)),
                ],
                Transform::Normal,
            );
            let sample = |bytes: &[u8], x: f64, y: f64| {
                let index = (((y * scale) as i32 * size.w + (x * scale) as i32) * 4) as usize;
                [
                    bytes[index],
                    bytes[index + 1],
                    bytes[index + 2],
                    bytes[index + 3],
                ]
            };
            assert_eq!(
                sample(&actual, 20., 66.),
                [255, 0, 0, 255],
                "popup must remain visible over the parent's cut corner"
            );
            assert_eq!(
                sample(&actual, 50., 60.),
                [0, 255, 0, 255],
                "client center stays sharp and opaque"
            );
            let square = ClientClip { radius: 0., ..clip };
            let actual = draw(
                &mut renderer,
                &[SceneElement::TextureRounded(cache.wrap(
                    source(),
                    square,
                    &program,
                    scale,
                ))],
                Transform::Normal,
            );
            assert_eq!(
                sample(&actual, 20., 67.),
                [0, 255, 0, 255],
                "maximized content must keep its square lower corner"
            );

            let mut tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
            let mut target: smithay::backend::renderer::gles::GlesTexture = renderer
                .create_buffer(Fourcc::Abgr8888, (size.w, size.h).into())
                .unwrap();
            for radius in [8., 0., 8.] {
                let element = SceneElement::TextureRounded(cache.wrap(
                    source(),
                    ClientClip { radius, ..clip },
                    &program,
                    scale,
                ));
                let memory = {
                    let mut fb = renderer.bind(&mut target).unwrap();
                    tracker
                        .render_output(&mut renderer, &mut fb, 0, &[element], [0., 0., 0., 1.])
                        .unwrap();
                    renderer
                        .copy_framebuffer(
                            &fb,
                            Rectangle::from_size((size.w, size.h).into()),
                            Fourcc::Abgr8888,
                        )
                        .unwrap()
                };
                let bytes = renderer.map_texture(&memory).unwrap();
                assert_eq!(
                    sample(bytes, 20., 67.)[1],
                    if radius == 0. { 255 } else { 0 },
                    "radius change must invalidate stale corner damage"
                );
            }

            let element = cache.wrap(source(), clip, &program, scale);
            let scaled =
                smithay::backend::renderer::element::utils::RescaleRenderElement::from_element(
                    element,
                    (0, 0).into(),
                    (0.75, 0.5),
                );
            let reduced: smithay::utils::Size<i32, Physical> =
                ((size.w as f64 * 0.75) as i32, size.h / 2).into();
            let mut target: smithay::backend::renderer::gles::GlesTexture = renderer
                .create_buffer(Fourcc::Abgr8888, (reduced.w, reduced.h).into())
                .unwrap();
            let memory = {
                let mut fb = renderer.bind(&mut target).unwrap();
                OutputDamageTracker::new(reduced, scale, Transform::Normal)
                    .render_output(&mut renderer, &mut fb, 0, &[scaled], [0., 0., 0., 1.])
                    .unwrap();
                renderer
                    .copy_framebuffer(
                        &fb,
                        Rectangle::from_size((reduced.w, reduced.h).into()),
                        Fourcc::Abgr8888,
                    )
                    .unwrap()
            };
            let bytes = renderer.map_texture(&memory).unwrap();
            let corner = (((67. * scale * 0.5) as i32 * reduced.w + (20. * scale * 0.75) as i32)
                * 4
                + 1) as usize;
            let center = (((60. * scale * 0.5) as i32 * reduced.w + (50. * scale * 0.75) as i32)
                * 4
                + 1) as usize;
            assert_eq!(
                bytes[corner], 0,
                "downsampling must preserve the cut corner"
            );
            assert_eq!(
                bytes[center], 255,
                "downsampling must preserve the opaque client center"
            );
        }
    }
}
