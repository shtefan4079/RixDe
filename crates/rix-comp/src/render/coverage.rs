use crate::surface_effects::{Shape, MAX_SHAPES};
use smithay::{
    backend::renderer::{
        element::{
            texture::{TextureBuffer, TextureRenderElement},
            utils::RescaleRenderElement,
            Element, Id, Kind, RenderElement,
        },
        gles::{
            element::TextureShaderElement, GlesError, GlesFrame, GlesRenderer, GlesTexProgram,
            GlesTexture, Uniform, UniformName, UniformType,
        },
        utils::CommitCounter,
    },
    utils::{Buffer, Physical, Rectangle, Scale, Size},
};
use std::rc::Rc;
#[derive(Debug, Clone)]
pub struct Masked(Rc<RescaleRenderElement<TextureShaderElement>>);
impl Element for Masked {
    fn id(&self) -> &Id {
        self.0.id()
    }
    fn current_commit(&self) -> CommitCounter {
        self.0.current_commit()
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        self.0.src()
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.0.geometry(scale)
    }
    fn alpha(&self) -> f32 {
        self.0.alpha()
    }
}
impl RenderElement<GlesRenderer> for Masked {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        <RescaleRenderElement<TextureShaderElement> as RenderElement<GlesRenderer>>::draw(
            &self.0, frame, src, dst, damage, opaque,
        )
    }
}
pub fn program(renderer: &mut GlesRenderer) -> Result<GlesTexProgram, GlesError> {
    let mut shader = String::from("#version 100\n//_DEFINES_\nprecision highp float;\nuniform sampler2D tex;\nuniform float alpha;\nuniform vec2 output_size;\nuniform vec2 output_origin;\nuniform vec3 grain;\nvarying vec2 v_coords;\n");
    let mut uniforms = vec![
        UniformName::new("output_size", UniformType::_2f),
        UniformName::new("output_origin", UniformType::_2f),
        UniformName::new("grain", UniformType::_3f),
    ];
    uniforms.push(UniformName::new("client_rect", UniformType::_4f));
    uniforms.push(UniformName::new("client_curve", UniformType::_2f));
    shader.push_str("uniform vec4 client_rect;\nuniform vec2 client_curve;\n");
    for i in 0..MAX_SHAPES {
        shader.push_str(&format!(
            "uniform vec4 rect{i};\nuniform vec2 curve{i};\nuniform vec4 clip{i};\n"
        ));
        uniforms.push(UniformName::new(format!("rect{i}"), UniformType::_4f));
        uniforms.push(UniformName::new(format!("curve{i}"), UniformType::_2f));
        uniforms.push(UniformName::new(format!("clip{i}"), UniformType::_4f));
    }
    shader.push_str("float coverage(vec2 p, vec4 r, vec2 curve, vec4 clip) {\n vec2 half_size = r.zw * 0.5;\n vec2 q = abs(p - r.xy - half_size) - half_size + curve.x;\n float d = length(max(q,vec2(0.0))) + min(max(q.x,q.y),0.0) - curve.x;\n vec2 cq = abs(p - clip.xy - clip.zw * 0.5) - clip.zw * 0.5;\n d = max(d, max(cq.x,cq.y));\n return (1.0-smoothstep(-0.5,0.5,d)) * curve.y;\n}\nvoid main() {\n vec2 p = v_coords * output_size + output_origin;\n float mask = 0.0;\n");
    for i in 0..MAX_SHAPES {
        shader.push_str(&format!(
            " if (curve{i}.y > 0.0) mask = max(mask,coverage(p,rect{i},curve{i},clip{i}));\n"
        ));
    }
    shader.push_str(" if (client_curve.y != 0.0) {\n float r = min(client_curve.x,min(client_rect.z * 0.5,client_rect.w));\n vec2 q = vec2(abs(p.x-client_rect.x-client_rect.z*0.5)-client_rect.z*0.5+r,p.y-client_rect.y-client_rect.w+r);\n float d = length(max(q,vec2(0.0)))+min(max(q.x,q.y),0.0)-r;\n d = max(d,client_rect.y-p.y);\n float client = 1.0-smoothstep(-0.5,0.5,d);\n mask *= client_curve.y < 0.0 ? 1.0-client : client;\n }\n vec4 color = texture2D(tex,v_coords);\n // Grain is added after the retained image is sampled, so the effect's\n // working resolution (downsampled blurs) cannot smear it into blobs.\n // gl_FragCoord keeps every cell inside one output pixel and stationary.\n if (grain.x > 0.0) {\n vec2 cell = mod(floor(gl_FragCoord.xy / grain.y), 256.0);\n vec3 hash = fract(cell.xyx * 0.1031);\n hash += dot(hash, hash.yzx + 33.33);\n float random = fract((hash.x + hash.y) * hash.z);\n float levels = floor(grain.x * 255.0 + 0.5);\n float noise = (floor(random * levels) - max(levels - 1.0, 0.0) * (0.5 - grain.z)) / 255.0;\n color.rgb = clamp(color.rgb + noise, 0.0, 1.0);\n }\n gl_FragColor = color * mask * alpha;\n}\n");
    renderer.compile_custom_texture_shader(&shader, &uniforms)
}
pub fn element(
    image: &TextureBuffer<GlesTexture>,
    program: &GlesTexProgram,
    shapes: &[Shape],
    geometry: Rectangle<i32, Physical>,
    output: Size<i32, Physical>,
    scale: f64,
) -> Masked {
    element_with_client_clip(image, program, shapes, geometry, output, scale, None)
}
pub fn element_with_client_clip(
    image: &TextureBuffer<GlesTexture>,
    program: &GlesTexProgram,
    shapes: &[Shape],
    geometry: Rectangle<i32, Physical>,
    output: Size<i32, Physical>,
    scale: f64,
    client_clip: Option<(super::rounded::ClientClip, bool)>,
) -> Masked {
    element_for_region(
        image,
        program,
        shapes,
        geometry,
        output,
        Rectangle::from_size(output),
        scale,
        client_clip,
        [0., 1., 0.5],
    )
}

