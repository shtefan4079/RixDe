use crate::model::{
    migrate_schema_version, DisplaySettings, LayoutConfig, SchemaVersion, ThemeSettings,
    CURRENT_SCHEMA_VERSION,
};
use crate::parser::{format_layout_kdl, layout_schema_version, parse_layout_kdl, ConfigError};
use crate::DesktopSettings;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::{
    fd::AsRawFd,
    unix::fs::{MetadataExt, OpenOptionsExt},
};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex, RwLock,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use tracing::{error, info, warn};

const DEBOUNCE_WINDOW: Duration = Duration::from_millis(200);

const RELOAD_LOCK_BUDGET: Duration = Duration::from_millis(200);

const RELOAD_LOCK_RETRY: Duration = Duration::from_millis(10);

const RELOAD_RETRY_DELAY: Duration = Duration::from_millis(400);

const WORKER_POLL: Duration = Duration::from_millis(25);

const WATCHED_DOCUMENTS: [&str; 4] = ["layout.kdl", "theme.json", "display.json", "desktop.json"];

pub struct ConfigManager {
    config_dir: PathBuf,
    layout_file: PathBuf,
    theme_file: PathBuf,
    display_file: PathBuf,
    desktop_file: PathBuf,
    current_desktop: Arc<RwLock<DesktopSettings>>,
    desktop_sender: broadcast::Sender<DesktopSettings>,
    current_layout: Arc<RwLock<LayoutConfig>>,
    current_theme: Arc<RwLock<ThemeSettings>>,
    current_display: Arc<RwLock<DisplaySettings>>,
    layout_sender: broadcast::Sender<LayoutConfig>,
    theme_sender: broadcast::Sender<ThemeSettings>,
    display_sender: broadcast::Sender<DisplaySettings>,
    save_lock: Arc<Mutex<()>>,

    readonly_documents: Arc<Mutex<BTreeSet<PathBuf>>>,

    corrupt_backups: Arc<CorruptLog>,

    debounce: Option<DebounceWorker>,
    _watcher: Option<RecommendedWatcher>,
}

impl ConfigManager {

    pub fn init() -> Result<Self, ConfigError> {
        let has_config_home =
            std::env::var_os("XDG_CONFIG_HOME").is_some_and(|path| !path.is_empty());
        let has_home = std::env::var_os("HOME").is_some_and(|path| !path.is_empty());
        let path = dirs_or_fallback();
        if !has_config_home && !has_home {
            ensure_private_fallback(&path)?;
        }
        Self::init_with_theme(path, None)
    }

    pub fn init_in(config_dir: PathBuf) -> Result<Self, ConfigError> {
        Self::init_with_theme(config_dir, None)
    }

    fn init_with_theme(
        config_dir: PathBuf,
        first_theme: Option<ThemeSettings>,
    ) -> Result<Self, ConfigError> {
        fs::create_dir_all(&config_dir)?;
        let _disk_guard = AppearanceLock::acquire(&config_dir)?;

        recover_appearance(&config_dir)?;

        let readonly_documents = Arc::new(Mutex::new(BTreeSet::new()));
        let corrupt_backups = Arc::new(CorruptLog::default());

        let layout_file = config_dir.join("layout.kdl");
        let current_layout = if layout_file.exists() {
            match fs::read_to_string(&layout_file) {
                Ok(content) => match layout_schema_version(&content) {
                    Ok(found) if found > CURRENT_SCHEMA_VERSION => {
                        warn!(
                            "{:?} uses schema {found}, newer than this build (schema {CURRENT_SCHEMA_VERSION}); using the default layout and refusing to overwrite the file",
                            layout_file
                        );
                        readonly_documents
                            .lock()
                            .unwrap()
                            .insert(layout_file.clone());
                        LayoutConfig::default_layout()
                    }
                    Ok(_) => match parse_layout_kdl(&content) {
                        Ok(layout) => {
                            info!("Loaded configuration from {:?}", layout_file);
                            layout
                        }
                        Err(error) => {
                            corrupt_backups.preserve(
                                &layout_file,
                                Some(content.as_bytes()),
                                &format!("Failed to parse {:?}: {error}", layout_file),
                            );
                            LayoutConfig::default_layout()
                        }
                    },
                    Err(error) => {
                        corrupt_backups.preserve(
                            &layout_file,
                            Some(content.as_bytes()),
                            &format!("Failed to parse {:?}: {error}", layout_file),
                        );
                        LayoutConfig::default_layout()
                    }
                },
                Err(error) => {
                    corrupt_backups.preserve(
                        &layout_file,
                        None,
                        &format!("Failed to read {:?}: {error}", layout_file),
                    );
                    LayoutConfig::default_layout()
                }
            }
        } else {
            let default_layout = LayoutConfig::default_layout();
            let kdl_str = format_layout_kdl(&default_layout);
            if let Err(error) = write_atomic(&layout_file, kdl_str.as_bytes()) {
                warn!("Could not create default {:?}: {error}", layout_file);
            } else {
                info!("Created default configuration at {:?}", layout_file);
            }
            default_layout
        };

        let theme_file = config_dir.join("theme.json");
        let current_theme = if theme_file.exists() {
            match original_document(&theme_file) {
                Ok(Some(bytes)) => match parse_json_document::<ThemeSettings>(
                    &theme_file,
                    &bytes,
                    ThemeSettings::validate,
                ) {
                    Ok(DocumentLoad::Loaded(theme)) => theme,
                    Ok(DocumentLoad::FutureVersion(found)) => {
                        warn!(
                            "{:?} uses schema {found}, newer than this build (schema {CURRENT_SCHEMA_VERSION}); using the default theme and refusing to overwrite the file",
                            theme_file
                        );
                        readonly_documents
                            .lock()
                            .unwrap()
                            .insert(theme_file.clone());
                        ThemeSettings::default()
                    }
                    Err(error) => {
                        corrupt_backups.preserve(
                            &theme_file,
                            Some(&bytes),
                            &format!("Failed to load {:?}: {error}", theme_file),
                        );
                        ThemeSettings::default()
                    }
                },
                Ok(None) => ThemeSettings::default(),
                Err(error) => {
                    corrupt_backups.preserve(
                        &theme_file,
                        None,
                        &format!("Failed to read {:?}: {error}", theme_file),
                    );
                    ThemeSettings::default()
                }
            }
        } else {
            let def_theme = first_theme.unwrap_or_default();
            match serde_json::to_string_pretty(&def_theme) {
                Ok(serialized) => {
                    if let Err(error) = write_atomic(&theme_file, serialized.as_bytes()) {
                        warn!("Could not create default {:?}: {error}", theme_file);
                    }
                }
                Err(error) => warn!("Could not serialize the default theme: {error}"),
            }
            def_theme
        };

        let display_file = config_dir.join("display.json");
        let current_display = if display_file.exists() {
            match original_document(&display_file) {
                Ok(Some(bytes)) => {
                    match parse_json_document::<DisplaySettings>(
                        &display_file,
                        &bytes,
                        validate_display,
                    ) {
                        Ok(DocumentLoad::Loaded(display)) => display,
                        Ok(DocumentLoad::FutureVersion(found)) => {
                            warn!(
                                "{:?} uses schema {found}, newer than this build (schema {CURRENT_SCHEMA_VERSION}); using default display settings and refusing to overwrite the file",
                                display_file
                            );
                            readonly_documents
                                .lock()
                                .unwrap()
                                .insert(display_file.clone());
                            DisplaySettings::default()
                        }
                        Err(error) => {
                            corrupt_backups.preserve(
                                &display_file,
                                Some(&bytes),
                                &format!("Failed to load {:?}: {error}", display_file),
                            );
                            DisplaySettings::default()
                        }
                    }
                }
                Ok(None) => DisplaySettings::default(),
                Err(error) => {
                    corrupt_backups.preserve(
                        &display_file,
                        None,
                        &format!("Failed to read {:?}: {error}", display_file),
                    );
                    DisplaySettings::default()
                }
            }
        } else {
            let def_disp = DisplaySettings::default();
            match serde_json::to_string_pretty(&def_disp) {
                Ok(serialized) => {
                    if let Err(error) = write_atomic(&display_file, serialized.as_bytes()) {
                        warn!("Could not create default {:?}: {error}", display_file);
                    }
                }
                Err(error) => warn!("Could not serialize default display settings: {error}"),
            }
            def_disp
        };

        let desktop_file = config_dir.join("desktop.json");
        let current_desktop = match original_document(&desktop_file) {
            Ok(Some(bytes)) => match parse_json_document::<DesktopSettings>(
                &desktop_file,
                &bytes,
                DesktopSettings::validate,
            ) {
                Ok(DocumentLoad::Loaded(desktop)) => desktop,
                Ok(DocumentLoad::FutureVersion(found)) => {
                    warn!(
                        "{:?} uses schema {found}, newer than this build (schema {CURRENT_SCHEMA_VERSION}); using default desktop settings and refusing to overwrite the file",
                        desktop_file
                    );
                    readonly_documents
                        .lock()
                        .unwrap()
                        .insert(desktop_file.clone());
                    DesktopSettings::default()
                }
                Err(error) => {
                    corrupt_backups.preserve(
                        &desktop_file,
                        Some(&bytes),
                        &format!("Failed to load {:?}: {error}", desktop_file),
                    );
                    DesktopSettings::default()
                }
            },
            Ok(None) => DesktopSettings::default(),
            Err(error) => {
                corrupt_backups.preserve(
                    &desktop_file,
                    None,
                    &format!("Failed to read {:?}: {error}", desktop_file),
                );
                DesktopSettings::default()
            }
        };
        let current_desktop = Arc::new(RwLock::new(current_desktop));
        let (desktop_sender, _) = broadcast::channel(16);

        let current_layout = Arc::new(RwLock::new(current_layout));
        let current_theme = Arc::new(RwLock::new(current_theme));
        let current_display = Arc::new(RwLock::new(current_display));
        let (layout_sender, _) = broadcast::channel(16);
        let (theme_sender, _) = broadcast::channel(16);
        let (display_sender, _) = broadcast::channel(16);

        let mut manager = Self {
            config_dir,
            layout_file,
            theme_file,
            display_file,
            desktop_file,
            current_desktop,
            desktop_sender,
            current_layout,
            current_theme,
            current_display,
            layout_sender,
            theme_sender,
            display_sender,
            save_lock: Arc::new(Mutex::new(())),
            readonly_documents,
            corrupt_backups,
            debounce: None,
            _watcher: None,
        };

        manager.setup_watcher();
        Ok(manager)
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn get_layout(&self) -> LayoutConfig {
        self.current_layout.read().unwrap().clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LayoutConfig> {
        self.layout_sender.subscribe()
    }

    pub fn get_effects(&self) -> crate::appearance::MaterialEffects {
        let theme = self.current_theme.read().unwrap();
        crate::appearance::MaterialEffects::from(&*theme)
    }

    pub fn get_theme(&self) -> ThemeSettings {
        self.current_theme.read().unwrap().clone()
    }

    pub fn subscribe_theme(&self) -> broadcast::Receiver<ThemeSettings> {
        self.theme_sender.subscribe()
    }

    pub fn save_theme(&self, theme: ThemeSettings) -> Result<(), ConfigError> {
        let _save_guard = self.save_lock.lock().unwrap();
        self.write_theme(theme)
    }

    pub fn edit_theme(
        &self,
        base: &ThemeSettings,
        updated: &ThemeSettings,
    ) -> Result<ThemeSettings, ConfigError> {
        let _save_guard = self.save_lock.lock().unwrap();
        let _disk_guard = AppearanceLock::acquire(&self.config_dir)?;
        recover_appearance(&self.config_dir)?;
        let current = read_document(&self.theme_file, self.get_theme())?;
        let merged = crate::edit::merge_document(base, updated, &current)?;
        self.write_theme_locked(merged.clone())?;
        Ok(merged)
    }

    pub fn appearance_snapshot(&self) -> Result<(ThemeSettings, String), ConfigError> {
        let _save_guard = self.save_lock.lock().unwrap();
        let _disk_guard = AppearanceLock::acquire(&self.config_dir)?;
        recover_appearance(&self.config_dir)?;
        let required = |path: &Path| {
            original_document(path)?.ok_or_else(|| {
                ConfigError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("Appearance document is missing: {}", path.display()),
                ))
            })
        };
        let theme: ThemeSettings = serde_json::from_slice(&required(&self.theme_file)?)
            .map_err(|error| ConfigError::InvalidValue(error.to_string()))?;
        theme.validate()?;
        let layout = String::from_utf8(required(&self.layout_file)?)
            .map_err(|error| ConfigError::InvalidValue(error.to_string()))?;
        validated_layout(&parse_layout_kdl(&layout)?)?;
        Ok((theme, layout))
    }

