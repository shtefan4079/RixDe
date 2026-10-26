use crate::state::RixState;
use smithay::wayland::{
    compositor::{send_surface_state, with_states, with_surface_tree_downward, TraversalAction},
    fractional_scale::{with_fractional_scale, FractionalScaleHandler},
    selection::primary_selection::{PrimarySelectionHandler, PrimarySelectionState},
};
use wayland_server::protocol::wl_surface::WlSurface;

impl FractionalScaleHandler for RixState {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let scale = self.scale_for_surface(&surface);
        with_states(&surface, |states| {
            with_fractional_scale(states, |s| s.set_preferred_scale(scale))
        });
    }
}

impl PrimarySelectionHandler for RixState {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection
    }
}

impl RixState {
    fn scale_for_surface(&self, surface: &WlSurface) -> f64 {
        let mut root = surface.clone();
        for _ in 0..64 {
            let Some(parent) = crate::capture_policy::surface_parent(&root) else {
                break;
            };
            root = parent;
        }
        if let Some((_, window)) = self
            .windows
            .entries
            .iter()
            .find(|(_, w)| w.toplevel().is_some_and(|t| t.wl_surface() == &root))
        {
            if let Some(output) = self.space.outputs_for_element(window).first() {
                return output.current_scale().fractional_scale();
            }
        }
        if let Some(output) = self.initial_surface_output(&root) {
            return output.current_scale().fractional_scale();
        }
        for output in self.space.outputs() {
            if smithay::desktop::layer_map_for_output(output)
                .layers()
                .any(|layer| layer.wl_surface() == &root)
            {
                return output.current_scale().fractional_scale();
            }
        }
        self.space
            .outputs()
            .next()
            .map_or(1., |o| o.current_scale().fractional_scale())
    }

    pub(crate) fn prepare_initial_surface_scale(&self, surface: &WlSurface) {
        let Some(output) = self.initial_surface_output(surface) else {
            return;
        };
        let scale = output.current_scale();
        with_surface_tree_downward(
            surface,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |surface, states, _| {
                send_surface_state(
                    surface,
                    states,
                    scale.integer_scale(),
                    output.current_transform(),
                );
                with_fractional_scale(states, |s| s.set_preferred_scale(scale.fractional_scale()));
            },
            |_, _, _| true,
        );
    }

    pub(crate) fn refresh_fractional_scales(&self) {
        for root in self
            .windows
            .entries
            .iter()
            .filter_map(|(_, w)| w.toplevel().map(|t| t.wl_surface()))
            .chain(self.layer_surfaces.iter().map(|l| l.wl_surface()))
            .chain(
                self.lock
                    .surfaces
                    .iter()
                    .filter(|(_, l)| l.alive())
                    .map(|(_, l)| l.wl_surface()),
            )
        {
            let scale = self.scale_for_surface(root);
            with_surface_tree_downward(
                root,
                (),
                |_, _, _| TraversalAction::DoChildren(()),
                |_, states, _| with_fractional_scale(states, |s| s.set_preferred_scale(scale)),
                |_, _, _| true,
            );
            for (popup, _) in smithay::desktop::PopupManager::popups_for_surface(root) {
                with_states(popup.wl_surface(), |states| {
                    with_fractional_scale(states, |s| s.set_preferred_scale(scale))
                });
            }
        }
    }
}

smithay::delegate_viewporter!(RixState);
smithay::delegate_fractional_scale!(RixState);
smithay::delegate_primary_selection!(RixState);
