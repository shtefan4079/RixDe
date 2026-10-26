use smithay::{
    backend::renderer::utils::with_renderer_surface_state,
    desktop::LayerSurface,
    utils::{Logical, Point, Size},
    wayland::{
        compositor::with_states,
        shell::wlr_layer::{Anchor, LayerSurfaceCachedState},
    },
};

pub fn offset(layer: &LayerSurface) -> Point<i32, Logical> {
    let surface = layer.wl_surface();
    let Some(actual) = with_renderer_surface_state(surface, |state| state.buffer_size()).flatten()
    else {
        return (0, 0).into();
    };
    let Some(configured) = layer.layer_surface().with_pending_state(|state| state.size) else {
        return (0, 0).into();
    };
    let anchor = with_states(surface, |states| {
        states
            .cached_state
            .get::<LayerSurfaceCachedState>()
            .current()
            .anchor
    });
    anchor_offset(configured, actual, anchor)
}

fn anchor_offset(
    configured: Size<i32, Logical>,
    actual: Size<i32, Logical>,
    anchor: Anchor,
) -> Point<i32, Logical> {
    let axis = |delta, start, end| match (start, end) {
        (false, true) => delta,
        (false, false) => delta / 2,
        _ => 0,
    };
    (
        axis(
            configured.w - actual.w,
            anchor.contains(Anchor::LEFT),
            anchor.contains(Anchor::RIGHT),
        ),
        axis(
            configured.h - actual.h,
            anchor.contains(Anchor::TOP),
            anchor.contains(Anchor::BOTTOM),
        ),
    )
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expanding_bottom_layer_keeps_old_buffer_at_bottom_until_commit() {
        let anchors = Anchor::LEFT | Anchor::RIGHT | Anchor::BOTTOM;
        let configured = (1912, 629).into();
        assert_eq!(
            anchor_offset(configured, (1912, 46).into(), anchors),
            (0, 583).into()
        );
        assert_eq!(
            anchor_offset(configured, configured, anchors),
            (0, 0).into()
        );
        assert_eq!(
            anchor_offset((1912, 46).into(), configured, anchors),
            (0, -583).into()
        );
    }
}
