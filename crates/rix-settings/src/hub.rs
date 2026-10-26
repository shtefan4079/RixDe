use anyhow::{Context, Result};
use rix_config::model::ThemeSettings;
use rix_config::{parse_layout_kdl, ConfigManager};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use tracing::info;
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeComponentsSelection {
    pub panel: bool,
    pub colors: bool,
    pub icons: bool,
    pub cursor: bool,
}

impl Default for ThemeComponentsSelection {
    fn default() -> Self {
        Self {
            panel: true,
            colors: true,
            icons: true,
            cursor: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeManifest {
    pub name: String,
    pub created_at: String,
    pub components: ThemeComponentsSelection,
    #[serde(default = "default_manifest_version")]
    pub version: u32,
}

fn default_manifest_version() -> u32 {
    1
}

pub fn sanitize_theme_name(raw: &str) -> String {
    let trimmed = raw.trim();
    let mut clean: String = trimmed
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();

    while clean.contains("__") {
        clean = clean.replace("__", "_");
    }
    clean = clean.trim_matches('_').to_string();
    if clean.is_empty() {
        return "RixTheme".to_string();
    }
    clean.truncate(64);

    clean = clean.replace('.', "_");
    if clean.is_empty() {
        "RixTheme".to_string()
    } else {
        clean
    }
}

pub fn resolve_export_path(export_dir: &Path, theme_name: &str) -> PathBuf {
    let safe_name = sanitize_theme_name(theme_name);
    let candidate = export_dir.join(format!("{}.zip", safe_name));
    debug_assert!(candidate.starts_with(export_dir));
    candidate
}

pub fn resolve_import_path(export_dir: &Path, archive_name: &str) -> Option<PathBuf> {
    let file_name = Path::new(archive_name).file_name()?.to_str()?;
    if file_name.contains('/') || file_name.contains('\\') || file_name.contains('\0') {
        return None;
    }
    let lower = file_name.to_ascii_lowercase();
    if !lower.ends_with(".zip") {
        return None;
    }
    let candidate = export_dir.join(file_name);
    if !candidate.starts_with(export_dir) {
        return None;
    }
    Some(candidate)
}

fn clamp_theme(t: &mut ThemeSettings) {
    t.opacity = t.opacity.clamp(0.0, 1.0);
    t.blur_radius = t.blur_radius.min(128);
    t.noise_opacity = t.noise_opacity.clamp(0.0, 0.5);
    t.cursor_size = t.cursor_size.clamp(16, 96);
    if !is_valid_hex_color(&t.accent_color) {
        t.accent_color = "#38bdf8".to_string();
    }
}

fn is_valid_hex_color(s: &str) -> bool {
    let hex = s.strip_prefix('#').unwrap_or(s);
    (hex.len() == 6 || hex.len() == 8) && hex.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn get_themes_export_dir() -> PathBuf {
    let base = if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join("RixThemes")
    } else {
        PathBuf::from("/tmp/RixThemes")
    };
    if let Err(e) = fs::create_dir_all(&base) {
        tracing::warn!("Failed to create themes dir {:?}: {}", base, e);
    }
    base
}

pub fn export_theme_zip(
    dest_path: &Path,
    name: &str,
    selection: ThemeComponentsSelection,
    current_theme: &ThemeSettings,
    config_manager: &ConfigManager,
) -> Result<()> {
    let export_dir = get_themes_export_dir();
    export_theme_zip_in_dir(
        &export_dir,
        dest_path,
        name,
        selection,
        current_theme,
        config_manager,
    )
}

fn export_theme_zip_in_dir(
    export_dir: &Path,
    dest_path: &Path,
    name: &str,
    selection: ThemeComponentsSelection,
    _current_theme: &ThemeSettings,
    config_manager: &ConfigManager,
) -> Result<()> {

    if dest_path.parent() != Some(export_dir)
        || dest_path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        anyhow::bail!("Refusing to export outside themes dir: {:?}", dest_path);
    }
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)?;
    }

