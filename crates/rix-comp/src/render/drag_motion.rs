use crate::{drag_drop::DragState, state::RixState};
use smithay::{
    backend::renderer::{
        element::{
            surface::{WaylandSurfaceRenderElement, WaylandSurfaceTexture},
            texture::TextureRenderElement,
            utils::RescaleRenderElement,
            Element, Id, Kind,
        },
        gles::{GlesRenderer, GlesTexture},
        Renderer, Texture,
    },
    output::Output,
    utils::{Buffer, Physical, Point, Rectangle, Transform},
};
use std::collections::HashMap;

const MAX_TEXTURE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SURFACES: usize = 32;

pub(super) type LiveIcon = RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>;
pub(super) enum IconElement {
    Live(Box<LiveIcon>),
    Retained(Box<super::motion::AnimatedTexture>),
}

struct Part {
    id: Id,
    texture: GlesTexture,
    source: Rectangle<f64, Buffer>,
    geometry: Rectangle<i32, Physical>,
    transform: Transform,
    alpha: f32,
}
struct Snapshot {
    icon: Id,
    anchor: Point<i32, Physical>,
    scale: f64,
    bytes: u64,
    parts: Vec<Part>,
}
#[derive(Default)]
pub(super) struct DragMotion {
    outputs: HashMap<String, Snapshot>,
}

fn pickup(elapsed: f32, duration: u32) -> (f32, f64) {
    if duration == 0 {
        return (1., 1.);
    }
    let t = (elapsed / duration.min(160) as f32).clamp(0., 1.);
    let eased = 1. - (1. - t).powi(3);

    let back = 1. + 2.15 * (t - 1.).powi(3) + 1.15 * (t - 1.).powi(2);
    (0.84 + 0.16 * eased, 0.96 + 0.04 * back.max(0.) as f64)
}
fn release(elapsed: f32, duration: u32, accepted: bool) -> Option<(f32, f64)> {
    if duration == 0 {
        return None;
    }
    let duration = duration.min(if accepted { 140 } else { 95 }) as f32;
    let t = (elapsed / duration).clamp(0., 1.);
    if t >= 1. {
        return None;
    }
    let eased = t * t * (3. - 2. * t);
    Some((
        1. - eased,
        1. - if accepted { 0.09 } else { 0.03 } * eased as f64,
    ))
}

impl DragMotion {
    pub(super) fn prune(&mut self, state: &RixState) {
        if state.lock.active || state.greeter_mode || state.animation_duration == 0 {
            self.outputs.clear();
            return;
        }
        self.outputs.retain(|output, snapshot| {
            state.space.outputs().any(|known| known.name() == *output)
                && (state.drag.icon.as_ref().is_some_and(|icon| {
                    state.drag.active && Id::from_wayland_resource(icon) == snapshot.icon
                }) || state.drag.finished.as_ref().is_some_and(|finish| {
                    finish.icon == snapshot.icon
                        && release(
                            finish.ended.elapsed().as_secs_f32() * 1000.,
                            state.animation_duration,
                            finish.accepted,
                        )
                        .is_some()
                }))
        });
    }

    pub(super) fn finishing(drag: &DragState, duration: u32) -> bool {
        drag.finished.as_ref().is_some_and(|finish| {
            release(
                finish.ended.elapsed().as_secs_f32() * 1000.,
                duration,
                finish.accepted,
            )
            .is_some()
        })
    }

