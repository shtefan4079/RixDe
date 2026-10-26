use crate::{parse_layout_kdl, ConfigError, LayoutConfig, ThemeSettings};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ThemePackageInfo {
    pub version: u32,
    pub id: String,
    pub name: String,
    pub description: String,
    pub theme: String,
    pub layout: Option<String>,
    #[serde(default)]
    pub required_assets: Vec<String>,
}
#[derive(Debug, Clone)]
pub struct ThemePackage {
    pub info: ThemePackageInfo,
    root: PathBuf,
}
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct AppearanceSnapshot {
    pub theme: ThemeSettings,
    pub layout: LayoutConfig,
}
pub fn data_roots() -> Vec<PathBuf> {
    let user = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    let system = std::env::var_os("XDG_DATA_DIRS")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    user.into_iter()
        .chain(std::env::split_paths(&system))
        .collect()
}
fn relative(value: &str) -> bool {
    !value.is_empty()
        && Path::new(value)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, ConfigError> {
    let bytes = std::fs::read(path)?;
    if bytes.len() > 1024 * 1024 {
        return Err(ConfigError::InvalidValue(
            "Theme document exceeds 1 MiB".into(),
        ));
    }
    serde_json::from_slice(&bytes).map_err(|error| ConfigError::InvalidValue(error.to_string()))
}
impl ThemePackage {
    pub fn read(root: PathBuf) -> Result<Self, ConfigError> {
        let info: ThemePackageInfo = read_json(&root.join("manifest.json"))?;
        if info.version != 1
            || !relative(&info.id)
            || info.id.contains('/')
            || !relative(&info.theme)
            || info.layout.as_ref().is_some_and(|layout| !relative(layout))
            || info.required_assets.iter().any(|asset| !relative(asset))
        {
            return Err(ConfigError::InvalidValue(
                "Invalid portable theme manifest".into(),
            ));
        }
        Ok(Self { info, root })
    }
    pub fn load(
        &self,
        current: &LayoutConfig,
        include_layout: bool,
    ) -> Result<AppearanceSnapshot, ConfigError> {
        for asset in &self.info.required_assets {
            if !data_roots().iter().any(|root| root.join(asset).is_file()) {
                return Err(ConfigError::InvalidValue(format!(
                    "Theme resource is missing: {asset}; start RixDE with its installed launcher"
                )));
            }
        }
        let theme: ThemeSettings = read_json(&self.root.join(&self.info.theme))?;
        theme.validate()?;
        let mut layout = current.clone();
        if include_layout {
            if let Some(name) = &self.info.layout {
                let imported = parse_layout_kdl(&std::fs::read_to_string(self.root.join(name))?)?;
                layout.panels = imported.panels;
                layout.launcher.width = imported.launcher.width;
                layout.launcher.height = imported.launcher.height;
                layout.launcher.icon = imported.launcher.icon;
                layout.launcher.alignment = imported.launcher.alignment;
            }
        }
        crate::panels::validate_panels(&layout)?;
        layout.keyboard.validate()?;
        layout.services.validate()?;
        Ok(AppearanceSnapshot { theme, layout })
    }
}
pub fn discover_in(roots: &[PathBuf]) -> Vec<ThemePackage> {
    let mut packages = std::collections::BTreeMap::new();
    for data in roots {
        let Ok(entries) = std::fs::read_dir(data.join("rix/themes")) else {
            continue;
        };
        let mut roots: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
        roots.sort();
        for root in roots {
            if let Ok(package) = ThemePackage::read(root) {
                packages.entry(package.info.id.clone()).or_insert(package);
            }
        }
    }
    packages.into_values().collect()
}
pub fn discover() -> Vec<ThemePackage> {
    discover_in(&data_roots())
}
pub fn find(id: &str) -> Result<ThemePackage, ConfigError> {
    discover()
        .into_iter()
        .find(|package| package.info.id == id)
        .ok_or_else(|| ConfigError::InvalidValue(format!("Theme package {id} is not installed")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bundled_layout_preserves_application_choices_and_input_settings() {
        let root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/rix/themes/fairywren-dark");
        let mut package = ThemePackage::read(root).unwrap();
        package.info.required_assets.clear();
        let mut current = LayoutConfig::default_layout();
        current.launcher.favorites = vec!["org.example.Editor".into()];
        current.keyboard.repeat_delay = 730;
        current.panels[0].thickness = 90;
        let changed = package.load(&current, true).unwrap();
        assert_eq!(changed.layout.panels[0].thickness, 46);
        assert_eq!(changed.layout.launcher.width, 697);
        assert_eq!(
            changed.layout.launcher.favorites,
            current.launcher.favorites
        );
        assert_eq!(changed.layout.keyboard, current.keyboard);
        assert_eq!(changed.layout.services, current.services);
        assert_eq!(package.load(&current, false).unwrap().layout, current);
        assert_eq!(changed.theme.icon_theme, "Rix-FairyWren-Dark");
    }
    #[test]
    fn invalid_packages_cannot_leave_the_package_root() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("manifest.json"), r#"{"version":1,"id":"bad","name":"Bad","description":"","theme":"../../theme.json","layout":null}"#).unwrap();
        assert!(ThemePackage::read(directory.path().to_path_buf()).is_err());
    }
}
