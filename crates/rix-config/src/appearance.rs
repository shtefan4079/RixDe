use crate::{ConfigError, ThemeSettings};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub palette: BTreeMap<String, String>,
    pub metrics: BTreeMap<String, f32>,
    pub font_family: String,
    pub time_format: String,
    pub date_format: String,
    pub panel_frame: String,
    pub task_active_frame: String,
    pub task_hover_frame: String,
    pub menu_frame: String,
    pub decoration_theme: String,
}
impl Default for Appearance {
    fn default() -> Self {
        defaults().clone()
    }
}
fn defaults() -> &'static Appearance {
    static DEFAULT: std::sync::OnceLock<Appearance> = std::sync::OnceLock::new();
    DEFAULT.get_or_init(|| {

        let value: serde_json::Value =
            serde_json::from_str(include_str!("../themes/fairywren-dark.json")).unwrap();

        Appearance {
            palette: serde_json::from_value(value["appearance"]["palette"].clone()).unwrap(),
            metrics: serde_json::from_value(value["appearance"]["metrics"].clone()).unwrap(),
            font_family: value["appearance"]["font_family"].as_str().unwrap().into(),
            time_format: value["appearance"]["time_format"].as_str().unwrap().into(),
            date_format: value["appearance"]["date_format"].as_str().unwrap().into(),
            task_active_frame: value["appearance"]["task_active_frame"]
                .as_str()
                .unwrap()
                .into(),
            task_hover_frame: value["appearance"]["task_hover_frame"]
                .as_str()
                .unwrap()
                .into(),
            panel_frame: value["appearance"]["panel_frame"].as_str().unwrap().into(),
            menu_frame: value["appearance"]["menu_frame"].as_str().unwrap().into(),
            decoration_theme: value["appearance"]["decoration_theme"]
                .as_str()
                .unwrap()
                .into(),
        }
    })
}
impl Appearance {
    pub fn metric(&self, name: &str) -> f32 {
        self.metrics
            .get(name)
            .copied()
            .or_else(|| defaults().metrics.get(name).copied())
            .unwrap_or(0.)
    }
    pub fn color(&self, name: &str) -> rix_core::Color {
        self.palette
            .get(name)
            .or_else(|| defaults().palette.get(name))
            .and_then(|value| rix_core::Color::from_hex(value))
            .unwrap_or_default()
    }

