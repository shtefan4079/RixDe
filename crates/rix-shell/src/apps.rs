pub use rix_services::applications::{find_app, scan_applications, AppInfo, ApplicationCatalog};
use std::path::PathBuf;

static CURRENT_ICON_THEME: std::sync::RwLock<String> = std::sync::RwLock::new(String::new());
static ICON_IMAGE_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
type IconCache<T> = std::collections::HashMap<(String, u32), Option<T>>;
static ICON_PATH_CACHE: std::sync::RwLock<Option<IconCache<PathBuf>>> =
    std::sync::RwLock::new(None);

pub fn set_active_icon_theme(name: &str) {
    *CURRENT_ICON_THEME
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = name.to_string();
    *ICON_PATH_CACHE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    if let Some(resolver) = ICON_RESOLVER.get() {
        *resolver
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = rix_assets::IconResolver::default();
    }
    ICON_IMAGE_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    crate::tray::invalidate_icons();
}

pub fn get_active_icon_theme() -> String {
    let t = CURRENT_ICON_THEME
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if t.is_empty() {
        "hicolor".to_string()
    } else {
        t
    }
}
pub fn invalidate_application_icons() {
    *ICON_PATH_CACHE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    if let Some(resolver) = ICON_RESOLVER.get() {
        *resolver
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = rix_assets::IconResolver::default();
    }
    ICON_IMAGE_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    crate::menu_model::invalidate_icons();
}

static ICON_RESOLVER: std::sync::OnceLock<std::sync::Mutex<rix_assets::IconResolver>> =
    std::sync::OnceLock::new();
pub fn resolve_icon_path(icon_name: &str) -> Option<PathBuf> {
    resolve_icon_at_size(icon_name, 48)
}
pub fn resolve_icon_at_size(icon_name: &str, size: u32) -> Option<PathBuf> {
    let key = (icon_name.to_owned(), size);
    if icon_name.is_empty() {
        return None;
    }
    {
        let guard = ICON_PATH_CACHE
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(hit) = guard.as_ref().and_then(|cache| cache.get(&key)) {
            return hit.clone();
        }
    }
    let result = ICON_RESOLVER
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .resolve(icon_name, &get_active_icon_theme(), size, None);
    {
        let mut guard = ICON_PATH_CACHE
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let cache = guard.get_or_insert_with(std::collections::HashMap::new);
        if cache.len() >= 2048 {
            cache.clear();
        }
        cache.insert(key, result.clone());
    }
    result
}

pub fn load_slint_image(icon_name: &str) -> Option<slint::Image> {
    load_icon_at_size(icon_name, 48)
}
pub fn load_icon_at_size(icon_name: &str, size: u32) -> Option<slint::Image> {
    let key = (icon_name.to_owned(), size);
    thread_local! {
        static IMAGE_CACHE: std::cell::RefCell<(u64, IconCache<slint::Image>)> = std::cell::RefCell::new((0, std::collections::HashMap::new()));
    }
    let generation = ICON_IMAGE_GENERATION.load(std::sync::atomic::Ordering::Relaxed);
    IMAGE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.0 != generation {
            cache.0 = generation;
            cache.1.clear();
        }
        if let Some(image) = cache.1.get(&key) {
            return image.clone();
        }
        let image =
            resolve_icon_at_size(icon_name, size)
                .and_then(|path| rix_assets::rasterize_icon(&path, size.clamp(1, 512)).ok())
                .map(|tile| {
                    slint::Image::from_rgba8_premultiplied(slint::SharedPixelBuffer::<
                        slint::Rgba8Pixel,
                    >::clone_from_slice(
                        &tile.pixels, tile.width, tile.height
                    ))
                });
        cache.1.insert(key, image.clone());
        image
    })
}

pub fn convert_apps_to_menu_data(apps: &[AppInfo], size: u32) -> Vec<crate::AppEntryData> {
    apps.iter()
        .map(|app| {
            let icon = load_icon_at_size(&app.icon_name, size);
            let has_icon = icon.is_some();
            let badge = app.name.chars().next().unwrap_or(' ').to_string();
            crate::AppEntryData {
                name: app.name.clone().into(),

                exec: app.desktop_id.clone().into(),
                badge: badge.into(),
                icon: icon.unwrap_or_default(),
                has_icon,
            }
        })
        .collect()
}

pub fn task_icon_size(window: &crate::RixPanelWindow) -> u32 {
    use slint::ComponentHandle;
    let theme = window.global::<crate::Theme>();
    let logical = theme
        .get_panel_icon_size()
        .min(
            window.get_thickness() as f32
                - theme.get_panel_vertical_padding() * 2.
                - theme.get_task_padding() * 2.,
        )
        .min(theme.get_task_width() - theme.get_task_padding() * 2.)
        .max(1.);
    (logical * window.window().scale_factor())
        .round()
        .clamp(1., 512.) as u32
}
