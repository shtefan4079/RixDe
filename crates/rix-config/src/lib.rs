pub mod appearance;
pub mod desktop;
pub mod edit;
pub mod model;
pub use desktop::{
    DesktopSettings, SwitcherSettings, WallpaperFit, WallpaperSettings, WindowSnapSettings,
};
pub mod panels;
pub mod parser;
pub mod power;
pub mod themes;
pub use panels::{panel_targets, PanelTarget};
pub mod plasma;
pub mod watcher;
pub mod window_appearance;
pub use window_appearance::{WindowAppearanceRule, WindowAppearanceSettings};

pub use model::*;
pub use parser::{format_layout_kdl, parse_layout_kdl, ConfigError};
pub use plasma::PlasmaSnapshot;
pub use watcher::{write_atomic, ConfigManager};

#[cfg(test)]
mod tests {
    use super::*;
    use rix_core::{Alignment, AutoHide, PanelLength, Position};

    #[test]
    fn test_kdl_roundtrip() {
        let original = LayoutConfig::default_layout();
        let kdl_str = format_layout_kdl(&original);
        let parsed = parse_layout_kdl(&kdl_str).expect("Failed to parse serialized KDL");

        assert_eq!(parsed.panels.len(), 1);
        let panel = &parsed.panels[0];
        assert_eq!(panel.id, "main-panel");
        assert_eq!(panel.position, Position::Bottom);
        assert_eq!(panel.thickness, 46);
        assert!(panel.floating);
        assert_eq!(panel.alignment, Alignment::Center);
        assert!(panel.exclusive_zone);
        assert_eq!(panel.autohide, AutoHide::None);
        assert_eq!(panel.length, PanelLength::Fill);
        assert_eq!(panel.widgets.len(), original.panels[0].widgets.len());
    }

    #[test]
    fn test_custom_properties() {
        let mut layout = LayoutConfig::default_layout();
        layout.panels[0].widgets[0]
            .properties
            .insert("custom-key".to_string(), serde_json::json!("test-value"));

        let kdl_str = format_layout_kdl(&layout);
        assert!(kdl_str.contains("custom-key=\"test-value\""));

        let parsed = parse_layout_kdl(&kdl_str).unwrap();
        assert_eq!(
            parsed.panels[0].widgets[0].properties.get("custom-key"),
            Some(&serde_json::json!("test-value"))
        );
    }
}

#[cfg(test)]
mod desktop_config_tests {
    use super::*;
    #[test]
    fn keyboard_and_service_preferences_roundtrip_and_validate() {
        let mut layout = LayoutConfig::default_layout();
        layout.keyboard.layouts.push(KeyboardLayout {
            layout: "ua".into(),
            variant: "phonetic".into(),
            label: "UA".into(),
        });
        layout.keyboard.switch_option = "grp:win_space_toggle".into();
        layout.services.tray_modules = vec!["audio".into(), "keyboard".into()];
        layout.services.week_start = 6;
        layout.services.maximum_volume = 150;
        layout.services.previews = false;
        assert_eq!(
            parse_layout_kdl(&format_layout_kdl(&layout)).unwrap(),
            layout
        );
        assert!(parse_layout_kdl("keyboard repeat-rate=-1 { layout \"us\"; }").is_err());
        assert!(parse_layout_kdl("services week-start=8").is_err());
        assert_eq!(
            parse_layout_kdl("keyboard { layout \"ua\"; }")
                .unwrap()
                .keyboard
                .layouts[0]
                .layout,
            "ua"
        );
    }
}
