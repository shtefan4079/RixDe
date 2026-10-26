use clap::Parser;
use notify::Watcher;
use rix_config::ThemeSettings;
const APPLICATION_ID: &str = "org.rixde.Files";
use rix_files::{
    fs::{self, Entry, SortBy},
    history, opening,
    operations::{self, ConflictPolicy, Kind, Request},
    preferences::{self, Preferences},
    state::Tab,
    storage,
};
use slint::{ComponentHandle, Image, Model, ModelRc, VecModel};
use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant, SystemTime},
};
slint::include_modules!();
mod content;
mod dialog_backdrop;
mod disk;
mod file_drag;
mod i18n;
mod menu_host;
mod native_dnd;
mod native_popup;
mod native_style;
mod picker;
mod picker_parent;
mod properties;
#[path = "../../../ui/common/skins.rs"]
mod skins;
#[path = "../../../ui/common/bindings.rs"]
mod theme_bindings;

#[derive(Parser)]
#[command(about = "Rix Files — native file manager for RixDE")]
struct Args {

    #[arg(long, conflicts_with_all = ["select", "trash", "paths", "view"])]
    portal_picker: bool,

    #[arg(long)]
    view: bool,

    #[arg(long)]
    select: bool,

    #[arg(long)]
    trash: bool,

    paths: Vec<OsString>,
}
#[derive(Clone)]
struct FileProperties {
    path: PathBuf,
    kind: &'static str,
    size: u64,
    modified: Option<SystemTime>,
    accessed: Option<SystemTime>,
    created: Option<SystemTime>,
    allocated: u64,
    permissions: u32,
    owner: u32,
    group: u32,
    link_target: Option<PathBuf>,
    original_path: Option<PathBuf>,
    deleted: Option<SystemTime>,
}

