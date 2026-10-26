use crate::{ConfigError, ThemeSettings};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const MAX_WINDOW_APPEARANCE_RULES: usize = 64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowAppearanceSettings {
    pub use_desktop_material: bool,
    pub rules: Vec<WindowAppearanceRule>,
}

impl Default for WindowAppearanceSettings {
    fn default() -> Self {
        Self {
            use_desktop_material: true,
            rules: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowAppearanceRule {

    pub app_id: String,
    #[serde(default = "material_default")]
    pub use_desktop_material: bool,

    #[serde(default)]
    pub theme: Option<ThemeSettings>,
}

fn material_default() -> bool {
    true
}

impl WindowAppearanceSettings {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.rules.len() > MAX_WINDOW_APPEARANCE_RULES {
            return Err(ConfigError::InvalidValue(
                "At most 64 window appearance rules are allowed".into(),
            ));
        }
        let mut ids = HashSet::new();
        for rule in &self.rules {
            if rule.app_id.is_empty()
                || rule.app_id.len() > 255
                || rule.app_id.trim() != rule.app_id
                || rule.app_id.chars().any(char::is_control)
            {
                return Err(ConfigError::InvalidValue(
                    "Invalid window appearance application ID".into(),
                ));
            }
            if !ids.insert(&rule.app_id) {
                return Err(ConfigError::InvalidValue(
                    "Window appearance application IDs must be unique".into(),
                ));
            }
            if let Some(theme) = &rule.theme {
                theme.validate()?;
            }
        }
        Ok(())
    }

    pub fn effective(&self, app_id: &str, desktop: &ThemeSettings) -> (ThemeSettings, bool) {
        match self.rules.iter().find(|rule| rule.app_id == app_id) {
            Some(rule) => (
                rule.theme.as_ref().unwrap_or(desktop).clone(),
                rule.use_desktop_material,
            ),
            None => (desktop.clone(), self.use_desktop_material),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_desktop_uses_material_and_no_rules() {
        let desktop: crate::DesktopSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(
            desktop.window_appearance,
            WindowAppearanceSettings::default()
        );
        let settings: WindowAppearanceSettings =
            serde_json::from_str("{\"rules\":[{\"app_id\":\"org.example.Editor\"}]}").unwrap();
        assert!(settings.rules[0].use_desktop_material);
        assert!(settings.rules[0].theme.is_none());
    }

    #[test]
    fn exact_override_keeps_snapshot_and_global_fallback() {
        let desktop = ThemeSettings::cyber_dark();
        let custom = ThemeSettings::crisp_light();
        let settings = WindowAppearanceSettings {
            use_desktop_material: false,
            rules: vec![WindowAppearanceRule {
                app_id: "org.example.Editor".into(),
                use_desktop_material: true,
                theme: Some(custom.clone()),
            }],
        };
        assert_eq!(
            settings.effective("org.example.Editor", &desktop),
            (custom.clone(), true)
        );
        assert_eq!(
            settings.effective("org.example.editor", &desktop),
            (desktop.clone(), false)
        );
        let restored: WindowAppearanceSettings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(restored, settings);
        assert_eq!(
            restored.effective("org.example.Editor", &ThemeSettings::aero_glass()),
            (custom, true)
        );
    }

    #[test]
    fn rules_reject_duplicates_invalid_ids_invalid_themes_and_overflow() {
        let mut settings = WindowAppearanceSettings::default();
        let mut rule = WindowAppearanceRule {
            app_id: "org.example.Editor".into(),
            use_desktop_material: false,
            theme: None,
        };
        settings.rules.push(rule.clone());
        assert!(settings.validate().is_ok());
        settings.rules.push(rule.clone());
        assert!(settings.validate().is_err());
        for id in ["", " org.example.Editor", "org.example.Editor\n"] {
            rule.app_id = id.into();
            settings.rules = vec![rule.clone()];
            assert!(settings.validate().is_err());
        }
        rule.app_id = "a".repeat(256);
        settings.rules = vec![rule.clone()];
        assert!(settings.validate().is_err());
        rule.app_id = "org.example.Editor".into();
        let theme = ThemeSettings {
            opacity: f32::NAN,
            ..ThemeSettings::default()
        };
        rule.theme = Some(theme);
        settings.rules = vec![rule];
        assert!(settings.validate().is_err());
        settings.rules = (0..65)
            .map(|index| WindowAppearanceRule {
                app_id: format!("org.example.App{index}"),
                use_desktop_material: true,
                theme: None,
            })
            .collect();
        assert!(settings.validate().is_err());
        settings.rules.pop();
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn appearance_edit_preserves_concurrent_desktop_changes() {
        let base = crate::DesktopSettings::default();
        let mut updated = base.clone();
        updated.window_appearance.use_desktop_material = false;
        let mut current = base.clone();
        current.language = "en".into();
        current.wallpaper.dim = 0.4;
        let merged: crate::DesktopSettings =
            crate::edit::merge_document(&base, &updated, &current).unwrap();
        assert!(!merged.window_appearance.use_desktop_material);
        assert_eq!(merged.language, "en");
        assert_eq!(merged.wallpaper, current.wallpaper);
    }
}
