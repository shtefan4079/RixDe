use rix_config::ConfigManager;

use crate::TaskbarItemData;

pub fn pinned_file_path(config: &ConfigManager) -> std::path::PathBuf {
    config.config_dir().join("pinned.json")
}

pub fn load_pinned_execs(config: &ConfigManager) -> Vec<String> {
    std::fs::read_to_string(pinned_file_path(config))
        .ok()
        .and_then(|c| serde_json::from_str::<Vec<String>>(&c).ok())
        .unwrap_or_default()
}

pub fn save_pinned_execs(config: &ConfigManager, execs: &[String]) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(execs)?;
    let path = pinned_file_path(config);
    std::fs::create_dir_all(config.config_dir())?;
    rix_config::write_atomic(&path, json.as_bytes())?;
    Ok(())
}

pub fn collect_pinned_execs(items: &[TaskbarItemData]) -> Vec<String> {
    items
        .iter()
        .filter(|i| i.is_pinned)
        .map(|i| i.app_id.to_string())
        .collect()
}
