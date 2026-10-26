use crate::{PanelWidgetData, RixPanelWindow, Theme};
use rix_config::{PanelConfig, WidgetSection};
use rix_core::{Alignment, PanelLength};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::rc::Rc;
thread_local! { static PANEL: std::cell::RefCell<PanelConfig> = std::cell::RefCell::new(PanelConfig::default()); }
pub fn apply(window: &RixPanelWindow, panel: &PanelConfig) {
    PANEL.with(|p| *p.borrow_mut() = panel.clone());
    reflow(window);
}
pub fn reflow(window: &RixPanelWindow) {
    let theme = window.global::<Theme>();
    crate::tray::set_icon_size(
        (theme.get_panel_tray_icon_size() * window.window().scale_factor()).ceil() as u32,
    );
    let extent = window.window().size().width as f32 / window.window().scale_factor();
    let margin = window.get_effective_panel_margin() as f32;
    let padding = theme.get_panel_padding();

    PANEL.with(|panel| {
        let panel = panel.borrow();
        let mut groups: [Vec<PanelWidgetData>; 3] = Default::default();
        for widget in &panel.widgets {
            let width = widget
                .properties
                .get("width")
                .and_then(|v| v.as_f64())
                .map(|v| v as f32)
                .unwrap_or_else(|| match widget.kind.as_str() {
                    "launcher" => theme.get_task_width(),
                    "taskbar" => window.get_taskbar_width() as f32,
                    "clock" => theme.get_clock_width(),
                    "tray" => {
                        theme.get_tray_width()
                            * (window.get_tray_items().row_count()
                                + window.get_system_modules().row_count())
                                as f32
                    }
                    "show-desktop" => theme.get_show_desktop_width(),
                    "spacer" => 0.,
                    _ => 0.,
                });
            if width <= 0. {
                continue;
            }
            let group = match widget.section {
                WidgetSection::Start => 0,
                WidgetSection::Center => 1,
                WidgetSection::End => 2,
            };
            groups[group].push(PanelWidgetData {
                kind: widget.kind.as_str().into(),
                id: widget.id.as_str().into(),
                x: 0.,
                width,
                edge_hit_target: false,
                hit_right_extension: 0.,
            });
        }
        let gap = theme.get_widget_spacing();
        let natural = groups.iter().flatten().map(|w| w.width).sum::<f32>()
            + groups.iter().map(Vec::len).sum::<usize>().saturating_sub(1) as f32 * gap
            + padding * 2.;
        let (offset, width) =
            panel_geometry(extent, margin, natural, panel.length, panel.alignment);
        window.set_panel_offset(offset);
        window.set_panel_content_width(width);
        let available = (width - padding * 2.).max(0.);
        fit_widgets(&mut groups, available, gap);
        let widths = groups.each_ref().map(|items| {
            items.iter().map(|w| w.width).sum::<f32>() + items.len().saturating_sub(1) as f32 * gap
        });
        let starts = section_positions(available, widths, gap);
        let mut items = Vec::new();
        for (group, start) in groups.into_iter().zip(starts) {
            let mut x = padding + start;
            for mut item in group {
                let scale = window.window().scale_factor();
                item.x = (x * scale).round() / scale;
                item.width = (item.width * scale).round() / scale;
                if item.kind == "launcher" {
                    window.set_launcher_x(item.x + offset);
                    window.set_launcher_width(item.width);
                }
                x += item.width + gap;
                items.push(item);
            }
        }
        extend_rightmost_show_desktop(&mut items, padding);
        let current = window.get_widgets();
        if window.get_taskbar_drag_active()
            && (current.row_count() != items.len()
                || current
                    .iter()
                    .zip(&items)
                    .any(|(old, new)| old.id != new.id || old.kind != new.kind))
        {

            window.invoke_cancel_taskbar_drag();
        }
        if let Some(model) = current.as_any().downcast_ref::<VecModel<PanelWidgetData>>() {

            while model.row_count() > items.len() {
                model.remove(model.row_count() - 1);
            }
            for (index, item) in items.into_iter().enumerate() {
                if index < model.row_count() {
                    model.set_row_data(index, item);
                } else {
                    model.push(item);
                }
            }
        } else {
            window.set_widgets(ModelRc::from(Rc::new(VecModel::from(items))));
        }
    });
}

fn extend_rightmost_show_desktop(items: &mut [PanelWidgetData], trailing_padding: f32) {
    for widget in items.iter_mut() {
        widget.edge_hit_target = false;
        widget.hit_right_extension = 0.;
    }
    if let Some(widget) = items
        .iter_mut()
        .max_by(|left, right| (left.x + left.width).total_cmp(&(right.x + right.width)))
    {
        if widget.kind == "show-desktop" {
            widget.edge_hit_target = true;
            widget.hit_right_extension = trailing_padding.max(0.);
        }
    }
}

