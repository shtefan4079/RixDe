use rix_core::{Alignment, AutoHide, PanelLength, Position};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaVersion {

    Current,

    Migrated,

    Future,
}

impl SchemaVersion {
    pub fn resolve(declared: u32) -> Self {
        if declared == 0 || declared == CURRENT_SCHEMA_VERSION {
            Self::Current
        } else if declared < CURRENT_SCHEMA_VERSION {
            Self::Migrated
        } else {
            Self::Future
        }
    }

    pub fn is_future(self) -> bool {
        matches!(self, Self::Future)
    }
}

pub fn migrate_schema_version(declared: u32) -> Option<u32> {
    let version = if declared == 0 {
        CURRENT_SCHEMA_VERSION
    } else {
        declared
    };
    if version < CURRENT_SCHEMA_VERSION {

        return None;
    }
    Some(version)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash, Default)]
#[serde(rename_all = "lowercase")]
pub enum WidgetSection {
    #[default]
    Start,
    Center,
    End,
}

impl WidgetSection {
    pub fn as_str(&self) -> &'static str {
        match self {
            WidgetSection::Start => "start",
            WidgetSection::Center => "center",
            WidgetSection::End => "end",
        }
    }

    pub fn from_str_loose(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "center" | "middle" => WidgetSection::Center,
            "end" | "right" | "bottom" => WidgetSection::End,
            _ => WidgetSection::Start,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum WidgetKind {
    Launcher,
    Taskbar,
    Spacer,
    Clock,
    Tray,
    ShowDesktop,
    QuickSettings,
    Power,
    CustomExec,
    Other(String),
}

impl WidgetKind {
    pub fn as_str(&self) -> &str {
        match self {
            WidgetKind::Launcher => "launcher",
            WidgetKind::Taskbar => "taskbar",
            WidgetKind::Spacer => "spacer",
            WidgetKind::Clock => "clock",
            WidgetKind::Tray => "tray",
            WidgetKind::ShowDesktop => "show-desktop",
            WidgetKind::QuickSettings => "quick-settings",
            WidgetKind::Power => "power",
            WidgetKind::CustomExec => "custom-exec",
            WidgetKind::Other(s) => s.as_str(),
        }
    }

    pub fn from_name(name: &str) -> Self {
        match name {
            "launcher" => WidgetKind::Launcher,
            "taskbar" => WidgetKind::Taskbar,
            "spacer" => WidgetKind::Spacer,
            "clock" => WidgetKind::Clock,
            "tray" => WidgetKind::Tray,
            "show-desktop" => WidgetKind::ShowDesktop,
            "quick-settings" => WidgetKind::QuickSettings,
            "power" => WidgetKind::Power,
            "custom-exec" => WidgetKind::CustomExec,
            other => WidgetKind::Other(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WidgetConfig {
    pub id: String,
    pub kind: WidgetKind,
    pub section: WidgetSection,
    #[serde(default)]
    pub properties: HashMap<String, serde_json::Value>,
}

impl WidgetConfig {
    pub fn new(id: impl Into<String>, kind: WidgetKind, section: WidgetSection) -> Self {
        Self {
            id: id.into(),
            kind,
            section,
            properties: HashMap::new(),
        }
    }

    pub fn with_property(
        mut self,
        key: impl Into<String>,
        value: impl Into<serde_json::Value>,
    ) -> Self {
        self.properties.insert(key.into(), value.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PanelConfig {
    pub id: String,
    pub screen: String,
    pub position: Position,
    pub thickness: u32,
    pub length: PanelLength,
    pub floating: bool,
    pub floating_margin: u32,
    pub corner_radius: u32,
    #[serde(default = "enabled")]
    pub use_theme_frame: bool,
    #[serde(default = "full_opacity")]
    pub opacity: f32,
    pub exclusive_zone: bool,
    pub autohide: AutoHide,
    pub alignment: Alignment,
    #[serde(default)]
    pub widgets: Vec<WidgetConfig>,
}

impl Default for PanelConfig {
    fn default() -> Self {
        Self {
            id: "main-panel".to_string(),
            screen: "focused".to_string(),
            position: Position::Bottom,
            thickness: 46,
            length: PanelLength::Fill,
            floating: true,
            floating_margin: 4,
            corner_radius: 8,
            use_theme_frame: true,
            opacity: 1.0,
            exclusive_zone: true,
            autohide: AutoHide::None,
            alignment: Alignment::Center,
            widgets: vec![
                WidgetConfig::new("w-spacer-1", WidgetKind::Spacer, WidgetSection::Start),
                WidgetConfig::new("w-launcher", WidgetKind::Launcher, WidgetSection::Center),
                WidgetConfig::new("w-taskbar", WidgetKind::Taskbar, WidgetSection::Center),
                WidgetConfig::new("w-spacer-2", WidgetKind::Spacer, WidgetSection::End),
                WidgetConfig::new("w-tray", WidgetKind::Tray, WidgetSection::End),
                WidgetConfig::new("w-clock", WidgetKind::Clock, WidgetSection::End),
                WidgetConfig::new(
                    "w-show-desktop",
                    WidgetKind::ShowDesktop,
                    WidgetSection::End,
                ),
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LauncherConfig {
    pub width: u32,
    pub height: u32,
    pub icon: String,
    pub favorites: Vec<String>,
    pub alignment: Alignment,
    pub footer: Vec<LauncherAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LauncherAction {
    pub action: String,
    pub label: String,
    pub icon: String,
    pub end: bool,
    pub icon_only: bool,
    #[serde(default = "default_visible")]
    pub visible: bool,
}
fn default_visible() -> bool {
    true
}
pub fn default_launcher_footer() -> Vec<LauncherAction> {
    static DEFAULTS: std::sync::OnceLock<Vec<LauncherAction>> = std::sync::OnceLock::new();
    DEFAULTS
        .get_or_init(|| {
            serde_json::from_str(include_str!("../defaults/launcher-footer.json"))
                .expect("valid bundled launcher footer")
        })
        .clone()
}

impl Default for LauncherConfig {
    fn default() -> Self {
        Self {
            width: 700,
            height: 580,
            alignment: Alignment::Center,
            footer: default_launcher_footer(),
            icon: "start-here".into(),
            favorites: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutConfig {

    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub keyboard: KeyboardConfig,
    #[serde(default)]
    pub services: ServiceSettings,
    pub panels: Vec<PanelConfig>,
    #[serde(default)]
    pub launcher: LauncherConfig,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            keyboard: KeyboardConfig::default(),
            services: ServiceSettings::default(),
            panels: Vec::new(),
            launcher: LauncherConfig::default(),
        }
    }
}

impl LayoutConfig {
    pub fn default_layout() -> Self {
        Self {
            panels: vec![PanelConfig::default()],
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThemeSettings {

    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub effects: crate::appearance::BackdropSettings,
    #[serde(default)]
    pub appearance: Box<crate::appearance::Appearance>,
    pub name: String,
    pub icon_theme: String,
    pub cursor_theme: String,
    pub cursor_size: u32,
    pub opacity: f32,
    pub blur_radius: u32,
    pub noise_opacity: f32,
    pub accent_color: String,
}

impl Default for ThemeSettings {
    fn default() -> Self {
        Self::fairywren_dark()
    }
}

impl ThemeSettings {

    fn bundled(source: &str) -> Self {
        let mut theme: Self = serde_json::from_str(source).expect("bundled theme must be valid");
        theme.schema_version = CURRENT_SCHEMA_VERSION;
        theme
    }
    pub fn fairywren_dark() -> Self {
        Self::bundled(include_str!("../themes/fairywren-dark.json"))
    }
    pub fn plasma_glass() -> Self {
        Self::bundled(include_str!("../themes/plasma-glass.json"))
    }
    pub fn aero_glass() -> Self {
        Self::bundled(include_str!("../themes/aero-glass.json"))
    }
    pub fn cyber_dark() -> Self {
        Self::bundled(include_str!("../themes/cyber-dark.json"))
    }
    pub fn crisp_light() -> Self {
        Self::bundled(include_str!("../themes/crisp-light.json"))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplaySettings {

    #[serde(default)]
    pub schema_version: u32,

    #[serde(default)]
    pub outputs: std::collections::BTreeMap<String, OutputProfile>,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub flipped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputMode {
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputProfile {
    pub mode: OutputMode,
    pub flipped: bool,
    #[serde(default = "default_output_scale")]
    pub scale_percent: u16,
    #[serde(default)]
    pub rotation: u16,
    #[serde(default)]
    pub position: Option<OutputPosition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputPosition {
    pub x: i32,
    pub y: i32,
}

fn default_output_scale() -> u16 {
    100
}

impl OutputProfile {
    pub fn validate(&self) -> Result<(), String> {
        if self.mode.width == 0
            || self.mode.height == 0
            || self.mode.width > 16384
            || self.mode.height > 16384
            || self.mode.refresh_millihz == 0
        {
            return Err("Invalid monitor mode".into());
        }
        if !(50..=400).contains(&self.scale_percent) {
            return Err("Display scale must be between 50% and 400%".into());
        }
        if ![0, 90, 180, 270].contains(&self.rotation) {
            return Err("Display rotation must be 0, 90, 180 or 270 degrees".into());
        }
        if self
            .position
            .is_some_and(|p| p.x.abs_diff(0) > 32768 || p.y.abs_diff(0) > 32768)
        {
            return Err("Display position is outside the supported desktop".into());
        }
        Ok(())
    }
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            outputs: Default::default(),
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            flipped: false,
        }
    }
}

fn enabled() -> bool {
    true
}

fn full_opacity() -> f32 {
    1.0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyboardConfig {
    pub layouts: Vec<KeyboardLayout>,
    pub switch_option: String,
    pub repeat_rate: u32,
    pub repeat_delay: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyboardLayout {
    pub layout: String,
    #[serde(default)]
    pub variant: String,
    #[serde(default)]
    pub label: String,
}
impl Default for KeyboardConfig {
    fn default() -> Self {
        Self {
            layouts: vec![KeyboardLayout {
                layout: "us".into(),
                variant: String::new(),
                label: "US".into(),
            }],
            switch_option: "grp:alt_shift_toggle".into(),
            repeat_rate: 30,
            repeat_delay: 500,
        }
    }
}
impl KeyboardConfig {
    pub fn validate(&self) -> Result<(), crate::ConfigError> {
        let safe = |s: &str| {
            s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_:-".contains(&b))
        };
        if self.layouts.is_empty()
            || self.layouts.len() > 4
            || self
                .layouts
                .iter()
                .any(|l| l.layout.is_empty() || !safe(&l.layout) || !safe(&l.variant))
            || !safe(&self.switch_option)
            || self.repeat_rate > 100
            || !(100..=2000).contains(&self.repeat_delay)
        {
            return Err(crate::ConfigError::InvalidValue(
                "Keyboard needs 1–4 valid XKB layouts, rate 0–100 and delay 100–2000 ms".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServiceSettings {
    pub tray_modules: Vec<String>,
    pub maximum_volume: u32,
    pub previews: bool,

    pub week_start: u32,
}
impl Default for ServiceSettings {
    fn default() -> Self {
        Self {
            tray_modules: vec!["keyboard".into(), "audio".into(), "network".into()],
            maximum_volume: 100,
            previews: true,
            week_start: 0,
        }
    }
}
impl ServiceSettings {
    pub fn validate(&self) -> Result<(), crate::ConfigError> {
        if ![100, 150].contains(&self.maximum_volume)
            || self.week_start > 6
            || self
                .tray_modules
                .iter()
                .any(|s| !["keyboard", "audio", "network"].contains(&s.as_str()))
            || self
                .tray_modules
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                != self.tray_modules.len()
        {
            return Err(crate::ConfigError::InvalidValue(
                "Invalid desktop service settings".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod schema_version_tests {
    use super::*;

    #[test]
    fn schema_versions_resolve_legacy_current_and_future() {
        assert_eq!(SchemaVersion::resolve(0), SchemaVersion::Current);
        assert_eq!(
            SchemaVersion::resolve(CURRENT_SCHEMA_VERSION),
            SchemaVersion::Current
        );
        assert_eq!(
            SchemaVersion::resolve(CURRENT_SCHEMA_VERSION + 1),
            SchemaVersion::Future
        );
        assert!(SchemaVersion::resolve(CURRENT_SCHEMA_VERSION + 1).is_future());

        assert_eq!(migrate_schema_version(0), Some(CURRENT_SCHEMA_VERSION));
        assert_eq!(
            migrate_schema_version(CURRENT_SCHEMA_VERSION),
            Some(CURRENT_SCHEMA_VERSION)
        );

        assert_eq!(
            migrate_schema_version(CURRENT_SCHEMA_VERSION + 1),
            Some(CURRENT_SCHEMA_VERSION + 1)
        );
    }

    fn without_version<T: Serialize>(document: &T) -> serde_json::Value {
        let mut value = serde_json::to_value(document).unwrap();
        value.as_object_mut().unwrap().remove("schema_version");
        value
    }

    #[test]
    fn documents_default_to_current_schema_and_missing_headers_are_legacy() {
        let theme: ThemeSettings =
            serde_json::from_value(without_version(&ThemeSettings::default())).unwrap();
        assert_eq!(theme.schema_version, 0);
        assert_eq!(
            ThemeSettings::default().schema_version,
            CURRENT_SCHEMA_VERSION
        );

        let display: DisplaySettings =
            serde_json::from_value(without_version(&DisplaySettings::default())).unwrap();
        assert_eq!(display.schema_version, 0);
        assert_eq!(
            DisplaySettings::default().schema_version,
            CURRENT_SCHEMA_VERSION
        );

        let desktop: crate::DesktopSettings =
            serde_json::from_value(without_version(&crate::DesktopSettings::default())).unwrap();

        assert_eq!(desktop.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(
            crate::DesktopSettings::default().schema_version,
            CURRENT_SCHEMA_VERSION
        );

        assert_eq!(
            LayoutConfig::default().schema_version,
            CURRENT_SCHEMA_VERSION
        );
        assert_eq!(
            LayoutConfig::default_layout().schema_version,
            CURRENT_SCHEMA_VERSION
        );

        let serialized = serde_json::to_string(&ThemeSettings::default()).unwrap();
        assert!(serialized.contains("\"schema_version\":1"));
        let restored: ThemeSettings = serde_json::from_str(&serialized).unwrap();
        assert_eq!(restored, ThemeSettings::default());

        let display = DisplaySettings::default();
        let restored: DisplaySettings =
            serde_json::from_str(&serde_json::to_string(&display).unwrap()).unwrap();
        assert_eq!(restored, display);

        let desktop = crate::DesktopSettings::default();
        let restored: crate::DesktopSettings =
            serde_json::from_str(&serde_json::to_string(&desktop).unwrap()).unwrap();
        assert_eq!(restored, desktop);
    }
}