    pub fn edit_theme_and_layout(
        &self,
        base: &ThemeSettings,
        updated: &ThemeSettings,
        layout: LayoutConfig,
    ) -> Result<crate::themes::AppearanceSnapshot, ConfigError> {
        let _save_guard = self.save_lock.lock().unwrap();
        let _disk_guard = AppearanceLock::acquire(&self.config_dir)?;
        recover_appearance(&self.config_dir)?;
        let current = read_document(&self.theme_file, self.get_theme())?;
        let theme = crate::edit::merge_document(base, updated, &current)?;
        let snapshot = crate::themes::AppearanceSnapshot { theme, layout };
        self.commit_appearance_locked(snapshot.clone())?;
        Ok(snapshot)
    }

    pub fn apply_theme_package(
        &self,
        id: &str,
        include_layout: bool,
    ) -> Result<crate::themes::AppearanceSnapshot, ConfigError> {
        let package = crate::themes::find(id)?;
        let _guard = self.save_lock.lock().unwrap();
        let _disk_guard = AppearanceLock::acquire(&self.config_dir)?;
        recover_appearance(&self.config_dir)?;
        let current = match fs::read_to_string(&self.layout_file) {
            Ok(content) => parse_layout_kdl(&content)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => self.get_layout(),
            Err(error) => return Err(error.into()),
        };
        let snapshot = package.load(&current, include_layout)?;
        self.commit_appearance_locked(snapshot.clone())?;
        Ok(snapshot)
    }

    pub fn save_theme_and_layout(
        &self,
        theme: ThemeSettings,
        layout: LayoutConfig,
    ) -> Result<(), ConfigError> {
        let _guard = self.save_lock.lock().unwrap();
        self.commit_appearance(crate::themes::AppearanceSnapshot { theme, layout })
    }

    fn commit_appearance(
        &self,
        snapshot: crate::themes::AppearanceSnapshot,
    ) -> Result<(), ConfigError> {
        let _disk_guard = AppearanceLock::acquire(&self.config_dir)?;
        recover_appearance(&self.config_dir)?;
        self.commit_appearance_locked(snapshot)
    }

    fn commit_appearance_locked(
        &self,
        snapshot: crate::themes::AppearanceSnapshot,
    ) -> Result<(), ConfigError> {
        self.ensure_writable(&self.theme_file)?;
        self.ensure_writable(&self.layout_file)?;
        snapshot.theme.validate()?;
        let layout_data = validated_layout(&snapshot.layout)?;
        let theme_data = serde_json::to_vec_pretty(&snapshot.theme)
            .map_err(|error| ConfigError::InvalidValue(error.to_string()))?;
        let theme_before = original_document_with_mode(&self.theme_file)?;
        let layout_before = original_document_with_mode(&self.layout_file)?;
        let rollback = AppearanceRollback {
            version: 1,
            theme: theme_before.as_ref().map(|(bytes, _)| bytes.clone()),
            layout: layout_before.as_ref().map(|(bytes, _)| bytes.clone()),
            theme_mode: theme_before.map(|(_, mode)| mode),
            layout_mode: layout_before.map(|(_, mode)| mode),
        };
        let theme = PreparedDocument::new(&self.theme_file, &theme_data)?;
        let layout = PreparedDocument::new(&self.layout_file, layout_data.as_bytes())?;
        let journal = self.config_dir.join("appearance-transaction.json");
        let journal_bytes = serde_json::to_vec(&rollback)
            .map_err(|error| ConfigError::InvalidValue(error.to_string()))?;
        write_atomic(&journal, &journal_bytes)?;
        let commit = (|| {
            theme.publish()?;
            layout.publish()?;
            fs::File::open(&self.config_dir)?.sync_all()?;
            remove_journal(&journal)?;
            fs::File::open(&self.config_dir)?.sync_all()?;
            Ok::<(), ConfigError>(())
        })();
        if let Err(error) = commit {
            if let Err(rollback_error) =
                rollback_failed_appearance(&self.config_dir, &rollback, &journal_bytes)
            {
                return Err(ConfigError::InvalidValue(format!(
                    "Appearance commit failed: {error}; rollback requires recovery: {rollback_error}. Backup: {}",
                    journal.display()
                )));
            }
            return Err(error);
        }
        {
            let mut theme = self.current_theme.write().unwrap();
            let mut layout = self.current_layout.write().unwrap();
            *theme = snapshot.theme.clone();
            *layout = snapshot.layout.clone();
        }
        let _ = self.theme_sender.send(snapshot.theme.clone());
        let _ = self.layout_sender.send(snapshot.layout.clone());
        Ok(())
    }

    fn write_theme(&self, theme: ThemeSettings) -> Result<(), ConfigError> {
        let _disk_guard = AppearanceLock::acquire(&self.config_dir)?;
        recover_appearance(&self.config_dir)?;
        self.write_theme_locked(theme)
    }

    fn write_theme_locked(&self, theme: ThemeSettings) -> Result<(), ConfigError> {
        self.ensure_writable(&self.theme_file)?;
        theme.validate()?;
        let serialized = serde_json::to_string_pretty(&theme)
            .map_err(|e| ConfigError::InvalidValue(e.to_string()))?;
        write_atomic(&self.theme_file, serialized.as_bytes())?;
        *self.current_theme.write().unwrap() = theme.clone();
        let _ = self.theme_sender.send(theme);
        info!("Saved theme configuration to {:?}", self.theme_file);
        Ok(())
    }

    fn ensure_writable(&self, path: &Path) -> Result<(), ConfigError> {
        let readonly = self
            .readonly_documents
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if readonly.contains(path) {
            return Err(ConfigError::InvalidValue(format!(
                "{} was written by a newer RixDE build and is read-only",
                path.display()
            )));
        }
        Ok(())
    }

    pub fn get_display(&self) -> DisplaySettings {
        self.current_display.read().unwrap().clone()
    }

    pub fn subscribe_display(&self) -> broadcast::Receiver<DisplaySettings> {
        self.display_sender.subscribe()
    }

    pub fn save_display(&self, display: DisplaySettings) -> Result<(), ConfigError> {
        let _save_guard = self.save_lock.lock().unwrap();
        self.write_display(display)
    }

    pub fn save_output_profile(
        &self,
        output: String,
        profile: crate::OutputProfile,
    ) -> Result<(), ConfigError> {
        let _save_guard = self.save_lock.lock().unwrap();
        let mut display = self.get_display();
        display.outputs.insert(output, profile);
        self.write_display(display)
    }