    if let Ok(metadata) = fs::symlink_metadata(dest_path) {
        anyhow::ensure!(
            metadata.is_file(),
            "Export destination must be a regular file"
        );
    }
    let temporary = tempfile::NamedTempFile::new_in(export_dir)?;
    let (current_theme, raw_layout) = config_manager.appearance_snapshot()?;
    let mut zip = ZipWriter::new(temporary);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let manifest = ThemeManifest {
        name: sanitize_theme_name(name),
        created_at: chrono::Utc::now().to_rfc3339(),
        components: selection.clone(),
        version: 1,
    };
    let manifest_str = serde_json::to_string_pretty(&manifest)?;
    zip.start_file("manifest.json", options)?;
    zip.write_all(manifest_str.as_bytes())?;

    let theme_data = serde_json::to_string_pretty(&current_theme)?;
    zip.start_file("theme.json", options)?;
    zip.write_all(theme_data.as_bytes())?;

    if selection.panel {
        zip.start_file("layout.kdl", options)?;
        zip.write_all(raw_layout.as_bytes())?;
    }

    let completed = zip.finish()?;
    completed.as_file().sync_all()?;
    completed.persist(dest_path).map_err(|error| error.error)?;
    File::open(export_dir)?.sync_all()?;
    info!("Theme exported successfully to {:?}", dest_path);
    Ok(())
}

pub fn import_theme_zip(zip_path: &Path, config_manager: &ConfigManager) -> Result<ThemeManifest> {

    read_theme_archive(zip_path, config_manager)
}

#[cfg(test)]
fn import_theme_zip_in_dir(
    export_dir: &Path,
    zip_path: &Path,
    config_manager: &ConfigManager,
) -> Result<ThemeManifest> {

    if !zip_path.starts_with(export_dir) {
        anyhow::bail!("Refusing to import outside themes dir: {:?}", zip_path);
    }
    read_theme_archive(zip_path, config_manager)
}

