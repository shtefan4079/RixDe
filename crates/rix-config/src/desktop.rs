use crate::ConfigError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum WallpaperFit {
    #[default]
    Fill,
    Fit,
    Stretch,
    Center,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WallpaperSettings {
    pub path: Option<PathBuf>,
    pub fit: WallpaperFit,
    pub blur: f32,
    pub dim: f32,
    pub background: [u8; 3],
    pub animate: bool,
    pub max_fps: u32,
}
impl Default for WallpaperSettings {
    fn default() -> Self {
        Self {
            path: None,
            fit: WallpaperFit::Fill,
            blur: 0.,
            dim: 0.,
            background: [22, 25, 34],
            animate: true,
            max_fps: 30,
        }
    }
}

impl WallpaperSettings {
    pub fn is_video(&self) -> bool {
        self.path
            .as_ref()
            .is_some_and(|path| is_video_wallpaper(path))
    }
}

pub fn is_video_wallpaper(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "mp4" | "m4v" | "webm" | "mkv" | "mov"
            )
        })
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopSettings {

    pub schema_version: u32,
    pub session: SessionSettings,

    pub power: Option<crate::power::PowerSettings>,
    pub clipboard: ClipboardSettings,

    pub language: String,
    pub group_tasks: bool,

    pub server_side_decorations: bool,
    pub window_appearance: crate::window_appearance::WindowAppearanceSettings,
    pub screen_cast_fps: u32,
    pub enabled_plugins: Vec<String>,
    pub wallpaper: WallpaperSettings,
    pub switcher: SwitcherSettings,
    pub window_snap: WindowSnapSettings,
}
impl Default for DesktopSettings {
    fn default() -> Self {
        Self {
            schema_version: crate::CURRENT_SCHEMA_VERSION,
            session: SessionSettings::default(),
            power: None,
            clipboard: ClipboardSettings::default(),
            language: "system".into(),
            group_tasks: true,
            server_side_decorations: true,
            window_appearance: crate::window_appearance::WindowAppearanceSettings::default(),
            screen_cast_fps: 30,
            enabled_plugins: Vec::new(),
            wallpaper: WallpaperSettings::default(),
            switcher: SwitcherSettings::default(),
            window_snap: WindowSnapSettings::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowSnapSettings {
    pub maximize_at_top_edge: bool,

    pub trigger_distance: u32,

    pub preview_opacity: f32,
}

impl Default for WindowSnapSettings {
    fn default() -> Self {
        Self {
            maximize_at_top_edge: true,
            trigger_distance: 28,
            preview_opacity: 0.24,
        }
    }
}

impl DesktopSettings {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.window_appearance.validate()?;
        if let Some(power) = &self.power {
            power.validate()?;
        }
        if !matches!(self.language.as_str(), "system" | "en") {
            return Err(ConfigError::InvalidValue(
                "Unsupported desktop language".into(),
            ));
        }
        if self.session.idle_lock_minutes > 240 {
            return Err(ConfigError::InvalidValue(
                "Idle lock delay must be 0–240 minutes".into(),
            ));
        }
        for command in [
            &self.session.lock_command,
            &self.session.notification_command,
            &self.session.authentication_command,
        ] {
            if command.len() > 32
                || command
                    .iter()
                    .any(|arg| arg.len() > 4096 || arg.contains('\0'))
                || command.first().is_some_and(String::is_empty)
            {
                return Err(ConfigError::InvalidValue(
                    "Invalid session service command".into(),
                ));
            }
        }
        if self.session.lock_command.is_empty() {
            return Err(ConfigError::InvalidValue(
                "A screen locker must be configured".into(),
            ));
        }
        if !(1..=200).contains(&self.clipboard.limit) {
            return Err(ConfigError::InvalidValue(
                "Clipboard limit must be 1–200".into(),
            ));
        }
        if !(1..=60).contains(&self.screen_cast_fps) {
            return Err(ConfigError::InvalidValue(
                "ScreenCast FPS must be between 1 and 60".into(),
            ));
        }
        if self.enabled_plugins.len() > 64
            || self.enabled_plugins.iter().any(|id| {
                id.is_empty()
                    || id.len() > 120
                    || !id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
                    || matches!(id.as_str(), "." | "..")
            })
        {
            return Err(ConfigError::InvalidValue(
                "Invalid plugin identifiers".into(),
            ));
        }
        if self.switcher.show_delay_ms > 1000
            || !(160..=400).contains(&self.switcher.card_width)
            || !(1..=7).contains(&self.switcher.max_visible)
        {
            return Err(ConfigError::InvalidValue(
                "Invalid switcher settings".into(),
            ));
        }
        if !(8..=160).contains(&self.window_snap.trigger_distance)
            || !self.window_snap.preview_opacity.is_finite()
            || !(0.08..=0.6).contains(&self.window_snap.preview_opacity)
        {
            return Err(ConfigError::InvalidValue(
                "Invalid window snapping settings".into(),
            ));
        }
        let w = &self.wallpaper;
        if !w.blur.is_finite()
            || !(0.0..=80.0).contains(&w.blur)
            || !w.dim.is_finite()
            || !(0.0..=1.0).contains(&w.dim)
            || !(1..=60).contains(&w.max_fps)
            || w.path.as_ref().is_some_and(|p| !p.is_absolute())
        {
            return Err(ConfigError::InvalidValue(
                "Invalid wallpaper settings".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod window_snap_tests {
    use super::DesktopSettings;

    #[test]
    fn older_desktop_preferences_receive_window_snap_defaults() {
        let mut value = serde_json::to_value(DesktopSettings::default()).unwrap();
        value.as_object_mut().unwrap().remove("window_snap");
        value
            .as_object_mut()
            .unwrap()
            .remove("server_side_decorations");

        let settings: DesktopSettings = serde_json::from_value(value).unwrap();
        assert!(settings.window_snap.maximize_at_top_edge);
        assert_eq!(settings.window_snap.trigger_distance, 28);
        assert_eq!(settings.window_snap.preview_opacity, 0.24);
        assert!(settings.server_side_decorations);
    }

    #[test]
    fn window_snap_settings_are_bounded_and_finite() {
        let mut settings = DesktopSettings::default();
        assert!(settings.validate().is_ok());

        settings.window_snap.trigger_distance = 7;
        assert!(settings.validate().is_err());

        settings.window_snap.trigger_distance = 28;
        settings.window_snap.preview_opacity = f32::NAN;
        assert!(settings.validate().is_err());
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionSettings {

    pub idle_lock_minutes: u32,
    pub respect_idle_inhibitors: bool,

    pub keep_running_with_external_display: bool,

    pub lock_command: Vec<String>,
    pub notifications: bool,
    pub authentication_agent: bool,
    pub notification_command: Vec<String>,

    pub authentication_command: Vec<String>,
}
impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            idle_lock_minutes: 10,
            respect_idle_inhibitors: true,
            keep_running_with_external_display: true,
            lock_command: vec!["rix-lock".into()],
            notifications: true,
            authentication_agent: true,
            notification_command: vec!["mako".into()],
            authentication_command: Vec::new(),
        }
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;
    #[test]
    fn older_profiles_get_session_defaults_and_delays_are_bounded() {
        let mut desktop: DesktopSettings = serde_json::from_str("{\"group_tasks\":false}").unwrap();
        assert_eq!(desktop.session.idle_lock_minutes, 10);
        assert!(desktop.session.respect_idle_inhibitors);
        assert!(desktop.session.keep_running_with_external_display);
        let existing: SessionSettings = serde_json::from_str("{\"idle_lock_minutes\":15}").unwrap();
        assert!(existing.keep_running_with_external_display);
        desktop.session.idle_lock_minutes = 0;
        assert!(desktop.validate().is_ok());
        desktop.session.idle_lock_minutes = 241;
        assert!(desktop.validate().is_err());
        desktop.session.idle_lock_minutes = 240;
        assert!(desktop.validate().is_ok());
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SwitcherSettings {
    pub include_minimized: bool,
    pub show_delay_ms: u32,
    pub card_width: u32,
    pub max_visible: u32,
}
impl Default for SwitcherSettings {
    fn default() -> Self {
        Self {
            include_minimized: true,
            show_delay_ms: 90,
            card_width: 260,
            max_visible: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipboardSettings {
    pub enabled: bool,
    pub persist: bool,
    pub images: bool,
    pub limit: usize,
}
impl Default for ClipboardSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            persist: true,
            images: true,
            limit: 50,
        }
    }
}