    pub fn update_display(
        &self,
        update: impl FnOnce(&mut DisplaySettings),
    ) -> Result<(), ConfigError> {
        let _save_guard = self.save_lock.lock().unwrap();
        let _disk_guard = AppearanceLock::acquire(&self.config_dir)?;
        recover_appearance(&self.config_dir)?;
        let mut display = read_document(&self.display_file, self.get_display())?;
        update(&mut display);
        self.write_display_locked(display)
    }

    fn write_display(&self, display: DisplaySettings) -> Result<(), ConfigError> {

        let _disk_guard = AppearanceLock::acquire(&self.config_dir)?;
        recover_appearance(&self.config_dir)?;
        self.write_display_locked(display)
    }

    fn write_display_locked(&self, display: DisplaySettings) -> Result<(), ConfigError> {
        self.ensure_writable(&self.display_file)?;
        validate_display(&display)?;
        let serialized = serde_json::to_string_pretty(&display)
            .map_err(|e| ConfigError::InvalidValue(e.to_string()))?;
        write_atomic(&self.display_file, serialized.as_bytes())?;
        *self.current_display.write().unwrap() = display.clone();
        let _ = self.display_sender.send(display);
        info!("Saved display configuration to {:?}", self.display_file);
        Ok(())
    }

    pub fn save_layout(&self, layout: LayoutConfig) -> Result<(), ConfigError> {
        let _save_guard = self.save_lock.lock().unwrap();
        self.write_layout(layout)
    }

    pub fn edit_layout(
        &self,
        base: &LayoutConfig,
        updated: &LayoutConfig,
    ) -> Result<LayoutConfig, ConfigError> {
        let _save_guard = self.save_lock.lock().unwrap();
        let _disk_guard = AppearanceLock::acquire(&self.config_dir)?;
        recover_appearance(&self.config_dir)?;
        let current = match fs::read_to_string(&self.layout_file) {
            Ok(content) => parse_layout_kdl(&content)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => self.get_layout(),
            Err(error) => return Err(error.into()),
        };
        let merged = crate::edit::merge_layout(base, updated, &current)?;
        self.write_layout_locked(merged.clone())?;
        Ok(merged)
    }

    fn write_layout(&self, layout: LayoutConfig) -> Result<(), ConfigError> {
        let _disk_guard = AppearanceLock::acquire(&self.config_dir)?;
        recover_appearance(&self.config_dir)?;
        self.write_layout_locked(layout)
    }

    fn write_layout_locked(&self, layout: LayoutConfig) -> Result<(), ConfigError> {
        self.ensure_writable(&self.layout_file)?;
        let kdl_str = validated_layout(&layout)?;

        write_atomic(&self.layout_file, kdl_str.as_bytes())?;

        *self.current_layout.write().unwrap() = layout.clone();

        let _ = self.layout_sender.send(layout);
        info!("Saved layout configuration to {:?}", self.layout_file);
        Ok(())
    }

    pub fn get_desktop(&self) -> DesktopSettings {
        self.current_desktop.read().unwrap().clone()
    }
    pub fn subscribe_desktop(&self) -> broadcast::Receiver<DesktopSettings> {
        self.desktop_sender.subscribe()
    }
    pub fn save_desktop(&self, desktop: DesktopSettings) -> Result<(), ConfigError> {
        let _guard = self.save_lock.lock().unwrap();
        self.write_desktop(desktop)
    }

    pub fn edit_desktop(
        &self,
        base: &DesktopSettings,
        updated: &DesktopSettings,
    ) -> Result<DesktopSettings, ConfigError> {

        let _guard = self.save_lock.lock().unwrap();
        let _disk_guard = AppearanceLock::acquire(&self.config_dir)?;
        recover_appearance(&self.config_dir)?;
        let current = read_document(&self.desktop_file, self.get_desktop())?;
        let merged = crate::edit::merge_document(base, updated, &current)?;
        self.write_desktop_locked(merged.clone())?;
        Ok(merged)
    }

    fn write_desktop(&self, desktop: DesktopSettings) -> Result<(), ConfigError> {
        let _disk_guard = AppearanceLock::acquire(&self.config_dir)?;
        recover_appearance(&self.config_dir)?;
        self.write_desktop_locked(desktop)
    }

    fn write_desktop_locked(&self, desktop: DesktopSettings) -> Result<(), ConfigError> {
        self.ensure_writable(&self.desktop_file)?;
        desktop.validate()?;
        let content = serde_json::to_vec_pretty(&desktop)
            .map_err(|e| ConfigError::InvalidValue(e.to_string()))?;
        write_atomic(&self.desktop_file, &content)?;
        *self.current_desktop.write().unwrap() = desktop.clone();
        let _ = self.desktop_sender.send(desktop);
        Ok(())
    }

    fn setup_watcher(&mut self) {
        let paths = DocumentPaths {
            config_dir: self.config_dir.clone(),
            layout: self.layout_file.clone(),
            theme: self.theme_file.clone(),
            display: self.display_file.clone(),
            desktop: self.desktop_file.clone(),
        };
        let mut handler = ReloadHandler {
            paths,
            current_layout: self.current_layout.clone(),
            current_theme: self.current_theme.clone(),
            current_display: self.current_display.clone(),
            current_desktop: self.current_desktop.clone(),
            layout_sender: self.layout_sender.clone(),
            theme_sender: self.theme_sender.clone(),
            display_sender: self.display_sender.clone(),
            desktop_sender: self.desktop_sender.clone(),
            save_lock: self.save_lock.clone(),
            readonly_documents: self.readonly_documents.clone(),
            corrupt_backups: self.corrupt_backups.clone(),
            deferred: false,
        };
        let worker = DebounceWorker::spawn(DEBOUNCE_WINDOW, RELOAD_RETRY_DELAY, move |batch| {
            handler.reload(batch)
        });
        let events = worker.sender();

        let mut watcher =
            notify::recommended_watcher(move |res: Result<Event, notify::Error>| match res {
                Ok(event) => {
                    if !matches!(
                        event.kind,
                        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                    ) {
                        return;
                    }
                    for path in event.paths {
                        let watched = path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| WATCHED_DOCUMENTS.contains(&name));
                        if watched {
                            let _ = events.send(path);
                        }
                    }
                }
                Err(e) => error!("File watch error: {:?}", e),
            });

        if let Ok(ref mut w) = watcher {
            if let Err(e) = w.watch(&self.config_dir, RecursiveMode::NonRecursive) {
                warn!("Failed to watch {:?}: {:?}", self.config_dir, e);
            }
        }

        self._watcher = watcher.ok();
        self.debounce = Some(worker);
    }
}

#[derive(Debug, Clone)]
struct DocumentPaths {
    config_dir: PathBuf,
    layout: PathBuf,
    theme: PathBuf,
    display: PathBuf,
    desktop: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Document {
    Layout,
    Theme,
    Display,
    Desktop,
}

impl DocumentPaths {

    fn classify(&self, path: &Path) -> Option<Document> {
        if path.parent() != Some(self.config_dir.as_path()) {
            return None;
        }
        match path.file_name()?.to_str()? {
            "layout.kdl" => Some(Document::Layout),
            "theme.json" => Some(Document::Theme),
            "display.json" => Some(Document::Display),
            "desktop.json" => Some(Document::Desktop),
            _ => None,
        }
    }
}

enum ReloadOutcome {

    Done,