    pub fn prefers_dark(&self) -> bool {
        let background = self.color("background");
        let linear = |channel: f32| {
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        };
        let luminance = 0.2126 * linear(background.r)
            + 0.7152 * linear(background.g)
            + 0.0722 * linear(background.b);
        luminance < 0.5
    }
}
impl ThemeSettings {
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |message: &str| ConfigError::InvalidValue(message.into());
        if !self.opacity.is_finite() || !(0.0..=1.0).contains(&self.opacity) {
            return Err(invalid("Opacity must be between 0 and 1"));
        }
        if !self.noise_opacity.is_finite() || !(0.0..=0.5).contains(&self.noise_opacity) {
            return Err(invalid("Noise must be between 0 and 0.5"));
        }
        if self.blur_radius > 128 {
            return Err(invalid("Blur radius must not exceed 128"));
        }
        self.effects.validate()?;
        if !(16..=96).contains(&self.cursor_size) {
            return Err(invalid("Cursor size must be between 16 and 96"));
        }
        for value in self
            .appearance
            .palette
            .values()
            .chain(std::iter::once(&self.accent_color))
        {
            if rix_core::Color::from_hex(value).is_none() {
                return Err(invalid("Colors must use #RRGGBB or #RRGGBBAA"));
            }
        }
        for (name, value) in &self.appearance.metrics {
            if !value.is_finite() || !(0.0..=4096.).contains(value) {
                return Err(invalid(&format!("Invalid metric: {name}")));
            }
        }
        let task_highlight_scale = self.appearance.metric("task_highlight_scale");
        if !(1.0..=2.0).contains(&task_highlight_scale) {
            return Err(invalid("Task highlight scale must be between 1 and 2"));
        }
        for format in [&self.appearance.time_format, &self.appearance.date_format] {
            if chrono::format::StrftimeItems::new(format)
                .any(|item| matches!(item, chrono::format::Item::Error))
            {
                return Err(invalid("Invalid clock/date format"));
            }
        }
        if self.appearance.metric("decoration_button_size")
            > self.appearance.metric("decoration_title_height")
        {
            return Err(invalid("Decoration button must fit inside the titlebar"));
        }
        for name in [
            "favorite_cell_width",
            "favorite_cell_height",
            "menu_row_height",
            "task_width",
        ] {
            if self.appearance.metric(name) < 1. {
                return Err(invalid(&format!("Metric {name} must be positive")));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlurAlgorithm {
    DualKawase,
    Gaussian,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BackdropSettings {
    pub algorithm: BlurAlgorithm,
    pub iterations: u32,
    pub offset: f32,
    pub gaussian_downsample: u32,
    pub noise_scale: f32,

    pub noise_bias: f32,
    pub brightness: f32,
    pub contrast: f32,
    pub saturation: f32,
}
impl Default for BackdropSettings {
    fn default() -> Self {
        let package: serde_json::Value =
            serde_json::from_str(include_str!("../themes/fairywren-dark.json")).unwrap();
        serde_json::from_value(package["effects"].clone()).unwrap()
    }
}
impl BackdropSettings {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(1..=6).contains(&self.iterations) || !(1..=4).contains(&self.gaussian_downsample) {
            return Err(ConfigError::InvalidValue(
                "Invalid blur pass count or downsample factor".into(),
            ));
        }
        for (name, value, min, max) in [
            ("offset", self.offset, 0.0, 16.0),
            ("noise_scale", self.noise_scale, 0.25, 8.0),
            ("noise_bias", self.noise_bias, 0.0, 1.0),
            ("brightness", self.brightness, 0.0, 2.0),
            ("contrast", self.contrast, 0.0, 2.0),
            ("saturation", self.saturation, 0.0, 2.0),
        ] {
            if !value.is_finite() || !(min..=max).contains(&value) {
                return Err(ConfigError::InvalidValue(format!(
                    "Invalid effect parameter: {name}"
                )));
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MaterialEffects {
    pub blur_radius: u32,
    pub noise_opacity: f32,
    pub options: BackdropSettings,
}
impl From<&ThemeSettings> for MaterialEffects {
    fn from(theme: &ThemeSettings) -> Self {
        Self {
            blur_radius: theme.blur_radius,
            noise_opacity: theme.noise_opacity,
            options: theme.effects,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_color_scheme_comes_from_background_color() {
        let mut appearance = Appearance::default();
        assert!(appearance.prefers_dark());

        appearance
            .palette
            .insert("background".into(), "#f2f6fc".into());
        assert!(!appearance.prefers_dark());

        appearance
            .palette
            .insert("background".into(), "#5271ff".into());
        assert!(appearance.prefers_dark());
    }

    #[test]
    fn old_theme_migrates_without_losing_user_values() {
        let old = r##"{"name":"Mine","icon_theme":"test","cursor_theme":"test","cursor_size":24,"opacity":0.42,"blur_radius":8,"noise_opacity":0.01,"accent_color":"#aabbcc"}"##;
        let theme: ThemeSettings = serde_json::from_str(old).unwrap();
        assert_eq!(theme.opacity, 0.42);
        theme.validate().unwrap();
        assert_eq!(theme.appearance.metric("task_width"), 40.);
    }
    #[test]
    fn rejects_invalid_effects_and_colors() {
        let mut theme = ThemeSettings {
            noise_opacity: f32::NAN,
            ..Default::default()
        };
        assert!(theme.validate().is_err());
        theme.noise_opacity = 0.;
        theme.accent_color = "not_a_hex_color".into();
        assert!(theme.validate().is_err());
        for theme in [
            ThemeSettings::plasma_glass(),
            ThemeSettings::aero_glass(),
            ThemeSettings::cyber_dark(),
            ThemeSettings::crisp_light(),
        ] {
            theme.validate().unwrap();
        }
    }
}
