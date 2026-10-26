use super::*;
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum StartupScaleEvent {
    Preferred(u32),
    Configured,
}

wayland_client::delegate_noop!(Events: ignore wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);
impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, ()> for Events {
    fn event(
        state: &mut Self,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state
                .startup_scale_events
                .push(StartupScaleEvent::Preferred(scale));
        }
    }
}

#[test]
fn attached_dialog_gets_destination_monitor_scale_before_its_first_configure_and_buffer() {
    use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
    for scale in [Scale::Fractional(1.5), Scale::Integer(2)] {
        let mut desktop = TestDesktop::new();
        let parent = desktop.window("Parent on the second monitor");
        desktop.inspect(move |state| {
            let output = Output::new(
                "second-hidpi".into(),
                PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: Subpixel::Unknown,
                    make: "fixture".into(),
                    model: "fixture".into(),
                },
            );
            output.change_current_state(
                Some(Mode {
                    size: (1800, 1200).into(),
                    refresh: 60_000,
                }),
                None,
                Some(scale),
                Some((1000, 0).into()),
            );
            state.space.map_output(&output, (1000, 0));
            let parent = state.windows.entries[0].1.clone();
            state.space.map_element(parent, (1200, 200), false);
            state.space.refresh();
        });
        let qh = desktop.queue.handle();
        let manager: wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1 =
            desktop.globals.bind(&qh, 1..=1, ()).unwrap();
        let surface = desktop.compositor.create_surface(&qh, ());

        let _fractional = manager.get_fractional_scale(&surface, &qh, ());
        desktop.roundtrip();
        assert_eq!(
            desktop.events.startup_scale_events.last(),
            Some(&StartupScaleEvent::Preferred(120))
        );
        desktop.events.startup_scale_events.clear();
        let xdg = desktop.wm.get_xdg_surface(&surface, &qh, ());
        let top = xdg.get_toplevel(&qh, ());
        top.set_title("Attached dialog first frame".into());
        top.set_parent(Some(&parent.2));
        surface.commit();
        desktop.roundtrip();
        let expected = (scale.fractional_scale() * 120.).round() as u32;
        assert_eq!(
            desktop.events.startup_scale_events,
            vec![
                StartupScaleEvent::Preferred(expected),
                StartupScaleEvent::Configured,
            ],
            "The destination scale must precede the configure that permits painting"
        );
        assert_eq!(desktop.inspect(|state| state.windows.entries.len()), 1);

        let physical = (240. * expected as f64 / 120.).round() as i32;
        let buffer = desktop.sized_buffer(physical, physical);
        let viewport = desktop.viewport(&surface);
        viewport.set_destination(240, 240);
        xdg.set_window_geometry(0, 0, 240, 240);
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        desktop.roundtrip();
        desktop.inspect(move |state| {
            let child = &state.windows.entries[1].1;
            assert_eq!(child.geometry().size, (240, 240).into());
            assert_eq!(
                state.space.element_location(child),
                Some((1088, 88).into()),
                "The first mapped buffer must stay centered on its parent's monitor"
            );
            assert_eq!(
                state
                    .space
                    .outputs_for_element(child)
                    .iter()
                    .map(Output::name)
                    .collect::<Vec<_>>(),
                vec!["second-hidpi".to_owned()],
                "First-buffer reflow must use the destination monitor's work area"
            );
            let preferred = smithay::wayland::compositor::with_states(
                child.toplevel().unwrap().wl_surface(),
                |states| {
                    smithay::wayland::fractional_scale::with_fractional_scale(states, |s| {
                        s.preferred_scale()
                    })
                },
            );
            assert_eq!(preferred, Some(scale.fractional_scale()));
            state.refresh_fractional_scales();
        });
        desktop.roundtrip();
        assert_eq!(
            desktop
                .events
                .startup_scale_events
                .iter()
                .filter(|event| matches!(event, StartupScaleEvent::Preferred(_)))
                .collect::<Vec<_>>(),
            vec![&StartupScaleEvent::Preferred(expected)],
            "The first mapped frame must not need another scale correction"
        );
    }
}