    Deferred,
}

struct DebounceWorker {
    events: mpsc::Sender<PathBuf>,
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DebounceWorker {
    fn spawn(
        window: Duration,
        retry_delay: Duration,
        mut handler: impl FnMut(&[PathBuf]) -> ReloadOutcome + Send + 'static,
    ) -> Self {
        let (events, receiver) = mpsc::channel::<PathBuf>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = shutdown.clone();
        let thread = std::thread::Builder::new()
            .name("rix-config-reload".into())
            .spawn(move || {

                let mut pending: Vec<PathBuf> = Vec::new();
                let mut deadline: Option<Instant> = None;
                while !thread_shutdown.load(Ordering::Acquire) {

                    let wait = deadline
                        .map(|fire_at| fire_at.saturating_duration_since(Instant::now()))
                        .unwrap_or(WORKER_POLL)
                        .min(WORKER_POLL)
                        .max(Duration::from_millis(1));
                    match receiver.recv_timeout(wait) {
                        Ok(path) => {
                            if deadline.is_none() {

                                deadline = Some(Instant::now() + window);
                            }
                            if !pending.contains(&path) {
                                pending.push(path);
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            let Some(fire_at) = deadline else { continue };
                            if Instant::now() < fire_at {
                                continue;
                            }
                            match handler(&pending) {
                                ReloadOutcome::Done => {
                                    pending.clear();
                                    deadline = None;
                                }
                                ReloadOutcome::Deferred => {
                                    deadline = Some(Instant::now() + retry_delay);
                                }
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            if !pending.is_empty() {
                                handler(&pending);
                            }
                            return;
                        }
                    }
                }
            })
            .expect("spawn configuration reload thread");
        Self {
            events,
            shutdown,
            thread: Some(thread),
        }
    }

    fn sender(&self) -> mpsc::Sender<PathBuf> {
        self.events.clone()
    }
}

impl Drop for DebounceWorker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct ReloadHandler {
    paths: DocumentPaths,
    current_layout: Arc<RwLock<LayoutConfig>>,
    current_theme: Arc<RwLock<ThemeSettings>>,
    current_display: Arc<RwLock<DisplaySettings>>,
    current_desktop: Arc<RwLock<DesktopSettings>>,
    layout_sender: broadcast::Sender<LayoutConfig>,
    theme_sender: broadcast::Sender<ThemeSettings>,
    display_sender: broadcast::Sender<DisplaySettings>,
    desktop_sender: broadcast::Sender<DesktopSettings>,
    save_lock: Arc<Mutex<()>>,
    readonly_documents: Arc<Mutex<BTreeSet<PathBuf>>>,
    corrupt_backups: Arc<CorruptLog>,

    deferred: bool,
}

impl ReloadHandler {
    fn reload(&mut self, batch: &[PathBuf]) -> ReloadOutcome {
        let documents: BTreeSet<Document> = batch
            .iter()
            .filter_map(|path| self.paths.classify(path))
            .collect();
        if documents.is_empty() {
            return ReloadOutcome::Done;
        }

        let _save_guard = self
            .save_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _disk_guard = match AppearanceLock::acquire_bounded(
            &self.paths.config_dir,
            RELOAD_LOCK_BUDGET,
        ) {
            Ok(Some(guard)) => guard,
            Ok(None) => {
                if !self.deferred {

                    warn!(
                        "Appearance lock is held by another process; retrying the configuration reload until it is released"
                    );
                    self.deferred = true;
                }
                return ReloadOutcome::Deferred;
            }
            Err(error) => {
                warn!(%error, "Could not take the appearance lock; skipping this reload");
                return ReloadOutcome::Done;
            }
        };
        self.deferred = false;
        if let Err(error) = recover_appearance(&self.paths.config_dir) {
            warn!(%error, "Appearance recovery pending; preserving active settings");
            return ReloadOutcome::Done;
        }
        if documents.contains(&Document::Layout) {
            self.reload_layout();
        }
        if documents.contains(&Document::Theme) {
            self.reload_theme();
        }
        if documents.contains(&Document::Display) {
            self.reload_display();
        }
        if documents.contains(&Document::Desktop) {
            self.reload_desktop();
        }
        ReloadOutcome::Done
    }

    fn reload_layout(&self) {
        let content = match fs::read_to_string(&self.paths.layout) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.unmark_readonly(&self.paths.layout);
                return;
            }
            Err(error) => {
                self.corrupt_backups.preserve(
                    &self.paths.layout,
                    None,
                    &format!("Failed to read layout.kdl: {error}"),
                );
                return;
            }
        };
        match layout_schema_version(&content) {
            Ok(found) if found > CURRENT_SCHEMA_VERSION => {
                self.mark_readonly(&self.paths.layout, found, "layout.kdl");
                publish_appearance(
                    &self.current_theme,
                    &self.current_layout,
                    &self.theme_sender,
                    &self.layout_sender,
                    None,
                    Some(LayoutConfig::default_layout()),
                );
            }
            Ok(_) => {
                self.unmark_readonly(&self.paths.layout);
                match parse_layout_kdl(&content) {
                    Ok(layout) => publish_appearance(
                        &self.current_theme,
                        &self.current_layout,
                        &self.theme_sender,
                        &self.layout_sender,
                        None,
                        Some(layout),
                    ),
                    Err(error) => {
                        warn!("Ignoring layout.kdl: {error}");
                        self.corrupt_backups.preserve(
                            &self.paths.layout,
                            Some(content.as_bytes()),
                            &format!("Invalid layout.kdl: {error}"),
                        );
                    }
                }
            }
            Err(error) => {
                warn!("Ignoring layout.kdl: {error}");
                self.corrupt_backups.preserve(
                    &self.paths.layout,
                    Some(content.as_bytes()),
                    &format!("Invalid layout.kdl: {error}"),
                );
            }
        }
    }

    fn reload_theme(&self) {
        match original_document(&self.paths.theme) {
            Ok(Some(bytes)) => {
                match parse_json_document::<ThemeSettings>(
                    &self.paths.theme,
                    &bytes,
                    ThemeSettings::validate,
                ) {
                    Ok(DocumentLoad::Loaded(theme)) => {
                        self.unmark_readonly(&self.paths.theme);
                        publish_appearance(
                            &self.current_theme,
                            &self.current_layout,
                            &self.theme_sender,
                            &self.layout_sender,
                            Some(theme),
                            None,
                        );
                    }
                    Ok(DocumentLoad::FutureVersion(found)) => {
                        self.mark_readonly(&self.paths.theme, found, "theme.json");
                        publish_appearance(
                            &self.current_theme,
                            &self.current_layout,
                            &self.theme_sender,
                            &self.layout_sender,
                            Some(ThemeSettings::default()),
                            None,
                        );
                    }
                    Err(error) => {
                        warn!("Ignoring theme.json: {error}");
                        self.corrupt_backups.preserve(
                            &self.paths.theme,
                            Some(&bytes),
                            &format!("Invalid theme.json: {error}"),
                        );
                    }
                }
            }
            Ok(None) => self.unmark_readonly(&self.paths.theme),
            Err(error) => {
                self.corrupt_backups.preserve(
                    &self.paths.theme,
                    None,
                    &format!("Failed to read theme.json: {error}"),
                );
            }
        }
    }

    fn reload_display(&self) {
        match original_document(&self.paths.display) {
            Ok(Some(bytes)) => {
                match parse_json_document::<DisplaySettings>(
                    &self.paths.display,
                    &bytes,
                    validate_display,
                ) {
                    Ok(DocumentLoad::Loaded(display)) => {
                        self.unmark_readonly(&self.paths.display);
                        let mut current = self.current_display.write().unwrap();
                        if *current != display {
                            *current = display.clone();
                            let _ = self.display_sender.send(display);
                        }
                    }
                    Ok(DocumentLoad::FutureVersion(found)) => {
                        self.mark_readonly(&self.paths.display, found, "display.json");
                        let default = DisplaySettings::default();
                        let mut current = self.current_display.write().unwrap();
                        if *current != default {
                            *current = default.clone();
                            let _ = self.display_sender.send(default);
                        }
                    }
                    Err(error) => {
                        warn!("Ignoring display.json: {error}");
                        self.corrupt_backups.preserve(
                            &self.paths.display,
                            Some(&bytes),
                            &format!("Invalid display.json: {error}"),
                        );
                    }
                }
            }
            Ok(None) => self.unmark_readonly(&self.paths.display),
            Err(error) => {
                self.corrupt_backups.preserve(
                    &self.paths.display,
                    None,
                    &format!("Failed to read display.json: {error}"),
                );
            }
        }
    }

    fn reload_desktop(&self) {
        match original_document(&self.paths.desktop) {
            Ok(Some(bytes)) => {
                match parse_json_document::<DesktopSettings>(
                    &self.paths.desktop,
                    &bytes,
                    DesktopSettings::validate,
                ) {
                    Ok(DocumentLoad::Loaded(desktop)) => {
                        self.unmark_readonly(&self.paths.desktop);
                        let mut current = self.current_desktop.write().unwrap();
                        if *current != desktop {
                            *current = desktop.clone();
                            let _ = self.desktop_sender.send(desktop);
                        }
                    }
                    Ok(DocumentLoad::FutureVersion(found)) => {
                        self.mark_readonly(&self.paths.desktop, found, "desktop.json");
                        let default = DesktopSettings::default();
                        let mut current = self.current_desktop.write().unwrap();
                        if *current != default {
                            *current = default.clone();
                            let _ = self.desktop_sender.send(default);
                        }
                    }
                    Err(error) => {
                        warn!("Ignoring desktop.json: {error}");
                        self.corrupt_backups.preserve(
                            &self.paths.desktop,
                            Some(&bytes),
                            &format!("Invalid desktop.json: {error}"),
                        );
                    }
                }
            }
            Ok(None) => self.unmark_readonly(&self.paths.desktop),
            Err(error) => {
                self.corrupt_backups.preserve(
                    &self.paths.desktop,
                    None,
                    &format!("Failed to read desktop.json: {error}"),
                );
            }
        }
    }

    fn mark_readonly(&self, path: &Path, found: u32, name: &str) {
        warn!(
            "{name} uses schema {found}, newer than this build (schema {CURRENT_SCHEMA_VERSION}); using defaults and refusing to overwrite the file"
        );
        self.readonly_documents
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(path.to_path_buf());
    }

    fn unmark_readonly(&self, path: &Path) {
        self.readonly_documents
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(path);
    }
}

#[derive(Default)]
struct CorruptLog {
    preserved: Mutex<HashMap<PathBuf, u64>>,
}

impl CorruptLog {
    fn preserve(&self, path: &Path, content: Option<&[u8]>, reason: &str) {
        let fingerprint = fingerprint(content, reason);
        {
            let preserved = self
                .preserved
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if preserved.get(path) == Some(&fingerprint) {
                return;
            }
        }
        match self.write_backup(path, content) {
            Ok(Some(backup)) => {
                self.preserved
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .insert(path.to_path_buf(), fingerprint);
                warn!(
                    "{reason}; original document preserved as {}",
                    backup.display()
                );
            }
            Ok(None) => {
                self.preserved
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .insert(path.to_path_buf(), fingerprint);
                warn!("{reason}; an identical backup copy is already preserved");
            }
            Err(error) => {
                error!("{reason}; could not preserve a backup copy: {error}");
            }
        }
    }