    pub(super) fn frame(
        &mut self,
        renderer: &mut GlesRenderer,
        state: &RixState,
        output: &Output,
    ) -> Vec<IconElement> {
        self.prune(state);
        let scale = output.current_scale().fractional_scale();
        if !state.drag.active {
            let Some(finish) = &state.drag.finished else {
                return Vec::new();
            };
            let Some((alpha, zoom)) = release(
                finish.ended.elapsed().as_secs_f32() * 1000.,
                state.animation_duration,
                finish.accepted,
            ) else {
                return Vec::new();
            };
            let Some(snapshot) = self
                .outputs
                .get(&output.name())
                .filter(|snapshot| snapshot.icon == finish.icon && snapshot.scale == scale)
            else {
                return Vec::new();
            };
            return snapshot
                .parts
                .iter()
                .map(|part| {
                    let texture = TextureRenderElement::from_static_texture(
                        part.id.clone(),
                        renderer.context_id(),
                        part.geometry.loc.to_f64(),
                        part.texture.clone(),
                        1,
                        part.transform,
                        Some(part.alpha * alpha),
                        Some(part.source.to_logical(
                            1.,
                            part.transform,
                            &part.texture.size().to_f64(),
                        )),
                        Some(part.geometry.size.to_f64().to_logical(scale).to_i32_round()),
                        None,
                        Kind::Unspecified,
                    );
                    IconElement::Retained(Box::new(RescaleRenderElement::from_element(
                        super::motion::PixelTexture::new(
                            texture,
                            part.geometry,
                            part.source,
                            Default::default(),
                        ),
                        snapshot.anchor,
                        zoom,
                    )))
                })
                .collect();
        }
        let Some(bounds) = state.space.output_geometry(output) else {
            return Vec::new();
        };
        let Some(pointer) = state.drag.touch_location.or_else(|| {
            state
                .seat
                .get_pointer()
                .map(|pointer| pointer.current_location())
        }) else {
            return Vec::new();
        };
        let anchor = (pointer - bounds.loc.to_f64()).to_physical_precise_round(scale);
        let (alpha, zoom) = pickup(
            state
                .drag
                .started
                .map_or(1000., |started| started.elapsed().as_secs_f32() * 1000.),
            state.animation_duration,
        );
        let raw = crate::drag_drop::icon_elements_with_alpha(renderer, state, output, false, alpha);
        if let Some(icon) = &state.drag.icon {
            let mut snapshot = Snapshot {
                icon: Id::from_wayland_resource(icon),
                anchor,
                scale,
                bytes: 0,
                parts: Vec::new(),
            };
            let local = Rectangle::from_size(bounds.size.to_physical_precise_round(scale));
            for part in &raw {
                if !part.geometry(scale.into()).overlaps(local) {
                    continue;
                }
                let WaylandSurfaceTexture::Texture(texture) = part.texture() else {
                    continue;
                };
                let size = texture.size();
                snapshot.bytes = snapshot
                    .bytes
                    .saturating_add(size.w.max(0) as u64 * size.h.max(0) as u64 * 4);
                if snapshot.bytes > MAX_TEXTURE_BYTES || snapshot.parts.len() == MAX_SURFACES {
                    snapshot.parts.clear();
                    break;
                }
                snapshot.parts.push(Part {
                    id: part.id().clone(),
                    texture: texture.clone(),
                    source: part.src(),
                    geometry: part.geometry(scale.into()),
                    transform: part.transform(),
                    alpha: part.alpha() / alpha,
                });
            }
            let other_bytes: u64 = self
                .outputs
                .iter()
                .filter(|(name, _)| **name != output.name())
                .map(|(_, saved)| saved.bytes)
                .sum();
            if !snapshot.parts.is_empty()
                && snapshot.bytes.saturating_add(other_bytes) <= MAX_TEXTURE_BYTES
                && state.animation_duration > 0
            {
                self.outputs.insert(output.name(), snapshot);
            } else {
                self.outputs.remove(&output.name());
            }
        }
        raw.into_iter()
            .map(|part| {
                IconElement::Live(Box::new(RescaleRenderElement::from_element(
                    part, anchor, zoom,
                )))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restrained_animation_settles_and_reduced_motion_is_immediate() {
        assert_eq!(pickup(0., 0), (1., 1.));
        assert!(release(0., 0, true).is_none());
        assert_eq!(pickup(200., 160), (1., 1.));
        for step in 0..=160 {
            let (alpha, scale) = pickup(step as f32, 160);
            assert!((0.84..=1.).contains(&alpha));
            assert!((0.96..=1.01).contains(&scale));
        }
        let first = release(0., 140, true).unwrap();
        let middle = release(70., 140, true).unwrap();
        assert_eq!(first, (1., 1.));
        assert!(middle.0 < first.0 && middle.1 < first.1);
        assert!(release(140., 140, true).is_none());
        assert!(release(95., 140, false).is_none());
    }
}