fn read_theme_archive(zip_path: &Path, config_manager: &ConfigManager) -> Result<ThemeManifest> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(zip_path)
        .with_context(|| format!("Failed to open zip archive {:?}", zip_path))?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "Theme archive must be a regular file"
    );
    let mut archive = ZipArchive::new(file)?;
    anyhow::ensure!(archive.len() <= 128, "Theme archive has too many entries");

    let mut manifest_raw: Option<String> = None;
    let mut theme_content: Option<String> = None;
    let mut panel_content: Option<String> = None;

    for i in 0..archive.len() {
        let f = archive.by_index(i)?;

        let raw_name = f.name().to_string();
        let short = Path::new(&raw_name)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if short.is_empty() || raw_name.contains("..") {
            continue;
        }
        if !matches!(
            short,
            "manifest.json" | "theme.json" | "layout.kdl" | "panel.kdl"
        ) {
            continue;
        }

        let mut buf = String::new();
        f.take(1024 * 1024 + 1).read_to_string(&mut buf)?;
        anyhow::ensure!(buf.len() <= 1024 * 1024, "Theme entry exceeds 1 MiB");

        match short {
            "manifest.json" => {
                manifest_raw = Some(buf);
            }
            "theme.json" => {
                theme_content = Some(buf);
            }

            "layout.kdl" | "panel.kdl" if panel_content.is_none() || short == "layout.kdl" => {
                panel_content = Some(buf);
            }
            _ => {}
        }
    }

    let manifest = match manifest_raw {
        Some(raw) => serde_json::from_str::<ThemeManifest>(&raw)
            .with_context(|| "Invalid manifest.json in theme archive")?,
        None => ThemeManifest {
            name: zip_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Imported Theme")
                .to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            components: ThemeComponentsSelection::default(),
            version: 1,
        },
    };

    let imported_theme = theme_content
        .as_deref()
        .map(|data| {
            serde_json::from_str::<ThemeSettings>(data)
                .context("Invalid theme.json in theme archive")
        })
        .transpose()?;
    let imported_layout = if manifest.components.panel {
        panel_content
            .as_deref()
            .map(|kdl| parse_layout_kdl(kdl).context("Invalid layout.kdl in theme archive"))
            .transpose()?
    } else {
        None
    };

    let base_theme = config_manager.get_theme();
    let updated_theme = imported_theme.map(|mut imported_theme| {
        clamp_theme(&mut imported_theme);
        let mut current = base_theme.clone();
        if manifest.components.colors {
            current.name = imported_theme.name;
            current.accent_color = imported_theme.accent_color;
            current.opacity = imported_theme.opacity;
            current.blur_radius = imported_theme.blur_radius;
            current.noise_opacity = imported_theme.noise_opacity;
            current.effects = imported_theme.effects;
            current.appearance = imported_theme.appearance;
        }
        if manifest.components.icons {
            current.icon_theme = imported_theme.icon_theme;
        }
        if manifest.components.cursor {
            current.cursor_theme = imported_theme.cursor_theme;
            current.cursor_size = imported_theme.cursor_size;
        }
        clamp_theme(&mut current);
        current
    });
    match (updated_theme, imported_layout) {
        (Some(theme), Some(layout)) => {
            config_manager.edit_theme_and_layout(&base_theme, &theme, layout)?;
        }
        (Some(theme), None) => {
            config_manager.edit_theme(&base_theme, &theme)?;
        }
        (None, Some(layout)) => config_manager.save_layout(layout)?,
        (None, None) => {}
    }

    info!("Theme '{}' imported successfully!", manifest.name);
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_export_keeps_previous_archive() {
        let directory = tempfile::tempdir().unwrap();
        let config = ConfigManager::init_in(directory.path().join("config")).unwrap();
        let export = directory.path().join("themes");
        fs::create_dir(&export).unwrap();
        let path = export.join("old.zip");
        fs::write(&path, b"previous valuable export").unwrap();
        fs::remove_file(config.config_dir().join("layout.kdl")).unwrap();
        assert!(export_theme_zip_in_dir(
            &export,
            &path,
            "old",
            ThemeComponentsSelection::default(),
            &config.get_theme(),
            &config
        )
        .is_err());
        assert_eq!(fs::read(path).unwrap(), b"previous valuable export");
        assert_eq!(fs::read_dir(export).unwrap().count(), 1);
    }

    #[test]
    fn import_io_failure_keeps_previous_theme_and_memory() {
        let directory = tempfile::tempdir().unwrap();
        let config = ConfigManager::init_in(directory.path().join("config")).unwrap();
        let source = ConfigManager::init_in(directory.path().join("source")).unwrap();
        source.save_theme(ThemeSettings::crisp_light()).unwrap();
        let export = directory.path().join("themes");
        let path = export.join("new.zip");
        export_theme_zip_in_dir(
            &export,
            &path,
            "new",
            ThemeComponentsSelection::default(),
            &ThemeSettings::crisp_light(),
            &source,
        )
        .unwrap();
        let original_theme = config.get_theme();
        let original_layout = config.get_layout();
        let original_bytes = fs::read(config.config_dir().join("theme.json")).unwrap();
        fs::remove_file(config.config_dir().join("layout.kdl")).unwrap();
        fs::create_dir(config.config_dir().join("layout.kdl")).unwrap();
        assert!(import_theme_zip(&path, &config).is_err());
        assert_eq!(config.get_theme(), original_theme);
        assert_eq!(config.get_layout(), original_layout);
        assert_eq!(
            fs::read(config.config_dir().join("theme.json")).unwrap(),
            original_bytes
        );
    }

    #[test]
    fn export_reads_one_disk_revision_instead_of_the_ui_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let config = ConfigManager::init_in(directory.path().join("config")).unwrap();
        let stale = config.get_theme();
        let mut layout = config.get_layout();
        layout.panels[0].thickness = 64;
        let latest = ThemeSettings::crisp_light();
        config
            .save_theme_and_layout(latest.clone(), layout.clone())
            .unwrap();
        let export = directory.path().join("themes");
        let path = export.join("latest.zip");
        export_theme_zip_in_dir(
            &export,
            &path,
            "latest",
            ThemeComponentsSelection::default(),
            &stale,
            &config,
        )
        .unwrap();
        let mut archive = ZipArchive::new(File::open(path).unwrap()).unwrap();
        let mut theme = String::new();
        archive
            .by_name("theme.json")
            .unwrap()
            .read_to_string(&mut theme)
            .unwrap();
        assert_eq!(
            serde_json::from_str::<ThemeSettings>(&theme).unwrap(),
            latest
        );
        let mut raw_layout = String::new();
        archive
            .by_name("layout.kdl")
            .unwrap()
            .read_to_string(&mut raw_layout)
            .unwrap();
        assert_eq!(parse_layout_kdl(&raw_layout).unwrap(), layout);
    }

    #[test]
    fn importing_a_fifo_is_rejected_before_waiting_for_a_writer() {
        let directory = tempfile::tempdir().unwrap();
        let config = ConfigManager::init_in(directory.path().join("config")).unwrap();
        let path = directory.path().join("theme.zip");
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(import_theme_zip(&path, &config).is_err());
    }

    #[test]
    fn test_export_and_import_theme_zip() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let export_dir = temp_dir.path().join("themes");
        let zip_path = export_dir.join("TestAero.zip");
        let config = ConfigManager::init_in(temp_dir.path().join("config")).unwrap();
        let theme = ThemeSettings::cyber_dark();
        config.save_theme(theme.clone()).unwrap();
        let mut layout = config.get_layout();
        layout.panels[0].thickness = 58;
        config.save_layout(layout.clone()).unwrap();
        let initial_display = config.get_display();
        let selection = ThemeComponentsSelection::default();
        export_theme_zip_in_dir(
            &export_dir,
            &zip_path,
            "TestAero",
            selection,
            &theme,
            &config,
        )
        .unwrap();
        let archive = ZipArchive::new(File::open(&zip_path).unwrap()).unwrap();
        let entries: Vec<_> = archive.file_names().collect();
        assert!(entries.contains(&"theme.json"));
        assert!(entries.contains(&"layout.kdl"));
        assert!(!entries.contains(&"display.json"));

        config.save_theme(ThemeSettings::aero_glass()).unwrap();
        config
            .save_layout(rix_config::LayoutConfig::default_layout())
            .unwrap();
        import_theme_zip_in_dir(&export_dir, &zip_path, &config).unwrap();
        assert_eq!(config.get_theme(), theme);
        assert_eq!(config.get_layout(), layout);
        assert_eq!(config.get_display(), initial_display);
    }

    #[test]
    fn invalid_import_keeps_current_theme() {
        let temp_dir = tempfile::tempdir().unwrap();
        let export_dir = temp_dir.path().join("themes");
        fs::create_dir_all(&export_dir).unwrap();
        let zip_path = export_dir.join("broken.zip");
        let config = ConfigManager::init_in(temp_dir.path().join("config")).unwrap();
        let original_theme = config.get_theme();
        let file = File::create(&zip_path).unwrap();
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        zip.start_file("theme.json", options).unwrap();
        zip.write_all(
            serde_json::to_string(&ThemeSettings::cyber_dark())
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        zip.start_file("layout.kdl", options).unwrap();
        zip.write_all(b"panel {").unwrap();
        zip.finish().unwrap();

        assert!(import_theme_zip_in_dir(&export_dir, &zip_path, &config).is_err());
        assert_eq!(config.get_theme(), original_theme);
    }

    #[test]
    fn test_sanitize_theme_name_blocks_traversal() {
        assert_eq!(sanitize_theme_name("../../.bashrc"), "bashrc");
        assert_eq!(sanitize_theme_name(""), "RixTheme");
        assert_eq!(sanitize_theme_name("My Theme!"), "My_Theme");
        let export_dir = PathBuf::from("/tmp/RixThemes");
        let p = resolve_export_path(&export_dir, "../../etc/passwd");
        assert!(p.starts_with(&export_dir));
        assert!(resolve_import_path(&export_dir, "../../etc/passwd").is_none());
        assert!(resolve_import_path(&export_dir, "theme.txt").is_none());
        assert!(resolve_import_path(&export_dir, "MyTheme.ZIP").is_some());
    }
}