    fn write_backup(
        &self,
        path: &Path,
        content: Option<&[u8]>,
    ) -> Result<Option<PathBuf>, ConfigError> {
        match content {

            Some(bytes) => {
                if let Some(existing) = identical_backup(path, bytes)? {
                    return Ok(Some(existing));
                }
            }

            None => {
                if any_backup_exists(path)? {
                    return Ok(None);
                }
            }
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config");
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        for index in 0..64 {
            let backup = path.with_file_name(if index == 0 {
                format!("{name}.corrupt-{stamp}")
            } else {
                format!("{name}.corrupt-{stamp}-{index}")
            });
            if backup.exists() {
                continue;
            }
            match content {
                Some(bytes) => {
                    let mut file = match OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(&backup)
                    {
                        Ok(file) => file,
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                        Err(error) => return Err(error.into()),
                    };
                    file.write_all(bytes)?;
                    file.sync_all()?;
                }
                None => {
                    fs::copy(path, &backup)?;
                }
            }
            return Ok(Some(backup));
        }
        Err(ConfigError::InvalidValue(format!(
            "No free corrupt backup name for {}",
            path.display()
        )))
    }
}

fn backup_entries(path: &Path) -> Result<Vec<PathBuf>, ConfigError> {
    let Some(directory) = path.parent() else {
        return Ok(Vec::new());
    };
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    let prefix = format!("{name}.corrupt-");
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    Ok(entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|entry| {
            entry
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix))
        })
        .collect())
}

fn identical_backup(path: &Path, content: &[u8]) -> Result<Option<PathBuf>, ConfigError> {
    for entry in backup_entries(path)? {
        if fs::read(&entry).ok().as_deref() == Some(content) {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

fn any_backup_exists(path: &Path) -> Result<bool, ConfigError> {
    Ok(!backup_entries(path)?.is_empty())
}

fn fingerprint(content: Option<&[u8]>, reason: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    reason.hash(&mut hasher);
    hasher.finish()
}

enum DocumentLoad<T> {

    Loaded(T),

    FutureVersion(u32),
}

fn json_declared_version(value: &serde_json::Value) -> u32 {
    value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .filter(|version| *version <= u32::MAX as u64)
        .map_or(0, |version| version as u32)
}

fn parse_json_document<T: serde::de::DeserializeOwned>(
    path: &Path,
    bytes: &[u8],
    validate: impl FnOnce(&T) -> Result<(), ConfigError>,
) -> Result<DocumentLoad<T>, ConfigError> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| ConfigError::InvalidValue(format!("{}: {error}", path.display())))?;
    let declared = json_declared_version(&value);
    if SchemaVersion::resolve(declared).is_future() {
        return Ok(DocumentLoad::FutureVersion(declared));
    }
    let migrated = migrate_schema_version(declared).ok_or_else(|| {
        ConfigError::InvalidValue(format!(
            "{} uses schema {declared} without a migration path",
            path.display()
        ))
    })?;
    if let serde_json::Value::Object(object) = &mut value {
        object.insert("schema_version".into(), migrated.into());
    }
    let document: T = serde_json::from_value(value)
        .map_err(|error| ConfigError::InvalidValue(format!("{}: {error}", path.display())))?;
    validate(&document)?;
    Ok(DocumentLoad::Loaded(document))
}

fn validate_display(display: &DisplaySettings) -> Result<(), ConfigError> {
    for profile in display.outputs.values() {
        profile.validate().map_err(ConfigError::InvalidValue)?;
    }
    Ok(())
}

fn read_document<T: serde::de::DeserializeOwned>(
    path: &Path,
    fallback: T,
) -> Result<T, ConfigError> {
    match original_document(path)? {
        Some(content) => match parse_json_document::<T>(path, &content, |_| Ok(()))? {
            DocumentLoad::Loaded(document) => Ok(document),
            DocumentLoad::FutureVersion(found) => Err(ConfigError::InvalidValue(format!(
                "{} was written by a newer RixDE build (schema {found}); the edit was refused",
                path.display()
            ))),
        },
        None => Ok(fallback),
    }
}

fn publish_appearance(
    current_theme: &RwLock<ThemeSettings>,
    current_layout: &RwLock<LayoutConfig>,
    theme_sender: &broadcast::Sender<ThemeSettings>,
    layout_sender: &broadcast::Sender<LayoutConfig>,
    theme: Option<ThemeSettings>,
    layout: Option<LayoutConfig>,
) {

    let (theme_update, layout_update) = {
        let mut current_theme = current_theme.write().unwrap();
        let mut current_layout = current_layout.write().unwrap();
        let theme_update = theme.filter(|theme| *theme != *current_theme);
        let layout_update = layout.filter(|layout| *layout != *current_layout);
        if let Some(theme) = &theme_update {
            *current_theme = theme.clone();
        }
        if let Some(layout) = &layout_update {
            *current_layout = layout.clone();
        }
        (theme_update, layout_update)
    };
    if let Some(theme) = theme_update {
        let _ = theme_sender.send(theme);
    }
    if let Some(layout) = layout_update {
        let _ = layout_sender.send(layout);
    }
}

fn remove_journal(path: &Path) -> Result<(), ConfigError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn rollback_failed_appearance(
    root: &Path,
    rollback: &AppearanceRollback,
    journal_bytes: &[u8],
) -> Result<(), ConfigError> {
    let journal = root.join("appearance-transaction.json");

    write_atomic(&journal, journal_bytes).map_err(|error| {
        ConfigError::InvalidValue(format!("Could not retain rollback journal: {error}"))
    })?;
    restore_appearance(root, rollback)?;
    remove_journal(&journal)?;
    fs::File::open(root)?.sync_all()?;
    Ok(())
}

struct AppearanceLock(fs::File);

impl AppearanceLock {
    fn open(root: &Path) -> Result<fs::File, ConfigError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(root.join(".appearance.lock"))?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0
        {
            return Err(ConfigError::InvalidValue(
                "Appearance lock must be a protected owned file".into(),
            ));
        }
        Ok(file)
    }

    fn acquire(root: &Path) -> Result<Self, ConfigError> {
        let file = Self::open(root)?;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(Self(file));
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error.into());
            }
        }
    }

    fn lock_nonblocking(file: &fs::File) -> Result<bool, ConfigError> {
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(true);
            }
            let error = std::io::Error::last_os_error();
            match error.kind() {
                std::io::ErrorKind::Interrupted => continue,
                std::io::ErrorKind::WouldBlock => return Ok(false),
                _ => return Err(error.into()),
            }
        }
    }

    fn acquire_bounded(root: &Path, budget: Duration) -> Result<Option<Self>, ConfigError> {
        let file = Self::open(root)?;
        let deadline = Instant::now() + budget;
        loop {
            if Self::lock_nonblocking(&file)? {
                return Ok(Some(Self(file)));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            std::thread::sleep(RELOAD_LOCK_RETRY.min(deadline - now));
        }
    }
}

impl Drop for AppearanceLock {
    fn drop(&mut self) {

        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AppearanceRollback {
    version: u32,
    theme: Option<Vec<u8>>,
    layout: Option<Vec<u8>>,
    #[serde(default)]
    theme_mode: Option<u32>,
    #[serde(default)]
    layout_mode: Option<u32>,
}

fn original_document(path: &Path) -> Result<Option<Vec<u8>>, ConfigError> {
    Ok(original_document_with_mode(path)?.map(|(bytes, _)| bytes))
}

fn original_document_with_mode(path: &Path) -> Result<Option<(Vec<u8>, u32)>, ConfigError> {
    use std::io::Read;
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(ConfigError::InvalidValue(format!(
            "Configuration is not a regular file: {}",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(2 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(ConfigError::InvalidValue(
            "Appearance document exceeds 2 MiB".into(),
        ));
    }
    Ok(Some((bytes, metadata.mode() & 0o777)))
}

struct PreparedDocument {
    path: PathBuf,
    temporary: PathBuf,
}
impl PreparedDocument {
    fn new(path: &Path, bytes: &[u8]) -> Result<Self, ConfigError> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

        original_document(path)?;
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temporary = path.with_file_name(format!(".appearance-{}-{id}.tmp", std::process::id()));
        let prepared = Self {
            path: path.to_path_buf(),
            temporary,
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&prepared.temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(prepared)
    }
    fn publish(&self) -> Result<(), ConfigError> {
        fs::rename(&self.temporary, &self.path)?;
        Ok(())
    }
}
impl Drop for PreparedDocument {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temporary);
    }
}

fn validated_layout(layout: &LayoutConfig) -> Result<String, ConfigError> {
    crate::panels::validate_panels(layout)?;
    layout.keyboard.validate()?;
    layout.services.validate()?;

    let mut canonical = layout.clone();
    if canonical.schema_version == 0 {
        canonical.schema_version = CURRENT_SCHEMA_VERSION;
    }
    let text = format_layout_kdl(&canonical);
    if parse_layout_kdl(&text)? != canonical {
        return Err(ConfigError::InvalidValue(
            "Layout cannot be represented without data loss".into(),
        ));
    }
    Ok(text)
}

fn restore_appearance(root: &Path, rollback: &AppearanceRollback) -> Result<(), ConfigError> {
    if rollback.version != 1 {
        return Err(ConfigError::InvalidValue(
            "Unsupported appearance rollback version".into(),
        ));
    }
    for (name, bytes, mode) in [
        ("theme.json", &rollback.theme, rollback.theme_mode),
        ("layout.kdl", &rollback.layout, rollback.layout_mode),
    ] {
        let path = root.join(name);
        let current = original_document_with_mode(&path)?;
        if current.as_ref().map(|(bytes, _)| bytes) == bytes.as_ref()
            && current
                .as_ref()
                .is_none_or(|(_, current_mode)| mode.is_none_or(|mode| mode == *current_mode))
        {
            continue;
        }
        match bytes {
            Some(bytes) => write_atomic_mode(&path, bytes, mode.unwrap_or(0o600))?,
            None => fs::remove_file(&path)?,
        }
    }
    fs::File::open(root)?.sync_all()?;
    Ok(())
}

fn recover_appearance(root: &Path) -> Result<(), ConfigError> {
    let journal = root.join("appearance-transaction.json");
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&journal)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
    {
        return Err(ConfigError::InvalidValue(
            "Appearance journal must be a protected owned regular file".into(),
        ));
    }
    let mut bytes = Vec::new();
    use std::io::Read;
    Read::by_ref(&mut file)
        .take(32 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 32 * 1024 * 1024 {
        return Err(ConfigError::InvalidValue(
            "Appearance journal exceeds 32 MiB".into(),
        ));
    }
    if let Ok(rollback) = serde_json::from_slice::<AppearanceRollback>(&bytes) {
        restore_appearance(root, &rollback)?;
        remove_journal(&journal)?;
        fs::File::open(root)?.sync_all()?;
        return Ok(());
    }

    let snapshot: crate::themes::AppearanceSnapshot = serde_json::from_slice(&bytes)
        .map_err(|error| ConfigError::InvalidValue(error.to_string()))?;
    snapshot.theme.validate()?;
    let layout_data = validated_layout(&snapshot.layout)?;
    let theme = PreparedDocument::new(
        &root.join("theme.json"),
        &serde_json::to_vec_pretty(&snapshot.theme)
            .map_err(|error| ConfigError::InvalidValue(error.to_string()))?,
    )?;
    let layout = PreparedDocument::new(&root.join("layout.kdl"), layout_data.as_bytes())?;
    theme.publish()?;
    layout.publish()?;
    remove_journal(&journal)?;
    fs::File::open(root)?.sync_all()?;
    Ok(())
}

pub fn write_atomic(path: &Path, content: &[u8]) -> Result<(), ConfigError> {
    write_atomic_mode(path, content, 0o600)
}

fn write_atomic_mode(path: &Path, content: &[u8], mode: u32) -> Result<(), ConfigError> {
    use std::os::unix::fs::OpenOptionsExt;
    static NEXT_TEMP_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT_TEMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");
    let temp = path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), id));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(content)?;
        file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(mode & 0o777))?;
        file.sync_all()?;
        fs::rename(&temp, path)?;

        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(ConfigError::Io)
}

