use rix_config::{LayoutConfig, PanelConfig, PanelTarget};
use std::sync::OnceLock;

static TARGET: OnceLock<PanelTarget> = OnceLock::new();
pub fn initialize(panel: String, output: String) {
    let _ = TARGET.set(PanelTarget { panel, output });
}
pub fn panel(layout: &LayoutConfig) -> Option<&PanelConfig> {
    match TARGET.get() {
        Some(target) => layout.panels.iter().find(|panel| panel.id == target.panel),
        None => layout.panels.first(),
    }
}
pub fn panel_mut(layout: &mut LayoutConfig) -> Option<&mut PanelConfig> {
    match TARGET.get() {
        Some(target) => layout
            .panels
            .iter_mut()
            .find(|panel| panel.id == target.panel),
        None => layout.panels.first_mut(),
    }
}
pub fn output() -> Option<String> {
    TARGET
        .get()
        .filter(|target| !target.output.is_empty())
        .map(|target| target.output.clone())
}
pub fn matches(target: &PanelTarget) -> bool {
    TARGET.get().is_none_or(|own| {
        own.panel == target.panel && (own.output.is_empty() || own.output == target.output)
    })
}

pub fn foreground_window_on_output(window: &rix_ipc::WindowInfo) -> bool {
    foreground_window_on(window, output().as_deref())
}

fn foreground_window_on(window: &rix_ipc::WindowInfo, output: Option<&str>) -> bool {
    if window.is_minimized {
        return false;
    }
    match &window.foreground_outputs {
        Some(outputs) => {
            !outputs.is_empty() && output.is_none_or(|own| outputs.iter().any(|name| name == own))
        }

        None => {
            window.is_active
                && output.is_none_or(|own| {
                    window.outputs.is_empty() || window.outputs.iter().any(|name| name == own)
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fullscreen() -> rix_ipc::WindowInfo {
        rix_ipc::WindowInfo {
            id: 1,
            title: "Game".into(),
            app_id: "game".into(),
            outputs: vec!["DP-1".into(), "DP-2".into()],
            foreground_outputs: Some(vec!["DP-2".into()]),
            is_active: false,
            is_minimized: false,
            is_maximized: false,
            is_fullscreen: true,
        }
    }

    #[test]
    fn foreground_uses_output_stacking_instead_of_global_keyboard_focus() {
        let mut game = fullscreen();
        assert!(!foreground_window_on(&game, Some("DP-1")));
        assert!(foreground_window_on(&game, Some("DP-2")));
        assert!(foreground_window_on(&game, None));
        game.is_active = true;
        assert!(!foreground_window_on(&game, Some("DP-1")));
        game.foreground_outputs = Some(vec![]);
        assert!(!foreground_window_on(&game, None));
        game.foreground_outputs = Some(vec!["DP-2".into()]);
        game.is_minimized = true;
        assert!(!foreground_window_on(&game, Some("DP-2")));
    }

    #[test]
    fn legacy_snapshots_follow_the_active_window_on_the_panel_output() {
        let mut game = fullscreen();
        game.foreground_outputs = None;
        assert!(!foreground_window_on(&game, Some("DP-1")));
        game.is_active = true;
        assert!(foreground_window_on(&game, Some("DP-1")));
        assert!(!foreground_window_on(&game, Some("HDMI-A-1")));
        game.outputs.clear();
        assert!(foreground_window_on(&game, Some("DP-1")));
    }
}