fn panel_geometry(
    extent: f32,
    margin: f32,
    content: f32,
    length: PanelLength,
    alignment: Alignment,
) -> (f32, f32) {
    let available = (extent - margin * 2.).max(0.);
    let width = match length {
        PanelLength::Fill => available,
        PanelLength::FitContent => content.min(available),
        PanelLength::Fixed(width) => (width as f32).min(available),
    }
    .max(0.);
    let factor = match alignment {
        Alignment::Start => 0.,
        Alignment::End => 1.,
        _ => 0.5,
    };
    (margin + (available - width) * factor, width)
}

fn fit_widgets(groups: &mut [Vec<PanelWidgetData>; 3], extent: f32, gap: f32) {
    let count = groups.iter().map(Vec::len).sum::<usize>();
    let room = (extent - count.saturating_sub(1) as f32 * gap).max(0.);
    let total = groups.iter().flatten().map(|w| w.width).sum::<f32>();
    let flexible = groups
        .iter()
        .flatten()
        .filter(|w| w.kind == "taskbar")
        .map(|w| w.width)
        .sum::<f32>();
    let shrink = (total - room).max(0.).min(flexible);
    if flexible > 0. {
        for widget in groups.iter_mut().flatten().filter(|w| w.kind == "taskbar") {
            widget.width *= 1. - shrink / flexible;
        }
    }
    let remaining = total - shrink;
    if remaining > room && remaining > 0. {
        for widget in groups.iter_mut().flatten() {
            widget.width *= room / remaining;
        }
    }
}
fn section_positions(extent: f32, widths: [f32; 3], gap: f32) -> [f32; 3] {
    let right = (extent - widths[2]).max(0.);
    let left_end = if widths[0] > 0. { widths[0] + gap } else { 0. };
    let center_max = (right - if widths[2] > 0. { gap } else { 0. } - widths[1]).max(left_end);
    [
        0.,
        ((extent - widths[1]) / 2.).clamp(left_end, center_max),
        right,
    ]
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn geometry_updates_keep_the_existing_panel_delegate_model() {
        i_slint_backend_testing::init_no_event_loop();
        let window = RixPanelWindow::new().unwrap();
        window.show().unwrap();
        let mut panel = PanelConfig::default();
        for widget in &mut panel.widgets {
            widget.section = WidgetSection::Start;
        }
        apply(&window, &panel);
        let original = window.get_widgets();
        let model = original
            .as_any()
            .downcast_ref::<VecModel<PanelWidgetData>>()
            .unwrap();
        assert!(model.row_count() > 0);
        for padding in [4., 12., 8.] {
            window.global::<Theme>().set_panel_padding(padding);
            reflow(&window);
            let updated = window.get_widgets();
            assert!(std::ptr::eq(
                model,
                updated
                    .as_any()
                    .downcast_ref::<VecModel<PanelWidgetData>>()
                    .unwrap()
            ));
            assert_eq!(model.row_data(0).unwrap().x, padding);
        }
    }

    #[test]
    fn window_states_preserve_configured_panel_geometry_and_work_area() {
        i_slint_backend_testing::init_no_event_loop();
        let window = RixPanelWindow::new().unwrap();
        window.set_panel_width(1280);
        window.set_available_height(720);
        let theme = window.global::<Theme>();
        theme.set_task_width(40.);
        theme.set_task_spacing(4.);
        theme.set_panel_padding(8.);
        let weak = window.as_weak();
        window.on_reflow_panel(move || {
            if let Some(window) = weak.upgrade() {
                reflow(&window);
            }
        });
        let reservations = Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorded = reservations.clone();
        window.on_surface_exclusive_zone_changed(move |extent| recorded.borrow_mut().push(extent));
        window.show().unwrap();
        for at_top in [false, true] {
            for length in [
                PanelLength::Fill,
                PanelLength::FitContent,
                PanelLength::Fixed(600),
            ] {
                let panel = PanelConfig {
                    thickness: 40,
                    floating_margin: 12,
                    length,
                    ..Default::default()
                };
                window.set_panel_at_top(at_top);
                window.set_thickness(panel.thickness as i32);
                window.set_floating(panel.floating);
                window.set_floating_margin(panel.floating_margin as i32);
                apply(&window, &panel);
                let mut app = rix_ipc::WindowInfo {
                    id: 1,
                    title: "Expanded window".into(),
                    app_id: String::new(),
                    outputs: vec![],
                    foreground_outputs: None,
                    is_active: true,
                    is_minimized: true,
                    is_maximized: false,
                    is_fullscreen: false,
                };
                crate::tasks::snapshot(&window, vec![app.clone()], &[], true);
                slint::platform::update_timers_and_animations();
                let ordinary = (window.get_panel_offset(), window.get_panel_content_width());
                let ordinary_region = window.get_surface_regions().row_data(0).unwrap();
                assert!(ordinary.0 >= 12.);
                assert_eq!(window.get_panel_reserved_extent(), 52);
                app.is_maximized = true;
                app.is_minimized = false;
                crate::tasks::snapshot(&window, vec![app.clone()], &[], true);
                slint::platform::update_timers_and_animations();
                assert_eq!(
                    (window.get_panel_offset(), window.get_panel_content_width()),
                    ordinary
                );
                assert!(!window.get_effective_panel_floating());
                assert_eq!(window.get_panel_reserved_extent(), 52);
                let panel_region = window.get_surface_regions().row_data(0).unwrap();
                assert_eq!(
                    (
                        panel_region.x,
                        panel_region.width,
                        panel_region.height,
                    ),
                    (
                        ordinary_region.x,
                        ordinary_region.width,
                        ordinary_region.height,
                    )
                );
                assert_eq!(panel_region.radius, 0.);
                assert_eq!(panel_region.y, if at_top { 0. } else { 680. });

                apply(&window, &panel);
                assert_eq!(
                    (window.get_panel_offset(), window.get_panel_content_width()),
                    ordinary
                );
                app.is_maximized = false;
                app.is_minimized = true;
                crate::tasks::snapshot(&window, vec![app.clone()], &[], true);
                slint::platform::update_timers_and_animations();
                let restored = (window.get_panel_offset(), window.get_panel_content_width());
                assert_eq!(restored, ordinary);
                assert!(window.get_effective_panel_floating());
                assert_eq!(window.get_panel_reserved_extent(), 52);
                let panel_region = window.get_surface_regions().row_data(0).unwrap();
                assert_eq!(panel_region.y, if at_top { 12. } else { 668. });
                app.is_fullscreen = true;
                app.is_minimized = false;
                crate::tasks::snapshot(&window, vec![app.clone()], &[], true);
                slint::platform::update_timers_and_animations();
                assert_eq!(window.get_surface_regions().row_data(0).unwrap().height, 0.);
                window.set_is_app_menu_open(true);
                slint::platform::update_timers_and_animations();
                assert_eq!(
                    window.get_surface_regions().row_data(0).unwrap().y,
                    if at_top { 0. } else { 680. }
                );
                window.set_is_app_menu_open(false);
                app.is_minimized = true;
                crate::tasks::snapshot(&window, vec![app], &[], true);
                slint::platform::update_timers_and_animations();
                assert_eq!(window.get_panel_reserved_extent(), 52);
                assert_eq!(
                    (window.get_panel_offset(), window.get_panel_content_width()),
                    ordinary
                );
            }
        }
        window.set_panel_reserve_space(false);
        assert_eq!(window.get_panel_reserved_extent(), 0);
        window.set_maximized_window(true);
        assert_eq!(window.get_panel_reserved_extent(), 0);
        slint::platform::update_timers_and_animations();
        assert!(!reservations.borrow().contains(&40));
        assert!(reservations.borrow().contains(&52));
    }
    #[test]
    fn length_and_alignment_obey_output_and_margins() {
        assert_eq!(
            panel_geometry(1280., 4., 330., PanelLength::Fill, Alignment::Center),
            (4., 1272.)
        );
        assert_eq!(
            panel_geometry(1280., 4., 330., PanelLength::FitContent, Alignment::Center),
            (475., 330.)
        );
        assert_eq!(
            panel_geometry(1280., 4., 330., PanelLength::Fixed(800), Alignment::End),
            (476., 800.)
        );
        assert_eq!(
            panel_geometry(640., 4., 1200., PanelLength::Fixed(800), Alignment::Start),
            (4., 632.)
        );
    }
    #[test]
    fn asymmetric_sections_keep_centered_launcher_until_collision() {
        assert_eq!(
            section_positions(1000., [50., 120., 200.], 4.),
            [0., 440., 800.]
        );
        assert_eq!(
            section_positions(500., [250., 120., 80.], 4.),
            [0., 254., 420.]
        );
    }

    #[test]
    fn only_rightmost_show_desktop_extends_to_the_panel_edge() {
        let mut widgets = vec![
            PanelWidgetData {
                kind: "clock".into(),
                id: "clock".into(),
                x: 100.,
                width: 80.,
                edge_hit_target: false,
                hit_right_extension: 0.,
            },
            PanelWidgetData {
                kind: "show-desktop".into(),
                id: "show-desktop".into(),
                x: 184.,
                width: 18.,
                edge_hit_target: false,
                hit_right_extension: 0.,
            },
        ];
        extend_rightmost_show_desktop(&mut widgets, 8.);
        assert!(widgets[1].edge_hit_target);
        assert_eq!(widgets[1].hit_right_extension, 8.);

        widgets.push(PanelWidgetData {
            kind: "clock".into(),
            id: "later-clock".into(),
            x: 206.,
            width: 40.,
            edge_hit_target: false,
            hit_right_extension: 0.,
        });
        extend_rightmost_show_desktop(&mut widgets, 8.);
        assert!(!widgets[1].edge_hit_target);
        assert_eq!(widgets[1].hit_right_extension, 0.);
    }
}