fn dirs_or_fallback() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|path| !path.is_empty()) {
        PathBuf::from(xdg).join("rix")
    } else if let Some(home) = std::env::var_os("HOME").filter(|path| !path.is_empty()) {
        PathBuf::from(home).join(".config").join("rix")
    } else {
        PathBuf::from(format!("/tmp/rix-config-{}", unsafe { libc::geteuid() }))
    }
}

fn ensure_private_fallback(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = directory.metadata()?;
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Fallback configuration directory belongs to another user",
        ));
    }
    if metadata.mode() & 0o077 != 0 && unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod appearance_transactions {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn paired_import_merges_against_disk_and_snapshot_reads_latest_revision() {
        let directory = tempfile::tempdir().unwrap();
        let mut importer = ConfigManager::init_in(directory.path().to_path_buf()).unwrap();
        importer._watcher = None;
        let writer = ConfigManager::init_in(directory.path().to_path_buf()).unwrap();
        let base = importer.get_theme();
        let mut newer = writer.get_theme();
        newer.blur_radius = 12;
        writer.save_theme(newer).unwrap();
        assert_eq!(importer.get_theme(), base);
        let mut imported = base.clone();
        imported.cursor_size = 48;
        let mut layout = importer.get_layout();
        layout.panels[0].thickness = 64;
        let result = importer
            .edit_theme_and_layout(&base, &imported, layout.clone())
            .unwrap();
        assert_eq!(result.theme.cursor_size, 48);
        assert_eq!(result.theme.blur_radius, 12);
        let (theme, raw_layout) = importer.appearance_snapshot().unwrap();
        assert_eq!(theme, result.theme);
        assert_eq!(parse_layout_kdl(&raw_layout).unwrap(), layout);
    }

    #[test]
    fn failed_layout_publication_does_not_change_theme_or_notify() {
        let directory = tempfile::tempdir().unwrap();
        let config = ConfigManager::init_in(directory.path().to_path_buf()).unwrap();
        let old_theme = config.get_theme();
        let old_layout = config.get_layout();
        let old_bytes = fs::read(directory.path().join("theme.json")).unwrap();
        let mut updates = config.subscribe_theme();
        fs::remove_file(directory.path().join("layout.kdl")).unwrap();
        fs::create_dir(directory.path().join("layout.kdl")).unwrap();
        let mut layout = old_layout.clone();
        layout.panels[0].thickness = 64;
        assert!(config
            .save_theme_and_layout(ThemeSettings::crisp_light(), layout)
            .is_err());
        assert_eq!(config.get_theme(), old_theme);
        assert_eq!(config.get_layout(), old_layout);
        assert_eq!(
            fs::read(directory.path().join("theme.json")).unwrap(),
            old_bytes
        );
        assert!(updates.try_recv().is_err());
        assert!(!directory
            .path()
            .join("appearance-transaction.json")
            .exists());
    }

    #[test]
    fn interrupted_import_rolls_back_before_clients_load() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let config = ConfigManager::init_in(root.to_path_buf()).unwrap();
        let old_theme = config.get_theme();
        let old_layout = config.get_layout();
        drop(config);
        for name in ["theme.json", "layout.kdl"] {
            fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o640)).unwrap();
        }
        let rollback = AppearanceRollback {
            version: 1,
            theme: Some(fs::read(root.join("theme.json")).unwrap()),
            layout: Some(fs::read(root.join("layout.kdl")).unwrap()),
            theme_mode: Some(0o640),
            layout_mode: Some(0o640),
        };
        write_atomic(
            &root.join("appearance-transaction.json"),
            &serde_json::to_vec(&rollback).unwrap(),
        )
        .unwrap();
        write_atomic(
            &root.join("theme.json"),
            &serde_json::to_vec(&ThemeSettings::crisp_light()).unwrap(),
        )
        .unwrap();

        write_atomic(&root.join("layout.kdl"), rollback.layout.as_ref().unwrap()).unwrap();
        let config = ConfigManager::init_in(root.to_path_buf()).unwrap();
        assert_eq!(config.get_theme(), old_theme);
        assert_eq!(config.get_layout(), old_layout);
        for name in ["theme.json", "layout.kdl"] {
            assert_eq!(fs::metadata(root.join(name)).unwrap().mode() & 0o777, 0o640);
        }
        assert!(!root.join("appearance-transaction.json").exists());
    }

    #[test]
    fn failed_rollback_after_journal_unlink_retains_recovery_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let rollback = AppearanceRollback {
            version: 1,
            theme: Some(b"original theme".to_vec()),
            layout: Some(b"original layout".to_vec()),
            theme_mode: Some(0o640),
            layout_mode: Some(0o640),
        };
        let bytes = serde_json::to_vec(&rollback).unwrap();
        write_atomic(&root.join("theme.json"), b"partially committed theme").unwrap();
        fs::create_dir(root.join("layout.kdl")).unwrap();
        let journal = root.join("appearance-transaction.json");
        assert!(!journal.exists());
        assert!(rollback_failed_appearance(root, &rollback, &bytes).is_err());
        assert_eq!(fs::read(&journal).unwrap(), bytes);
        assert_eq!(
            fs::read(root.join("theme.json")).unwrap(),
            b"original theme"
        );
        fs::remove_dir(root.join("layout.kdl")).unwrap();
        recover_appearance(root).unwrap();
        assert_eq!(
            fs::read(root.join("layout.kdl")).unwrap(),
            b"original layout"
        );
        for name in ["theme.json", "layout.kdl"] {
            assert_eq!(fs::metadata(root.join(name)).unwrap().mode() & 0o777, 0o640);
        }
        assert!(!journal.exists());
        remove_journal(&journal).unwrap();
    }

    #[test]
    fn other_manager_edits_merge_after_waiting_for_disk_lock() {
        let directory = tempfile::tempdir().unwrap();
        let mut first = ConfigManager::init_in(directory.path().to_path_buf()).unwrap();
        let mut second = ConfigManager::init_in(directory.path().to_path_buf()).unwrap();

        first._watcher = None;
        second._watcher = None;
        let base = second.get_theme();
        let mut changed = base.clone();
        changed.cursor_size = 48;
        let disk_guard = AppearanceLock::acquire(directory.path()).unwrap();
        let (started, started_rx) = std::sync::mpsc::channel();
        let (finished, finished_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            let merged = second.edit_theme(&base, &changed);
            finished.send(()).unwrap();
            merged
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            finished_rx.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));

        let mut newer = first.get_theme();
        newer.blur_radius = 12;
        first.write_theme_locked(newer).unwrap();
        drop(disk_guard);
        let merged = worker.join().unwrap().unwrap();
        assert_eq!(merged.cursor_size, 48);
        assert_eq!(merged.blur_radius, 12);
        let on_disk: ThemeSettings =
            serde_json::from_slice(&fs::read(directory.path().join("theme.json")).unwrap())
                .unwrap();
        assert_eq!(on_disk, merged);
    }

    #[test]
    fn reload_updates_both_caches_before_notifying_subscribers() {
        let theme = Arc::new(RwLock::new(ThemeSettings::default()));
        let layout = Arc::new(RwLock::new(LayoutConfig::default_layout()));
        let (theme_sender, mut theme_rx) = broadcast::channel(4);
        let (layout_sender, mut layout_rx) = broadcast::channel(4);
        let updated_theme = ThemeSettings::crisp_light();
        let mut updated_layout = LayoutConfig::default_layout();
        updated_layout.panels[0].thickness = 64;

        let layout_guard = layout.read().unwrap();
        let worker_theme = theme.clone();
        let worker_layout = layout.clone();
        let expected_theme = updated_theme.clone();
        let expected_layout = updated_layout.clone();
        let worker = std::thread::spawn(move || {
            publish_appearance(
                &worker_theme,
                &worker_layout,
                &theme_sender,
                &layout_sender,
                Some(expected_theme),
                Some(expected_layout),
            );
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while theme.try_read().is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "Reload did not acquire theme cache"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(matches!(
            theme_rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            layout_rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert_eq!(*layout_guard, LayoutConfig::default_layout());
        drop(layout_guard);
        worker.join().unwrap();
        assert_eq!(theme_rx.try_recv().unwrap(), updated_theme);
        assert_eq!(layout_rx.try_recv().unwrap(), updated_layout);
        assert_eq!(*theme.read().unwrap(), updated_theme);
        assert_eq!(*layout.read().unwrap(), updated_layout);
    }

    #[test]
    fn complete_import_publishes_both_documents_without_journal_or_temporary_files() {
        let directory = tempfile::tempdir().unwrap();
        let config = ConfigManager::init_in(directory.path().to_path_buf()).unwrap();
        let mut layout = config.get_layout();
        layout.panels[0].thickness = 64;
        let theme = ThemeSettings::crisp_light();
        config
            .save_theme_and_layout(theme.clone(), layout.clone())
            .unwrap();
        assert_eq!(config.get_theme(), theme);
        assert_eq!(config.get_layout(), layout);
        let reloaded = ConfigManager::init_in(directory.path().to_path_buf()).unwrap();
        assert_eq!(reloaded.get_theme(), theme);
        assert_eq!(reloaded.get_layout(), layout);
        assert!(!directory
            .path()
            .join("appearance-transaction.json")
            .exists());
        assert!(!fs::read_dir(directory.path()).unwrap().any(|entry| entry
            .unwrap()
            .path()
            .extension()
            .is_some_and(|ext| ext == "tmp")));
    }

    #[test]
    fn interrupted_theme_and_panel_commit_recovers_before_loading() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let mut layout = LayoutConfig::default_layout();
        layout.panels[0].thickness = 64;
        let snapshot = crate::themes::AppearanceSnapshot {
            theme: ThemeSettings::crisp_light(),
            layout,
        };
        fs::write(
            root.join("theme.json"),
            serde_json::to_vec(&snapshot.theme).unwrap(),
        )
        .unwrap();
        fs::write(
            root.join("layout.kdl"),
            format_layout_kdl(&LayoutConfig::default_layout()),
        )
        .unwrap();
        fs::write(
            root.join("appearance-transaction.json"),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();
        let config = ConfigManager::init_in(root.to_path_buf()).unwrap();
        assert_eq!(config.get_theme(), snapshot.theme);
        assert_eq!(config.get_layout(), snapshot.layout);
        assert!(!root.join("appearance-transaction.json").exists());
    }

    #[test]
    fn legacy_headerless_snapshot_journal_recovers_with_the_schema_header() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let mut layout = LayoutConfig::default_layout();
        layout.panels[0].thickness = 64;
        let snapshot = crate::themes::AppearanceSnapshot {
            theme: ThemeSettings::crisp_light(),
            layout,
        };

        let mut value = serde_json::to_value(&snapshot).unwrap();
        value["layout"]
            .as_object_mut()
            .unwrap()
            .remove("schema_version");
        fs::write(
            root.join("appearance-transaction.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();

        let config = ConfigManager::init_in(root.to_path_buf()).unwrap();
        assert_eq!(config.get_theme(), snapshot.theme);
        assert_eq!(config.get_layout().panels[0].thickness, 64);
        assert_eq!(config.get_layout().schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(
            layout_schema_version(&fs::read_to_string(root.join("layout.kdl")).unwrap()).unwrap(),
            CURRENT_SCHEMA_VERSION
        );
        assert!(!root.join("appearance-transaction.json").exists());
    }
}

#[cfg(test)]
mod live_reload {
    use super::*;

    fn corrupt_backups(root: &Path, name: &str) -> Vec<PathBuf> {
        let prefix = format!("{name}.corrupt-");
        let mut backups: Vec<PathBuf> = fs::read_dir(root)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&prefix))
            })
            .collect();
        backups.sort();
        backups
    }

    fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if predicate() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        predicate()
    }

    fn recv_until<T: Clone>(receiver: &mut broadcast::Receiver<T>, timeout: Duration) -> Option<T> {
        let deadline = Instant::now() + timeout;
        loop {
            match receiver.try_recv() {
                Ok(value) => return Some(value),
                Err(broadcast::error::TryRecvError::Empty) => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Closed) => return None,
            }
        }
    }

    #[test]
    fn debounce_coalesces_seven_events_into_one_reload() {
        let (batches, collected) = mpsc::channel::<Vec<PathBuf>>();
        let worker = DebounceWorker::spawn(
            Duration::from_millis(50),
            Duration::from_millis(50),
            move |batch: &[PathBuf]| {
                let _ = batches.send(batch.to_vec());
                ReloadOutcome::Done
            },
        );
        let sender = worker.sender();
        let root = PathBuf::from("/tmp/rix-config-debounce");
        for _ in 0..7 {
            sender.send(root.join("theme.json")).unwrap();
        }
        let batch = collected
            .recv_timeout(Duration::from_secs(2))
            .expect("debounced batch");
        assert_eq!(batch, vec![root.join("theme.json")]);
        assert!(collected.recv_timeout(Duration::from_millis(150)).is_err());
    }

    #[test]
    fn editor_save_burst_reloads_the_live_theme_once() {
        let directory = tempfile::tempdir().unwrap();
        let config = ConfigManager::init_in(directory.path().to_path_buf()).unwrap();
        let mut updates = config.subscribe_theme();
        for index in 0..7u32 {
            let mut theme = config.get_theme();
            theme.blur_radius = 30 + index;
            write_atomic(
                &directory.path().join("theme.json"),
                &serde_json::to_vec(&theme).unwrap(),
            )
            .unwrap();
        }
        let update = recv_until(&mut updates, Duration::from_secs(3)).expect("one reload");
        assert_eq!(update.blur_radius, 36);
        assert!(recv_until(&mut updates, Duration::from_millis(300)).is_none());
    }

    #[test]
    fn classification_ignores_internal_files_and_other_directories() {
        let root = PathBuf::from("/tmp/rix-config-classify");
        let paths = DocumentPaths {
            config_dir: root.clone(),
            layout: root.join("layout.kdl"),
            theme: root.join("theme.json"),
            display: root.join("display.json"),
            desktop: root.join("desktop.json"),
        };
        assert_eq!(
            paths.classify(&root.join("layout.kdl")),
            Some(Document::Layout)
        );
        assert_eq!(
            paths.classify(&root.join("theme.json")),
            Some(Document::Theme)
        );
        assert_eq!(
            paths.classify(&root.join("display.json")),
            Some(Document::Display)
        );
        assert_eq!(
            paths.classify(&root.join("desktop.json")),
            Some(Document::Desktop)
        );
        for ignored in [
            ".theme.json.1234.5.tmp",
            ".appearance-1234-5.tmp",
            ".appearance.lock",
            "appearance-transaction.json",
            "theme.json.corrupt-1700000000",
            "other.json",
        ] {
            assert_eq!(paths.classify(&root.join(ignored)), None, "{ignored}");
        }
        assert_eq!(paths.classify(Path::new("/tmp/theme.json")), None);
    }

    #[test]
    fn reload_lock_wait_is_bounded_and_never_blocks_writers_forever() {
        let directory = tempfile::tempdir().unwrap();
        let held = AppearanceLock::acquire(directory.path()).unwrap();
        let start = Instant::now();
        let acquired =
            AppearanceLock::acquire_bounded(directory.path(), Duration::from_millis(60)).unwrap();
        assert!(acquired.is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
        drop(held);
        assert!(
            AppearanceLock::acquire_bounded(directory.path(), Duration::from_millis(200))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn reload_defers_while_another_process_holds_the_appearance_lock() {
        let directory = tempfile::tempdir().unwrap();
        let config = ConfigManager::init_in(directory.path().to_path_buf()).unwrap();
        let mut updates = config.subscribe_theme();
        let held = AppearanceLock::acquire(directory.path()).unwrap();
        let mut theme = config.get_theme();
        theme.blur_radius = 41;
        write_atomic(
            &directory.path().join("theme.json"),
            &serde_json::to_vec(&theme).unwrap(),
        )
        .unwrap();

        std::thread::sleep(
            DEBOUNCE_WINDOW + RELOAD_LOCK_BUDGET + RELOAD_RETRY_DELAY + Duration::from_millis(200),
        );
        assert!(updates.try_recv().is_err());

        drop(held);
        let update = recv_until(&mut updates, Duration::from_secs(3))
            .expect("retry after the writer released the lock");
        assert_eq!(update.blur_radius, 41);
        assert!(recv_until(&mut updates, Duration::from_millis(300)).is_none());
    }

    #[test]
    fn deferred_reload_merges_events_that_arrive_while_retrying() {
        let directory = tempfile::tempdir().unwrap();
        let config = ConfigManager::init_in(directory.path().to_path_buf()).unwrap();
        let mut updates = config.subscribe_theme();
        let held = AppearanceLock::acquire(directory.path()).unwrap();
        let mut theme = config.get_theme();
        theme.blur_radius = 41;
        write_atomic(
            &directory.path().join("theme.json"),
            &serde_json::to_vec(&theme).unwrap(),
        )
        .unwrap();

        std::thread::sleep(DEBOUNCE_WINDOW + RELOAD_LOCK_BUDGET + Duration::from_millis(50));
        theme.blur_radius = 42;
        write_atomic(
            &directory.path().join("theme.json"),
            &serde_json::to_vec(&theme).unwrap(),
        )
        .unwrap();
        drop(held);
        let update =
            recv_until(&mut updates, Duration::from_secs(3)).expect("coalesced deferred reload");
        assert_eq!(update.blur_radius, 42);
        assert!(recv_until(&mut updates, Duration::from_millis(300)).is_none());
    }

    #[test]
    fn corrupt_documents_are_defaulted_and_preserved_once() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let corrupt = [
            ("theme.json", b"{ this is not json".as_slice()),
            ("display.json", b"junk".as_slice()),
            ("desktop.json", b"junk".as_slice()),
            ("layout.kdl", b"not a layout {{{".as_slice()),
        ];
        for (name, bytes) in corrupt {
            fs::write(root.join(name), bytes).unwrap();
        }
        let config = ConfigManager::init_in(root.to_path_buf()).unwrap();
        assert_eq!(config.get_theme(), ThemeSettings::default());
        assert_eq!(config.get_display(), DisplaySettings::default());
        assert_eq!(config.get_desktop(), DesktopSettings::default());
        assert_eq!(config.get_layout(), LayoutConfig::default_layout());
        for (name, bytes) in corrupt {
            let backups = corrupt_backups(root, name);
            assert_eq!(backups.len(), 1, "{name}");
            assert_eq!(fs::read(&backups[0]).unwrap(), bytes, "{name}");

            assert_eq!(fs::read(root.join(name)).unwrap(), bytes, "{name}");
        }

        drop(config);
        let config = ConfigManager::init_in(root.to_path_buf()).unwrap();
        for (name, _) in corrupt {
            assert_eq!(corrupt_backups(root, name).len(), 1, "{name}");
        }
        config.save_theme(ThemeSettings::crisp_light()).unwrap();
        assert_eq!(corrupt_backups(root, "theme.json").len(), 1);
    }

    #[test]
    fn future_schema_documents_are_defaulted_read_only_and_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let future = CURRENT_SCHEMA_VERSION + 7;

        let theme = ThemeSettings {
            schema_version: future,
            ..ThemeSettings::default()
        };
        let theme_bytes = serde_json::to_vec_pretty(&theme).unwrap();
        fs::write(root.join("theme.json"), &theme_bytes).unwrap();

        let display = DisplaySettings {
            schema_version: future,
            ..DisplaySettings::default()
        };
        let display_bytes = serde_json::to_vec_pretty(&display).unwrap();
        fs::write(root.join("display.json"), &display_bytes).unwrap();

        let desktop = DesktopSettings {
            schema_version: future,
            ..DesktopSettings::default()
        };
        let desktop_bytes = serde_json::to_vec_pretty(&desktop).unwrap();
        fs::write(root.join("desktop.json"), &desktop_bytes).unwrap();

        let mut layout = LayoutConfig::default_layout();
        layout.schema_version = future;
        let layout_bytes = format_layout_kdl(&layout).into_bytes();
        fs::write(root.join("layout.kdl"), &layout_bytes).unwrap();

        let config = ConfigManager::init_in(root.to_path_buf()).unwrap();
        assert_eq!(config.get_theme(), ThemeSettings::default());
        assert_eq!(config.get_display(), DisplaySettings::default());
        assert_eq!(config.get_desktop(), DesktopSettings::default());
        assert_eq!(config.get_layout(), LayoutConfig::default_layout());

        assert!(config.save_theme(ThemeSettings::crisp_light()).is_err());
        assert!(config.save_display(DisplaySettings::default()).is_err());
        assert!(config.save_desktop(DesktopSettings::default()).is_err());
        assert!(config.save_layout(LayoutConfig::default_layout()).is_err());
        assert!(config
            .save_theme_and_layout(ThemeSettings::crisp_light(), LayoutConfig::default_layout())
            .is_err());

        assert_eq!(fs::read(root.join("theme.json")).unwrap(), theme_bytes);
        assert_eq!(fs::read(root.join("display.json")).unwrap(), display_bytes);
        assert_eq!(fs::read(root.join("desktop.json")).unwrap(), desktop_bytes);
        assert_eq!(fs::read(root.join("layout.kdl")).unwrap(), layout_bytes);

        assert!(corrupt_backups(root, "theme.json").is_empty());
    }

    #[test]
    fn live_reload_of_a_future_theme_switches_to_default_and_read_only() {
        let directory = tempfile::tempdir().unwrap();
        let config = ConfigManager::init_in(directory.path().to_path_buf()).unwrap();
        let mut updates = config.subscribe_theme();
        config.save_theme(ThemeSettings::crisp_light()).unwrap();
        assert_eq!(
            recv_until(&mut updates, Duration::from_secs(2)).unwrap(),
            ThemeSettings::crisp_light()
        );

        let mut future = ThemeSettings::crisp_light();
        future.schema_version = CURRENT_SCHEMA_VERSION + 1;
        let bytes = serde_json::to_vec_pretty(&future).unwrap();
        write_atomic(&directory.path().join("theme.json"), &bytes).unwrap();

        let update = recv_until(&mut updates, Duration::from_secs(3))
            .expect("default theme after a future document");
        assert_eq!(update, ThemeSettings::default());
        assert!(config.save_theme(ThemeSettings::crisp_light()).is_err());
        assert_eq!(
            fs::read(directory.path().join("theme.json")).unwrap(),
            bytes
        );
    }

    #[test]
    fn legacy_documents_without_headers_load_and_upgrade_on_save() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();

        let theme = ThemeSettings {
            blur_radius: 17,
            ..ThemeSettings::default()
        };
        let mut theme_value = serde_json::to_value(&theme).unwrap();
        theme_value
            .as_object_mut()
            .unwrap()
            .remove("schema_version");
        fs::write(
            root.join("theme.json"),
            serde_json::to_vec(&theme_value).unwrap(),
        )
        .unwrap();

        let display = DisplaySettings {
            width: 2560,
            ..DisplaySettings::default()
        };
        let mut display_value = serde_json::to_value(&display).unwrap();
        display_value
            .as_object_mut()
            .unwrap()
            .remove("schema_version");
        fs::write(
            root.join("display.json"),
            serde_json::to_vec(&display_value).unwrap(),
        )
        .unwrap();

        let desktop = DesktopSettings {
            language: "en".into(),
            ..DesktopSettings::default()
        };
        let mut desktop_value = serde_json::to_value(&desktop).unwrap();
        desktop_value
            .as_object_mut()
            .unwrap()
            .remove("schema_version");
        fs::write(
            root.join("desktop.json"),
            serde_json::to_vec(&desktop_value).unwrap(),
        )
        .unwrap();

        let mut layout = LayoutConfig::default_layout();
        layout.panels[0].thickness = 64;
        let legacy_layout = format_layout_kdl(&layout).replace("schema-version 1\n\n", "");
        fs::write(root.join("layout.kdl"), &legacy_layout).unwrap();

        let config = ConfigManager::init_in(root.to_path_buf()).unwrap();
        assert_eq!(config.get_theme().blur_radius, 17);
        assert_eq!(config.get_theme().schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(config.get_display().width, 2560);
        assert_eq!(config.get_display().schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(config.get_desktop().language, "en");
        assert_eq!(config.get_desktop().schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(config.get_layout().panels[0].thickness, 64);
        assert_eq!(config.get_layout().schema_version, CURRENT_SCHEMA_VERSION);

        config.save_theme(config.get_theme()).unwrap();
        config.save_display(config.get_display()).unwrap();
        config.save_desktop(config.get_desktop()).unwrap();
        config.save_layout(config.get_layout()).unwrap();
        let saved_theme: ThemeSettings =
            serde_json::from_slice(&fs::read(root.join("theme.json")).unwrap()).unwrap();
        assert_eq!(saved_theme.schema_version, CURRENT_SCHEMA_VERSION);
        let saved_display: DisplaySettings =
            serde_json::from_slice(&fs::read(root.join("display.json")).unwrap()).unwrap();
        assert_eq!(saved_display.schema_version, CURRENT_SCHEMA_VERSION);
        let saved_desktop: DesktopSettings =
            serde_json::from_slice(&fs::read(root.join("desktop.json")).unwrap()).unwrap();
        assert_eq!(saved_desktop.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(
            layout_schema_version(&fs::read_to_string(root.join("layout.kdl")).unwrap()).unwrap(),
            CURRENT_SCHEMA_VERSION
        );
    }

    #[test]
    fn reload_rebuilds_only_documents_named_by_events() {
        let directory = tempfile::tempdir().unwrap();
        let config = ConfigManager::init_in(directory.path().to_path_buf()).unwrap();
        let mut theme_updates = config.subscribe_theme();
        let mut layout_updates = config.subscribe();
        let mut desktop_updates = config.subscribe_desktop();

        let mut desktop = config.get_desktop();
        desktop.screen_cast_fps = 45;
        write_atomic(
            &directory.path().join("desktop.json"),
            &serde_json::to_vec(&desktop).unwrap(),
        )
        .unwrap();

        let update =
            recv_until(&mut desktop_updates, Duration::from_secs(3)).expect("desktop reload");
        assert_eq!(update.screen_cast_fps, 45);
        assert!(theme_updates.try_recv().is_err());
        assert!(layout_updates.try_recv().is_err());
    }

    #[test]
    fn live_reload_of_a_corrupt_document_keeps_settings_and_preserves_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let config = ConfigManager::init_in(directory.path().to_path_buf()).unwrap();
        let before = config.get_theme();
        let corrupt = b"{ broken live revision".to_vec();
        fs::write(directory.path().join("theme.json"), &corrupt).unwrap();
        assert!(
            wait_until(Duration::from_secs(3), || {
                !corrupt_backups(directory.path(), "theme.json").is_empty()
            }),
            "corrupt live revision was not preserved"
        );
        assert_eq!(config.get_theme(), before);
        let backups = corrupt_backups(directory.path(), "theme.json");
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read(&backups[0]).unwrap(), corrupt);
    }
}