#[allow(clippy::too_many_arguments)]
pub fn element_for_region(
    image: &TextureBuffer<GlesTexture>,
    program: &GlesTexProgram,
    shapes: &[Shape],
    geometry: Rectangle<i32, Physical>,
    output: Size<i32, Physical>,
    image_region: Rectangle<i32, Physical>,
    scale: f64,
    client_clip: Option<(super::rounded::ClientClip, bool)>,
    grain: [f32; 3],
) -> Masked {
    let mut uniforms = vec![
        Uniform::new(
            "output_size",
            [image_region.size.w as f32, image_region.size.h as f32],
        ),
        Uniform::new(
            "output_origin",
            [image_region.loc.x as f32, image_region.loc.y as f32],
        ),
        Uniform::new("grain", grain),
    ];
    let (client_rect, client_curve) = client_clip
        .map(|(clip, excluded)| {
            (
                [
                    (clip.bounds.loc.x * scale) as f32,
                    (clip.bounds.loc.y * scale) as f32,
                    (clip.bounds.size.w * scale) as f32,
                    (clip.bounds.size.h * scale) as f32,
                ],
                [
                    (clip.radius * scale) as f32,
                    if excluded { -1. } else { 1. },
                ],
            )
        })
        .unwrap_or(([0.; 4], [0.; 2]));
    uniforms.push(Uniform::new("client_rect", client_rect));
    uniforms.push(Uniform::new("client_curve", client_curve));
    for i in 0..MAX_SHAPES {
        let (rect, curve) = if let Some(s) = shapes.get(i) {
            (
                [
                    geometry.loc.x as f32 + (s.x * scale) as f32,
                    geometry.loc.y as f32 + (s.y * scale) as f32,
                    (s.width * scale) as f32,
                    (s.height * scale) as f32,
                ],
                [(s.radius * scale) as f32, s.opacity as f32],
            )
        } else {
            ([0.; 4], [0.; 2])
        };
        let clip = shapes
            .get(i)
            .and_then(|s| s.clip)
            .map(|c| {
                [
                    geometry.loc.x as f32 + (c.x * scale) as f32,
                    geometry.loc.y as f32 + (c.y * scale) as f32,
                    (c.width * scale) as f32,
                    (c.height * scale) as f32,
                ]
            })
            .unwrap_or([0., 0., output.w as f32, output.h as f32]);
        uniforms.push(Uniform::new(format!("clip{i}"), clip));
        uniforms.push(Uniform::new(format!("rect{i}"), rect));
        uniforms.push(Uniform::new(format!("curve{i}"), curve));
    }
    let inner = TextureRenderElement::from_texture_buffer(
        image_region.loc.to_f64(),
        image,
        Some(1.),
        Some(super::texture_pixel_source(image)),
        Some(image_region.size.to_f64().to_logical(scale).to_i32_round()),
        Kind::Unspecified,
    );
    let shader = TextureShaderElement::new(inner, program.clone(), uniforms);
    let extent = shader.geometry(scale.into()).size;
    let correction = Scale::from((
        image_region.size.w as f64 / extent.w.max(1) as f64,
        image_region.size.h as f64 / extent.h.max(1) as f64,
    ));
    Masked(Rc::new(RescaleRenderElement::from_element(
        shader,
        image_region.loc,
        correction,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::{
        backend::{
            allocator::Fourcc,
            egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay},
            renderer::{damage::OutputDamageTracker, Bind, ExportMem, Offscreen},
        },
        utils::Transform,
    };
    #[test]
    #[ignore = "requires EGL; verifies real GPU rounded coverage"]
    fn gpu_coverage_is_antialiased_and_union_does_not_double_opacity() {
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        let context = EGLContext::new(&display).unwrap();
        let mut renderer = unsafe { GlesRenderer::new(context) }.unwrap();
        let source = TextureBuffer::from_memory(
            &mut renderer,
            &vec![255; 64 * 64 * 4],
            Fourcc::Abgr8888,
            (64, 64),
            false,
            1,
            Transform::Normal,
            None,
        )
        .unwrap();
        let program = program(&mut renderer).unwrap();
        let shape = Shape {
            x: 8.,
            y: 8.,
            width: 40.,
            height: 40.,
            radius: 10.,
            opacity: 0.5,
            clip: None,
        };
        let masked = element(
            &source,
            &program,
            &[shape, shape],
            Rectangle::from_size((64, 64).into()),
            (64, 64).into(),
            1.,
        );
        let mut target: GlesTexture = renderer
            .create_buffer(Fourcc::Abgr8888, (64, 64).into())
            .unwrap();
        let mut fb = renderer.bind(&mut target).unwrap();
        let mut damage = OutputDamageTracker::new((64, 64), 1., Transform::Normal);
        damage
            .render_output(&mut renderer, &mut fb, 0, &[masked], [0., 0., 0., 1.])
            .unwrap();
        let mapping = renderer
            .copy_framebuffer(&fb, Rectangle::from_size((64, 64).into()), Fourcc::Abgr8888)
            .unwrap();
        let actual = renderer.map_texture(&mapping).unwrap();
        let pixel = |x: usize, y: usize| actual[(y * 64 + x) * 4];
        assert!(
            (126..=129).contains(&pixel(25, 25)),
            "Overlap must not increase opacity"
        );
        assert_eq!(pixel(8, 8), 0, "Corner outside the circle");
        assert_eq!(pixel(3, 25), 0);
        assert!(
            (8..18).any(|y| (8..18).any(|x| {
                let v = pixel(x, y);
                v > 0 && v < 125
            })),
            "Edge must contain fractional coverage"
        );
        for (y, clip_y) in [(24.25, 0.), (-4.25, 16.)] {
            let shape = Shape {
                y,
                clip: Some(crate::surface_effects::Clip {
                    x: 0.,
                    y: clip_y,
                    width: 64.,
                    height: 48. - clip_y,
                }),
                ..shape
            };
            let masked = element(
                &source,
                &program,
                &[shape],
                Rectangle::from_size((64, 64).into()),
                (64, 64).into(),
                1.,
            );
            let mut damage = OutputDamageTracker::new((64, 64), 1., Transform::Normal);
            damage
                .render_output(&mut renderer, &mut fb, 0, &[masked], [0., 0., 0., 1.])
                .unwrap();
            let mapping = renderer
                .copy_framebuffer(&fb, Rectangle::from_size((64, 64).into()), Fourcc::Abgr8888)
                .unwrap();
            let actual = renderer.map_texture(&mapping).unwrap();
            let pixel = |x: usize, y: usize| actual[(y * 64 + x) * 4];
            let edge_y = if clip_y == 0. { 47 } else { 16 };
            assert!(
                (126..=129).contains(&pixel(8, edge_y)),
                "Clip boundary must not introduce a rounded corner"
            );
            assert_eq!(
                pixel(25, if clip_y == 0. { 49 } else { 14 }),
                0,
                "Blur must stay inside its viewport"
            );
        }

        let stripes: Vec<_> = (0..64 * 64)
            .flat_map(|index| {
                let value = if index % 2 == 0 { 0 } else { 255 };
                [value, value, value, 255]
            })
            .collect();
        let source = TextureBuffer::from_memory(
            &mut renderer,
            &stripes,
            Fourcc::Abgr8888,
            (64, 64),
            false,
            1,
            Transform::Normal,
            None,
        )
        .unwrap();
        let masked = element(
            &source,
            &program,
            &[Shape {
                x: 0.,
                y: 0.,
                width: 64. / 1.25,
                height: 64. / 1.25,
                radius: 0.,
                opacity: 1.,
                clip: None,
            }],
            Rectangle::from_size((64, 64).into()),
            (64, 64).into(),
            1.25,
        );
        OutputDamageTracker::new((64, 64), 1.25, Transform::Normal)
            .render_output(&mut renderer, &mut fb, 0, &[masked], [0., 0., 0., 1.])
            .unwrap();
        let mapping = renderer
            .copy_framebuffer(&fb, Rectangle::from_size((64, 64).into()), Fourcc::Abgr8888)
            .unwrap();
        let actual = renderer.map_texture(&mapping).unwrap();
        for x in 8..56 {
            let expected = if x % 2 == 0 { 0 } else { 255 };
            assert_eq!(
                actual[(32 * 64 + x) * 4],
                expected,
                "Fractional backdrop must preserve source pixel {x}"
            );
        }
    }
}
