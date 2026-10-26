use crate::{BehaviorSettings, SettingsWindow, SystemState};
use slint::ComponentHandle;
use std::sync::Arc;
#[cfg(test)]
pub fn install(window: &SettingsWindow, config: Arc<rix_config::ConfigManager>) {
    let state = crate::theme_editor::State::new(config.get_theme());
    install_with_theme(window, config, state);
}
pub fn install_with_theme(
    window: &SettingsWindow,
    config: Arc<rix_config::ConfigManager>,
    theme_state: crate::theme_editor::State,
) {
    sync(
        window,
        &config.get_layout(),
        &config.get_theme(),
        &config.get_desktop(),
    );
    let weak = window.as_weak();
    let global = window.global::<BehaviorSettings>();
    let baseline = Arc::new(std::sync::Mutex::new((
        config.get_layout(),
        config.get_theme(),
        config.get_desktop(),
    )));
    global.on_apply(move || {
        let Some(window) = weak.upgrade() else { return };
        let global = window.global::<BehaviorSettings>();
        let (base, base_theme, base_desktop) = baseline.lock().unwrap().clone();
        let mut desktop = base_desktop.clone();
        desktop.group_tasks = global.get_group_tasks();
        desktop.server_side_decorations = global.get_server_side_decorations();
        desktop.clipboard.enabled = global.get_clipboard_enabled();
        desktop.clipboard.persist = global.get_clipboard_persist();
        desktop.clipboard.images = global.get_clipboard_images();
        desktop.clipboard.limit = global.get_clipboard_limit() as usize;
        desktop.switcher.include_minimized = global.get_switcher_minimized();
        desktop.switcher.show_delay_ms = global.get_switcher_delay().max(0) as u32;
        desktop.switcher.card_width = global.get_switcher_width().max(0) as u32;
        desktop.switcher.max_visible = global.get_switcher_count().max(0) as u32;
        desktop.window_snap.maximize_at_top_edge = global.get_window_snap_enabled();
        desktop.window_snap.trigger_distance =
            global.get_window_snap_distance().clamp(8, 160) as u32;
        desktop.window_snap.preview_opacity =
            (global.get_window_snap_opacity().clamp(8, 60) as f32) / 100.;
        let mut layout = base.clone();
        let mut theme = base_theme.clone();
        let modules = [
            ("keyboard", global.get_keyboard()),
            ("audio", global.get_audio()),
            ("network", global.get_network()),
        ];
        layout
            .services
            .tray_modules
            .retain(|name| modules.iter().any(|(id, on)| name == id && *on));
        for (id, on) in modules {
            if on && !layout.services.tray_modules.iter().any(|name| name == id) {
                layout.services.tray_modules.push(id.into());
            }
        }
        layout.services.maximum_volume = if global.get_amplified() { 150 } else { 100 };
        layout.services.previews = global.get_previews();
        layout.services.week_start = global.get_week_start().clamp(0, 6) as u32;
        for (key, value) in [
            ("popup_duration", global.get_animation_duration()),
            ("hover_duration", global.get_hover_duration()),
            ("window_duration", global.get_window_duration()),
            ("preview_delay", global.get_preview_delay()),
            ("preview_refresh", global.get_preview_refresh()),
            ("panel_icon_size", global.get_panel_icon_size()),
            ("panel_tray_icon_size", global.get_tray_icon_size()),
        ] {
            theme.appearance.metrics.insert(key.into(), value as f32);
        }
        let baseline = baseline.clone();
        let theme_state = theme_state.clone();
        crate::jobs::run(
            &window,
            move || {
                theme.validate()?;
                layout.services.validate()?;
                desktop.validate()?;
                let layout = crate::jobs::commit_layout(base, layout)?;
                let theme = crate::jobs::commit_theme(base_theme, theme)?;
                let desktop = crate::jobs::commit_desktop(base_desktop, desktop)?;
                Ok((theme, layout, desktop))
            },
            move |window, (theme, layout, desktop)| {
                *baseline.lock().unwrap() = (layout.clone(), theme.clone(), desktop.clone());
                window
                    .global::<SystemState>()
                    .set_maximum_volume(layout.services.maximum_volume as i32);
                window
                    .global::<SystemState>()
                    .set_week_start(layout.services.week_start as i32);
                window.global::<SystemState>().invoke_calendar_today();
                sync(window, &layout, &theme, &desktop);
                theme_state.load(window, &theme);
                window.set_status_message(crate::i18n::tr("Desktop behavior saved"));
            },
        );
    });
}

fn sync(
    window: &SettingsWindow,
    layout: &rix_config::LayoutConfig,
    theme: &rix_config::ThemeSettings,
    desktop: &rix_config::DesktopSettings,
) {
    let switcher = &desktop.switcher;
    let services = &layout.services;
    let global = window.global::<BehaviorSettings>();
    global.set_group_tasks(desktop.group_tasks);
    global.set_server_side_decorations(desktop.server_side_decorations);
    global.set_clipboard_enabled(desktop.clipboard.enabled);
    global.set_clipboard_persist(desktop.clipboard.persist);
    global.set_clipboard_images(desktop.clipboard.images);
    global.set_clipboard_limit(desktop.clipboard.limit as i32);
    global.set_animation_duration(theme.appearance.metric("popup_duration") as i32);
    global.set_hover_duration(theme.appearance.metric("hover_duration") as i32);
    global.set_window_duration(theme.appearance.metric("window_duration") as i32);
    global.set_switcher_minimized(switcher.include_minimized);
    global.set_switcher_delay(switcher.show_delay_ms as i32);
    global.set_switcher_width(switcher.card_width as i32);
    global.set_switcher_count(switcher.max_visible as i32);
    let snap = &desktop.window_snap;
    global.set_window_snap_enabled(snap.maximize_at_top_edge);
    global.set_window_snap_distance(snap.trigger_distance as i32);
    global.set_window_snap_opacity((snap.preview_opacity * 100.).round() as i32);
    global.set_keyboard(services.tray_modules.iter().any(|s| s == "keyboard"));
    global.set_audio(services.tray_modules.iter().any(|s| s == "audio"));
    global.set_network(services.tray_modules.iter().any(|s| s == "network"));
    global.set_amplified(services.maximum_volume > 100);
    global.set_previews(services.previews);
    global.set_week_start(services.week_start as i32);
    global.set_preview_delay(theme.appearance.metric("preview_delay") as i32);
    global.set_preview_refresh(theme.appearance.metric("preview_refresh") as i32);
    global.set_panel_icon_size(theme.appearance.metric("panel_icon_size") as i32);
    global.set_tray_icon_size(theme.appearance.metric("panel_tray_icon_size") as i32);
}