struct IncomingLocation {
    folder: PathBuf,
    selected: Option<PathBuf>,
}
enum Message {
    Content(content::Event),
    Disk {
        request: u64,
        path: PathBuf,
        result: Result<storage::DiskUsage, String>,
    },
    Loaded {
        id: u64,
        generation: u64,
        resolved_path: Option<PathBuf>,
        result: Result<Vec<Entry>, String>,
        trash: Vec<trash::TrashItem>,
        search_limited: bool,
    },
    Theme(Box<ThemeSettings>),
    WindowAppearance(Box<rix_config::WindowAppearanceSettings>),
    Operation(operations::Report),
    History {
        history: history::History,
        report: operations::Report,
        redo: bool,
    },
    Error(String),
    ClipboardError(String),
    Apps(PathBuf, opening::ApplicationChoices),
    Incoming(Vec<IncomingLocation>),
    Properties {
        request: u64,
        paths: Vec<PathBuf>,
        result: Result<Vec<FileProperties>, String>,
    },
    PropertiesSummary {
        request: u64,
        summary: properties::Summary,
    },
    Clipboard {
        paths: Vec<PathBuf>,
        cut: bool,
        destination: PathBuf,
    },
}
#[derive(Default)]
struct PreferenceWrites {
    pending: Option<Preferences>,
    running: bool,
}
struct Place {
    name: String,
    path: PathBuf,
    icon: String,
    builtin: bool,
}
struct App {
    history: history::History,
    content: content::State,
    picker: Option<picker::Picker>,
    window: slint::Weak<FilesWindow>,
    native_style: Option<Rc<RefCell<native_style::NativeStyle>>>,
    native_popup: Option<Rc<RefCell<native_popup::NativePopup>>>,
    native_dnd: Option<Rc<RefCell<native_dnd::NativeDnd>>>,
    drop_operation: Option<u64>,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    prefs: Preferences,
    desktop_theme: ThemeSettings,
    window_appearance: rix_config::WindowAppearanceSettings,
    desktop_material_enabled: bool,
    theme: ThemeSettings,
    icons: HashMap<String, Image>,
    resolver: rix_assets::IconResolver,
    places: Vec<Place>,
    disk_path: Option<PathBuf>,
    disk_usage: Option<storage::DiskUsage>,
    disk_request: Option<u64>,
    disk_next_request: u64,
    disk_refresh_due: Instant,
    grid_columns: usize,
    sender: mpsc::Sender<Message>,
    watcher: Option<notify::RecommendedWatcher>,
    dirty: Arc<AtomicBool>,
    search_due: Option<Instant>,
    watch_due: Option<Instant>,
    operation: Option<Arc<AtomicBool>>,
    clipboard: Vec<PathBuf>,
    cut: bool,
    dialog_paths: Vec<PathBuf>,
    dialog_kind: i32,
    properties_items: Vec<FileProperties>,
    properties_scan: Option<Arc<AtomicBool>>,
    properties_request: u64,
    properties_metadata_request: Arc<AtomicU64>,
    properties_summary: Option<properties::Summary>,
    applications: Vec<opening::Application>,
    application_path: Option<PathBuf>,
    application_choices: Option<opening::ApplicationChoices>,
    trash_items: HashMap<PathBuf, trash::TrashItem>,
    notice: Option<String>,
    preference_writes: Arc<std::sync::Mutex<PreferenceWrites>>,
    preference_directory: PathBuf,
}
fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::from(Rc::new(VecModel::from(items)))
}
fn update_model<T: Clone + PartialEq + 'static>(old: ModelRc<T>, items: Vec<T>) -> ModelRc<T> {
    if let Some(model) = old.as_any().downcast_ref::<VecModel<T>>() {
        if model.row_count() != items.len() {
            model.set_vec(items);
        } else {
            for (index, item) in items.into_iter().enumerate() {
                if model.row_data(index).as_ref() != Some(&item) {
                    model.set_row_data(index, item);
                }
            }
        }
        old
    } else {
        model(items)
    }
}
fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/".into())
}
fn local_path(value: &std::ffi::OsStr) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStrExt;
    if value.as_bytes().contains(&0) {
        return Err("Invalid path".into());
    }
    if let Some(value) = value.to_str().filter(|s| s.contains("://")) {
        let uri = url::Url::parse(value).map_err(|e| e.to_string())?;
        if uri.query().is_some() || uri.fragment().is_some() {
            return Err("File URI contains a query or fragment".into());
        }
        let path = uri
            .to_file_path()
            .map_err(|_| "Only local files and mounted filesystems are supported")?;
        if path.as_os_str().as_bytes().contains(&0) {
            return Err("Invalid path".into());
        }
        return Ok(path);
    }
    let path = PathBuf::from(value);
    Ok(if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path)
    })
}
fn translated(template: &str, values: &[(&str, String)]) -> String {
    let mut text = i18n::tr(template).to_string();
    for (key, value) in values {
        text = text.replace(&format!("{{{key}}}"), value);
    }
    text
}
fn entry_kind(kind: &str) -> String {
    if let Some(extension) = kind.strip_suffix(" file") {
        translated("{extension} file", &[("extension", extension.into())])
    } else {
        i18n::tr(kind).to_string()
    }
}
fn load_theme() -> ThemeSettings {
    std::fs::read(preferences::config_directory().join("theme.json"))
        .ok()
        .and_then(|v| serde_json::from_slice::<ThemeSettings>(&v).ok())
        .filter(|t| t.validate().is_ok())
        .unwrap_or_default()
}
fn load_desktop() -> rix_config::DesktopSettings {
    std::fs::read(preferences::config_directory().join("desktop.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<rix_config::DesktopSettings>(&bytes).ok())
        .filter(|settings| settings.validate().is_ok())
        .unwrap_or_default()
}
fn size(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut i = 0;
    while value >= 1024. && i < 4 {
        value /= 1024.;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", units[i])
    }
}
fn date(value: Option<SystemTime>) -> String {
    value
        .map(|v| {
            let dt: chrono::DateTime<chrono::Local> = v.into();
            dt.format("%Y-%m-%d %H:%M").to_string()
        })
        .unwrap_or_default()
}
fn configured_places(home: &Path, content: &str) -> Vec<Place> {
    let mut out: Vec<Place> = Vec::new();
    for line in content.lines().map(str::trim) {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Some(key) = key
            .strip_prefix("XDG_")
            .and_then(|key| key.strip_suffix("_DIR"))
        else {
            continue;
        };
        let (name, icon) = match key {
            "DESKTOP" => ("Desktop", "folder"),
            "DOWNLOAD" => ("Downloads", "folder-download"),
            "DOCUMENTS" => ("Documents", "folder"),
            "PICTURES" => ("Pictures", "folder-pictures"),
            "MUSIC" => ("Music", "folder-music"),
            "VIDEOS" => ("Videos", "folder-videos"),

            _ => continue,
        };
        let Some(value) = value
            .trim()
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
        else {
            continue;
        };
        let path = if let Some(value) = value.strip_prefix("$HOME/") {
            home.join(value)
        } else {
            PathBuf::from(value)
        };
        if path.is_absolute()
            && path.is_dir()
            && path != home
            && !out.iter().any(|place| place.path == path)
        {
            out.push(Place {
                name: name.into(),
                path,
                icon: icon.into(),
                builtin: true,
            });
        }
    }
    out
}
fn places(prefs: &Preferences) -> Vec<Place> {
    let home = home();
    let mut out = vec![Place {
        name: "Home".into(),
        path: home.clone(),
        icon: "user-home".into(),
        builtin: true,
    }];
    let config = preferences::config_directory()
        .parent()
        .unwrap_or(Path::new("/tmp"))
        .join("user-dirs.dirs");
    if let Ok(content) = std::fs::read_to_string(config) {
        out.extend(configured_places(&home, &content));
    }
    out.push(Place {
        name: "Filesystem".into(),
        path: "/".into(),
        icon: "drive-harddisk".into(),
        builtin: true,
    });
    out.push(Place {
        name: "Trash".into(),
        path: "trash:".into(),
        icon: "user-trash".into(),
        builtin: true,
    });
    out.retain(|place| !prefs.hidden_places.contains(&place.path));
    for path in &prefs.bookmarks {
        if !out.iter().any(|p| &p.path == path) {
            out.push(Place {
                name: path
                    .file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy()
                    .into(),
                path: path.clone(),
                icon: "folder-favorites".into(),
                builtin: false,
            });
        }
    }
    out
}
fn active_place(places: &[Place], path: &Path) -> Option<usize> {
    places
        .iter()
        .enumerate()
        .filter(|(_, place)| {
            place.path == path || (place.path.is_absolute() && path.starts_with(&place.path))
        })
        .max_by_key(|(_, place)| place.path.components().count())
        .map(|(index, _)| index)
}
impl App {
    fn new(
        window: &FilesWindow,
        tabs: Vec<Tab>,
        prefs: Preferences,
        theme: ThemeSettings,
        sender: mpsc::Sender<Message>,
        dirty: Arc<AtomicBool>,
    ) -> Self {
        if let Some(width) = prefs.sidebar_width {
            window.set_sidebar_width(width);
        }
        if prefs.sidebar_collapsed {
            window.set_sidebar_collapsed(true);
        }
        Self {
            history: history::History::default(),
            window: window.as_weak(),
            native_style: None,
            content: content::State::default(),
            picker: None,
            native_popup: None,
            native_dnd: None,
            drop_operation: None,
            next_id: tabs.iter().map(|t| t.id).max().unwrap_or(0) + 1,
            tabs,
            active: 0,
            places: places(&prefs),
            disk_path: None,
            disk_usage: None,
            disk_request: None,
            disk_next_request: 0,
            disk_refresh_due: Instant::now(),
            grid_columns: 5,
            prefs,
            desktop_theme: theme.clone(),
            window_appearance: rix_config::WindowAppearanceSettings::default(),
            desktop_material_enabled: true,
            theme,
            icons: HashMap::new(),
            resolver: rix_assets::IconResolver::default(),
            sender,
            watcher: None,
            dirty,
            search_due: None,
            watch_due: None,
            operation: None,
            clipboard: vec![],
            cut: false,
            dialog_paths: vec![],
            dialog_kind: 0,
            properties_items: vec![],
            properties_scan: None,
            properties_request: 0,
            properties_metadata_request: Arc::new(AtomicU64::new(0)),
            properties_summary: None,
            applications: vec![],
            application_path: None,
            application_choices: None,
            trash_items: HashMap::new(),
            notice: None,
            preference_writes: Arc::new(std::sync::Mutex::new(PreferenceWrites::default())),
            preference_directory: preferences::config_directory(),
        }
    }
    fn icon(&mut self, name: &str) -> Image {
        let path = Path::new(name);
        if path.is_absolute() {
            if let Some(icon) = self.icons.get(name) {
                return icon.clone();
            }
            if let Some(icon) = Self::absolute_icon(path) {
                self.icons.insert(name.into(), icon.clone());
                return icon;
            }
        }
        self.icon_candidates(&[name.into()])
    }
    fn absolute_icon(path: &Path) -> Option<Image> {
        use std::{io::Read, os::unix::fs::OpenOptionsExt};
        const MAX_BYTES: u64 = 16 * 1024 * 1024;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(path)
            .ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || metadata.len() > MAX_BYTES {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() as u64 > MAX_BYTES {
            return None;
        }
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("svg") || extension.eq_ignore_ascii_case("svgz")
            })
        {
            if bytes.starts_with(&[0x1f, 0x8b]) {
                let mut svg = Vec::new();
                flate2::read::GzDecoder::new(std::io::Cursor::new(bytes))
                    .take(MAX_BYTES + 1)
                    .read_to_end(&mut svg)
                    .ok()?;
                if svg.len() as u64 > MAX_BYTES {
                    return None;
                }
                bytes = svg;
            }
            return Image::load_from_svg_data(&bytes).ok();
        }
        let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .ok()?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(4096);
        limits.max_image_height = Some(4096);
        limits.max_alloc = Some(64 * 1024 * 1024);
        reader.limits(limits);
        let pixels = reader.decode().ok()?.to_rgba8();
        Some(Image::from_rgba8(
            slint::SharedPixelBuffer::clone_from_slice(
                pixels.as_raw(),
                pixels.width(),
                pixels.height(),
            ),
        ))
    }
    fn file_icon(&mut self, entry: &Entry) -> Image {
        if entry.icon_names.is_empty() {
            self.icon_candidates(&rix_files::file_icons::names(
                &entry.path,
                entry.is_dir,
                false,
            ))
        } else {
            self.icon_candidates(&entry.icon_names)
        }
    }
    fn icon_candidates(&mut self, names: &[String]) -> Image {
        let key = names.join("\u{1f}");
        if let Some(icon) = self.icons.get(&key) {
            return icon.clone();
        }
        let name = names
            .first()
            .map(String::as_str)
            .unwrap_or("text-x-generic");
        let icon = self
            .resolver
            .resolve_names_with(names, &self.theme.icon_theme, 64, |path| {
                Image::load_from_path(path).ok()
            })
            .unwrap_or_else(|| {
                let svg = match name {
                    "user-trash" => include_str!("../assets/trash.svg"),
                    "drive-harddisk" => include_str!("../assets/drive.svg"),
                    name if name.contains("folder") || name == "user-home" => {
                        include_str!("../../../ui/common/icons/folder.svg")
                    }
                    name if names.iter().any(|name| name == "text-x-generic") => {
                        let family = names
                            .iter()
                            .find(|name| {
                                matches!(
                                    name.as_str(),
                                    "application-x-executable"
                                        | "package-x-generic"
                                        | "image-x-generic"
                                        | "audio-x-generic"
                                        | "video-x-generic"
                                        | "x-office-spreadsheet"
                                        | "text-x-script"
                                )
                            })
                            .map(String::as_str)
                            .unwrap_or(name);
                        let svg = rix_files::file_icons::fallback_svg(
                            family,
                            &self.theme.appearance.color("text_primary").to_hex(),
                            &self.theme.accent_color,
                        );
                        return Image::load_from_svg_data(svg.as_bytes()).unwrap_or_default();
                    }
                    _ => include_str!("../../../ui/common/icons/applications.svg"),
                };
                let svg = svg.replace(
                    "#fff",
                    &self.theme.appearance.color("text_primary").to_hex(),
                );
                Image::load_from_svg_data(svg.as_bytes()).unwrap_or_default()
            });
        self.icons.insert(key, icon.clone());
        icon
    }
    fn render(&mut self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let tab = &self.tabs[self.active];
        let data: Vec<_> = tab
            .entries
            .iter()
            .map(|e| {
                (
                    e.clone(),
                    tab.selected.contains(&e.path),
                    tab.focus
                        .is_some_and(|i| tab.entries.get(i).is_some_and(|f| f.path == e.path)),
                )
            })
            .collect();
        let mut items = Vec::with_capacity(data.len());
        for (e, selected, focused) in data {
            let icon = self.file_icon(&e);
            let is_hidden = e.name.starts_with('.');
            items.push(FileItem {
                name: e.name.into(),
                detail: if self.tabs[self.active].path == Path::new("trash:") {
                    e.kind.into()
                } else {
                    entry_kind(&e.kind).into()
                },
                size: if e.is_dir {
                    String::new()
                } else {
                    size(e.size)
                }
                .into(),
                modified: date(e.modified).into(),
                icon,
                is_dir: e.is_dir,
                is_symlink: e.is_symlink,
                selected,
                cut: self.cut && self.clipboard.contains(&e.path),
                focused,
                is_hidden,
            });
        }
        let previous_grid = window.get_grid_rows();
        let columns = self.grid_columns;
        let rows = items
            .chunks(columns)
            .enumerate()
            .map(|(i, chunk)| {
                let mut row = chunk.to_vec();
                row.resize(columns, FileItem::default());
                let old = previous_grid
                    .row_data(i)
                    .map(|r| r.items)
                    .unwrap_or_default();
                GridRow {
                    items: update_model(old, row),
                    start: (i * columns) as i32,
                }
            })
            .collect();
        window.set_grid_rows(update_model(previous_grid, rows));
        window.set_entries(update_model(window.get_entries(), items));
        window.set_tabs(model(
            self.tabs
                .iter()
                .map(|t| TabItem {
                    label: if t.path == Path::new("trash:") {
                        i18n::tr("Trash").to_string()
                    } else {
                        t.path
                            .file_name()
                            .unwrap_or(t.path.as_os_str())
                            .to_string_lossy()
                            .into_owned()
                    }
                    .into(),
                })
                .collect(),
        ));
        let active_place = active_place(&self.places, &self.tabs[self.active].path);
        let mut previous_builtin = true;
        let places_data: Vec<_> = self
            .places
            .iter()
            .enumerate()
            .map(|(index, place)| {
                let group_start = !place.builtin && previous_builtin;
                previous_builtin = place.builtin;
                (
                    place.name.clone(),
                    place.icon.clone(),
                    place.builtin,
                    active_place == Some(index),
                    group_start,
                )
            })
            .collect();
        let places_data = places_data
            .into_iter()
            .map(|(name, icon, builtin, active, group_start)| PlaceItem {
                label: if builtin {
                    i18n::tr(&name)
                } else {
                    name.into()
                },
                icon: self.icon(&icon),
                active,
                removable: true,
                group_start,
            })
            .collect();
        window.set_places(model(places_data));
        self.render_disk(&window);
        let tab = &self.tabs[self.active];
        window.set_active_tab(self.active as i32);
        window.set_location(tab.path.to_string_lossy().into_owned().into());
        window.set_search(tab.query.clone().into());
        window.set_can_back(tab.can_back());
        window.set_can_forward(tab.can_forward());
        window.set_can_up(tab.path.parent().is_some());
        window.set_busy(tab.loading);
        window.set_show_hidden(tab.show_hidden);
        window.set_sort_mode(match tab.sort {
            SortBy::Name => 0,
            SortBy::Size => 1,
            SortBy::Modified => 2,
            SortBy::Kind => 3,
        });
        window.set_grid_view(self.prefs.grid_view);
        window.set_selection_count(tab.selected.len() as i32);
        window.set_trash_count(self.trash_items.len() as i32);
        window.set_paste_available(true);
        window.set_trash_view(tab.path == Path::new("trash:"));
        window.set_operation_active(self.operation.is_some());
        window.set_can_undo(
            self.history.can_undo() && self.operation.is_none() && self.picker.is_none(),
        );
        window.set_can_redo(
            self.history.can_redo() && self.operation.is_none() && self.picker.is_none(),
        );
        window.set_empty_message(i18n::tr(&tab.error.clone().unwrap_or_else(|| {
            if tab.query.is_empty() {
                "This folder is empty".into()
            } else {
                "No matching files".into()
            }
        })));
        window.set_status(i18n::tr(
            &self
                .notice
                .clone()
                .or_else(|| tab.error.clone())
                .unwrap_or_else(|| {
                    format!(
                        "{}{}",
                        translated(
                            "{count} items · {selected} selected",
                            &[
                                ("count", tab.entries.len().to_string()),
                                ("selected", tab.selected.len().to_string())
                            ]
                        ),
                        if tab.search_limited && !tab.query.is_empty() {
                            i18n::tr(" · search limited by time").to_string()
                        } else if tab.entries.len() == fs::SEARCH_LIMIT && !tab.query.is_empty() {
                            i18n::tr(" · search limited to 10,000 results").to_string()
                        } else {
                            String::new()
                        }
                    )
                }),
        ));
    }
    fn save(&self) {
        let mut writes = self.preference_writes.lock().unwrap();
        let mut prefs = self.prefs.clone();
        if let Some(window) = self.window.upgrade() {
            prefs.sidebar_width = Some(window.get_sidebar_width());
            prefs.sidebar_collapsed = window.get_sidebar_collapsed();
        }
        writes.pending = Some(prefs);
        if writes.running {
            return;
        }
        writes.running = true;
        drop(writes);
        let writes = self.preference_writes.clone();
        let directory = self.preference_directory.clone();
        let sender = self.sender.clone();
        tokio::task::spawn_blocking(move || loop {
            let prefs = {
                let mut state = writes.lock().unwrap();
                match state.pending.take() {
                    Some(prefs) => prefs,
                    None => {
                        state.running = false;
                        return;
                    }
                }
            };
            if let Err(e) = prefs.save_to(directory.clone()) {
                let _ = sender.send(Message::Error(format!("Could not save preferences: {e}")));
            }
        });
    }
    fn watch(&mut self) {
        self.watcher = None;
        let path = self.tabs[self.active].path.clone();
        if path == Path::new("trash:") {
            return;
        }
        let dirty = self.dirty.clone();
        dirty.store(false, Ordering::Relaxed);
        if let Ok(mut watcher) =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if event.is_ok_and(|e| !matches!(e.kind, notify::EventKind::Access(_))) {
                    dirty.store(true, Ordering::Relaxed);
                }
            })
        {
            if watcher
                .watch(&path, notify::RecursiveMode::NonRecursive)
                .is_ok()
            {
                self.watcher = Some(watcher);
            }
        }
    }
    fn load(&mut self) {
        self.tabs[self.active].end_rectangle(true);
        self.disk_refresh_due = Instant::now();
        self.search_due = None;
        self.notice = None;
        let tab = &mut self.tabs[self.active];
        tab.cancellation.store(true, Ordering::Relaxed);
        tab.cancellation = Arc::new(AtomicBool::new(false));
        tab.generation += 1;
        tab.loading = true;
        tab.search_limited = false;
        let (id, generation, path, query, hidden, sort, descending, cancel) = (
            tab.id,
            tab.generation,
            tab.path.clone(),
            tab.query.clone(),
            tab.show_hidden,
            tab.sort,
            tab.descending,
            tab.cancellation.clone(),
        );
        let picker_filter = self
            .picker
            .as_ref()
            .map(|picker| (picker.request.clone(), picker.filter.clone()));
        let sender = self.sender.clone();
        self.render();
        tokio::task::spawn_blocking(move || {
            let mut trash_items = Vec::new();
            let mut resolved_path = None;
            let mut search_limited = false;
            let result = if path == Path::new("trash:") {
                trash::os_limited::list()
                    .map_err(|e| e.to_string())
                    .map(|items| {
                        let query = query.to_lowercase();
                        let mut entries: Vec<_> = items
                            .iter()
                            .filter(|item| {
                                item.name.to_string_lossy().to_lowercase().contains(&query)
                            })
                            .map(|item| {
                                let metadata = trash::os_limited::metadata(item).ok();
                                let is_dir = metadata.is_some_and(|m| m.size.entries().is_some());
                                Entry {
                                    path: PathBuf::from(&item.id),
                                    name: item.name.to_string_lossy().into_owned(),
                                    is_dir,
                                    is_symlink: false,
                                    size: metadata.and_then(|m| m.size.size()).unwrap_or(0),
                                    modified: (item.time_deleted >= 0).then(|| {
                                        SystemTime::UNIX_EPOCH
                                            + Duration::from_secs(item.time_deleted as u64)
                                    }),
                                    kind: item.original_parent.to_string_lossy().into(),
                                    icon_name: if is_dir { "folder" } else { "text-x-generic" }
                                        .into(),
                                    icon_names: rix_files::file_icons::names(
                                        Path::new(&item.name),
                                        is_dir,
                                        false,
                                    ),
                                }
                            })
                            .collect();
                        fs::sort_entries(&mut entries, sort, descending);
                        trash_items = items;
                        entries
                    })
            } else {
                std::fs::canonicalize(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|path| {
                        resolved_path = Some(path.clone());
                        if query.is_empty() {
                            fs::read_directory(&path, hidden, sort, descending, &cancel)
                                .map_err(|e| e.to_string())
                        } else {
                            let outcome =
                                fs::search_directory_with_status(&path, &query, hidden, &cancel)
                                    .map_err(|e| e.to_string())?;
                            search_limited = outcome.stop == fs::SearchStop::Deadline;
                            Ok(outcome.entries)
                        }
                    })
            };
            let result = result.map(|mut entries| {
                if let Some((request, filter)) = &picker_filter {
                    if !cancel.load(Ordering::Relaxed) {
                        picker::Matcher::new(request, filter.as_ref())
                            .filter(&mut entries, &cancel);
                    }
                }
                entries
            });
            if !cancel.load(Ordering::Relaxed) {
                let _ = sender.send(Message::Loaded {
                    id,
                    generation,
                    resolved_path,
                    result,
                    trash: trash_items,
                    search_limited,
                });
            }
        });
    }
    fn navigate(&mut self, path: PathBuf) {
        self.invalidate_properties_metadata();
        self.tabs[self.active].navigate(path);
        self.load();
        if let Some(w) = self.window.upgrade() {
            w.invoke_focus_files();
        }
    }
    fn add_tab(&mut self, path: PathBuf) {
        if self.tabs.len() >= 12 {
            self.notice = Some("Up to 12 tabs can be open; close a tab to add another".into());
            self.render();
            return;
        }
        self.invalidate_properties_metadata();
        self.tabs.push(Tab::new(
            self.next_id,
            path,
            self.prefs.show_hidden,
            self.prefs.sort,
        ));
        self.next_id += 1;
        self.active = self.tabs.len() - 1;
        self.load();
    }
    fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.invalidate_properties_metadata();
        if self.tabs.len() == 1 {
            self.tabs[0].cancellation.store(true, Ordering::Relaxed);
            if let Some(cancel) = &self.operation {
                cancel.store(true, Ordering::Relaxed);
            }
            let _ = slint::quit_event_loop();
            return;
        }
        if index >= self.tabs.len() {
            return;
        }
        self.tabs[index].cancellation.store(true, Ordering::Relaxed);
        self.tabs.remove(index);
        if index < self.active {
            self.active -= 1;
        } else {
            self.active = self.active.min(self.tabs.len() - 1);
        }
        self.render();
        self.watch();
    }
    fn error(&mut self, error: String) {
        self.dialog(4, "Could not complete action", error, vec![], String::new());
    }
    fn dialog(&mut self, kind: i32, title: &str, text: String, paths: Vec<PathBuf>, value: String) {
        self.invalidate_properties_metadata();
        self.present_dialog(kind, title, text, paths, value);
    }

    fn present_dialog(
        &mut self,
        kind: i32,
        title: &str,
        text: String,
        paths: Vec<PathBuf>,
        value: String,
    ) {
        self.cancel_properties_scan();
        self.tabs[self.active].end_rectangle(true);
        self.dialog_paths = paths;
        self.dialog_kind = kind;
        if let Some(w) = self.window.upgrade() {
            if w.get_dialog_kind() == 0 {
                w.global::<DialogBackdrop>().invoke_prepare();
            }
            w.set_dialog_title(i18n::tr(title));
            w.set_dialog_text(i18n::tr(&text));
            w.set_dialog_value(value.into());
            w.set_dialog_kind(kind);
            if kind == 1 || kind == 2 || kind == 6 {
                w.invoke_focus_dialog();
            }
        }
    }
    fn dismiss(&mut self) {
        self.invalidate_properties_metadata();
        self.cancel_properties_scan();
        self.properties_items.clear();
        self.properties_summary = None;
        self.dialog_kind = 0;
        self.dialog_paths.clear();
        self.application_path = None;
        self.application_choices = None;
        self.applications.clear();
        if let Some(w) = self.window.upgrade() {
            w.global::<DialogBackdrop>().invoke_clear();
            w.global::<ApplicationChooser>()
                .set_applications(model(Vec::<ChooserApplication>::new()));
            w.set_dialog_kind(0);
            w.invoke_focus_files();
        }
    }
    fn open(&mut self, index: usize) {
        let Some(e) = self.tabs[self.active].entries.get(index).cloned() else {
            return;
        };
        self.invalidate_properties_metadata();
        if self.tabs[self.active].path == Path::new("trash:") {
            self.notice = Some("Restore this item before opening it".into());
            self.render();
            return;
        }
        if e.is_dir {
            self.navigate(e.path);
        } else if self.picker.is_some() {
            if let Some(window) = self.window.upgrade() {
                window
                    .global::<FilePicker>()
                    .set_name(e.name.clone().into());
            }
            if self
                .picker
                .as_ref()
                .is_some_and(|picker| picker.request.mode == rix_portal::picker::PickerMode::Open)
            {
                self.accept_picker(false);
            }
        } else if rix_files::archive::supported(&e.path) {
            self.open_archive(e.path, String::new());
        } else {
            let sender = self.sender.clone();
            tokio::spawn(async move {
                if let Err(e) = opening::open_default(e.path).await {
                    let _ = sender.send(Message::Error(e));
                }
            });
        }
    }
    fn start_operation(&mut self, kind: Kind, paths: Vec<PathBuf>, destination: Option<PathBuf>) {
        if self.operation.is_some() || paths.is_empty() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.operation = Some(cancel.clone());
        if let Some(w) = self.window.upgrade() {
            w.set_operation_progress(0.);
            w.set_operation_message(i18n::tr("Preparing…"));
        }
        self.render();
        let sender = self.sender.clone();
        let window = self.window.clone();
        let progress = Arc::new(std::sync::Mutex::new(None::<operations::Progress>));
        let progress_worker = progress.clone();

        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(100),
            move || {
                if let Some(progress) = progress.lock().unwrap().take() {
                    if let Some(w) = window.upgrade() {
                        w.set_operation_progress(if progress.total > 0 {
                            progress.completed as f32 / progress.total as f32
                        } else {
                            0.
                        });
                        w.set_operation_message(
                            if let Some(name) = progress.message.strip_prefix("Copying ") {
                                translated("Copying {name}", &[("name", name.into())]).into()
                            } else {
                                progress.message.into()
                            },
                        );
                    }
                }
            },
        );
        OPERATION_TIMER.with(|t| *t.borrow_mut() = Some(timer));
        tokio::task::spawn_blocking(move || {
            let report = operations::execute(
                Request {
                    kind,
                    sources: paths,
                    destination,
                    conflict: ConflictPolicy::KeepBoth,
                },
                &cancel,
                |p| {
                    *progress_worker.lock().unwrap() = Some(p);
                },
            );
            let _ = sender.send(Message::Operation(report));
        });
    }
}
thread_local! {static OPERATION_TIMER:RefCell<Option<slint::Timer>>=const{RefCell::new(None)};}
mod controller;
mod undo;
fn main() -> anyhow::Result<()> {
    controller::run()
}
