use crate::{Palette, Theme};
use rix_config::ThemeSettings;
fn color(value: rix_core::Color, alpha: f32) -> slint::Color {
    slint::Color::from_argb_f32(value.a * alpha, value.r, value.g, value.b)
}
#[allow(dead_code)]
pub fn apply(theme: Theme<'_>, controls: Palette<'_>, settings: &ThemeSettings) {
    apply_with_frames(theme, controls, settings, true);
}

#[allow(dead_code)]
pub fn apply_for_app(theme: Theme<'_>, controls: Palette<'_>, settings: &ThemeSettings) {
    apply_with_frames(theme, controls, settings, false);
}

fn apply_with_frames(
    theme: Theme<'_>,
    controls: Palette<'_>,
    settings: &ThemeSettings,
    taskbar_frames: bool,
) {
    let a = &settings.appearance;
    let background = a.color("window_background");
    controls.set_color_scheme(
        if background.r * 0.2126 + background.g * 0.7152 + background.b * 0.0722 < 0.5 {
            slint::language::ColorScheme::Dark
        } else {
            slint::language::ColorScheme::Light
        },
    );
    macro_rules! colors {
        ($($setter:ident => $name:literal, $alpha:expr;)*) => {$(
            theme.$setter(color(a.color($name), $alpha));
        )*};
    }
    macro_rules! metrics {
        ($($setter:ident => $name:literal;)*) => {$(
            theme.$setter(a.metric($name));
        )*};
    }
    colors! {
        set_control_surface => "control_surface", 1.;
        set_control_border => "control_border", 1.;
        set_window_background => "window_background", 1.;
        set_sidebar_background => "sidebar_background", 1.;
        set_background => "background", settings.opacity;
        set_surface => "surface", 1.;
        set_surface_hover => "surface_hover", 1.;
        set_surface_active => "surface_active", 1.;
        set_menu_bg => "menu_bg", settings.opacity;
        set_glass_start => "glass_start", settings.opacity;
        set_glass_end => "glass_end", settings.opacity;
        set_accent => "accent", 1.;
        set_accent_hover => "accent_hover", 1.;
        set_accent_glow => "accent_glow", 1.;
        set_accent_gradient_start => "accent_gradient_start", 1.;
        set_accent_gradient_end => "accent_gradient_end", 1.;
        set_text_primary => "text_primary", 1.;
        set_text_secondary => "text_secondary", 1.;
        set_text_muted => "text_muted", 1.;
        set_success => "success", 1.;
        set_danger => "danger", 1.;
        set_danger_bg => "danger_bg", 1.;
        set_border => "border", 1.;
        set_border_highlight => "border_highlight", 1.;
        set_edit_outline => "edit_outline", 1.;
        set_divider => "divider", 1.;
        set_overlay_hover => "overlay_hover", 1.;
        set_overlay_idle => "overlay_idle", 1.;
        set_value_bg => "value_bg", 1.;
        set_knob => "knob", 1.;
        set_scrollbar => "scrollbar", 1.;
        set_scrollbar_hover => "scrollbar_hover", 1.;
        set_task_active_background => "task_active_background", 1.;
        set_task_hover_background => "task_hover_background", 1.;
        set_shadow => "shadow", 1.;
    }
    metrics! {
        set_preview_width => "preview_width";
        set_preview_height => "preview_height";
        set_preview_header_height => "preview_header_height";
        set_preview_delay => "preview_delay";
        set_preview_hide_delay => "preview_hide_delay";
        set_preview_refresh => "preview_refresh";
        set_system_popup_width => "system_popup_width";
        set_system_popup_height => "system_popup_height";
        set_calendar_cell_height => "calendar_cell_height";
        set_audio_row_height => "audio_row_height";
        set_default_radius => "default_radius";
        set_radius_sm => "radius_sm";
        set_radius_md => "radius_md";
        set_radius_lg => "radius_lg";
        set_radius_pill => "radius_pill";
        set_default_spacing => "default_spacing";
        set_spacing_xs => "spacing_xs";
        set_spacing_lg => "spacing_lg";
        set_spacing_xl => "spacing_xl";
        set_icon_size => "icon_size";
        set_icon_size_sm => "icon_size_sm";
        set_icon_size_lg => "icon_size_lg";
        set_font_size_sm => "font_size_sm";
        set_font_size_md => "font_size_md";
        set_font_size_lg => "font_size_lg";
        set_font_size_xl => "font_size_xl";
        set_menu_padding => "menu_padding";
        set_menu_sidebar_width => "menu_sidebar_width";
        set_menu_column_gap => "menu_column_gap";
        set_menu_header_height => "menu_header_height";
        set_menu_footer_height => "menu_footer_height";
        set_menu_row_height => "menu_row_height";
        set_app_row_height => "app_row_height";
        set_favorite_cell_width => "favorite_cell_width";
        set_favorite_cell_height => "favorite_cell_height";
        set_favorite_icon_size => "favorite_icon_size";
        set_panel_vertical_padding => "panel_vertical_padding";
        set_panel_icon_size => "panel_icon_size";
        set_panel_launcher_icon_size => "panel_launcher_icon_size";
        set_panel_tray_icon_size => "panel_tray_icon_size";
        set_task_padding => "task_padding";
        set_task_spacing => "task_spacing";
        set_task_indicator_height => "task_indicator_height";
        set_task_indicator_active_width => "task_indicator_active_width";
        set_task_indicator_width => "task_indicator_width";
        set_task_pin_indicator_size => "task_pin_indicator_size";
        set_clock_padding => "clock_padding";
        set_panel_padding => "panel_padding";
        set_widget_spacing => "widget_spacing";
        set_task_width => "task_width";
        set_task_label_width => "task_label_width";
        set_task_highlight_scale => "task_highlight_scale";
        set_task_highlight_radius_scale => "task_highlight_radius_scale";
        set_clock_width => "clock_width";
        set_tray_width => "tray_width";
        set_context_menu_radius => "context_menu_radius";
        set_context_menu_padding => "context_menu_padding";
        set_app_icon_size => "app_icon_size";
        set_context_menu_width => "context_menu_width";
        set_context_menu_max_height => "context_menu_max_height";
        set_show_desktop_width => "show_desktop_width";
        set_hover_duration => "hover_duration";
        set_popup_duration => "popup_duration";
        set_popup_distance => "popup_distance";
        set_scrollbar_gutter => "scrollbar_gutter";
        set_scrollbar_width => "scrollbar_width";
        set_scrollbar_hover_width => "scrollbar_hover_width";
        set_scrollbar_min_thumb => "scrollbar_min_thumb";
        set_menu_action_size => "menu_action_size";
        set_editor_width => "editor_width";
        set_editor_height => "editor_height";
        set_menu_gap => "menu_gap";
    }
    let scale = a.metric("frame_raster_scale").clamp(1., 4.) as u32;
    if taskbar_frames {
        theme.set_task_active_frame(crate::skins::load(&a.task_active_frame, scale));
        theme.set_task_hover_frame(crate::skins::load(&a.task_hover_frame, scale));
    } else {
        theme.set_task_active_frame(Default::default());
        theme.set_task_hover_frame(Default::default());
    }
    theme.set_panel_frame(crate::skins::load(&a.panel_frame, scale));
    theme.set_menu_frame(crate::skins::load(&a.menu_frame, scale));
    theme.set_material_opacity(settings.opacity);
    let accent =
        rix_core::Color::from_hex(&settings.accent_color).unwrap_or_else(|| a.color("accent"));
    theme.set_accent(color(accent, 1.));
    theme.set_edit_outline(color(accent, 1.));
    theme.set_font_family(a.font_family.clone().into());
    theme.set_is_aero(settings.blur_radius > 0);
}
