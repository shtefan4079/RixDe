mod autostart;
mod behavior;
mod bundled_themes;
mod clavis;
mod default_apps;
mod dialogs;
mod display;
#[path = "../../../ui/common/i18n.rs"]
mod i18n;
mod jobs;
mod keyboard;
mod language;
mod plugins;
mod power;
mod theme_editor;
mod wallpaper;
mod window_appearance;
type SystemHost = SettingsWindow;
mod panel_editor;
#[path = "../../../ui/common/system_bridge.rs"]
mod system_bridge;
slint::include_modules!();
mod launcher_editor;
#[path = "../../../ui/common/skins.rs"]
mod skins;
mod style_editor;
#[path = "../../../ui/common/bindings.rs"]
mod theme_bindings;
#[cfg(test)]
mod ui_tests;

pub mod hub;
pub mod scanners;

use hub::{
    export_theme_zip, get_themes_export_dir, import_theme_zip, resolve_export_path,
    resolve_import_path, sanitize_theme_name, ThemeComponentsSelection,
};
use rix_config::model::{ThemeSettings, WidgetKind};
use rix_config::{ConfigManager, LayoutConfig, PlasmaSnapshot};
use rix_ipc::{IpcClient, IpcRequest};
use scanners::{scan_cursor_themes, scan_icon_themes};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use tracing::info;

fn send_ipc_fire_and_forget(req: IpcRequest) {
    tokio::spawn(async move {
        match IpcClient::connect().await {
            Ok(mut client) => {
                if let Err(e) = client.send(req).await {
                    tracing::warn!("Settings IPC send failed: {}", e);
                }
            }
            Err(e) => tracing::warn!("Settings IPC connect failed: {}", e),
        }
    });
}

async fn send_plasma_snapshot_ipc(snapshot: &PlasmaSnapshot) -> Result<(), String> {
    let mut client = IpcClient::connect().await.map_err(|err| err.to_string())?;
    client
        .send(IpcRequest::SetLayout(snapshot.layout.clone()))
        .await
        .map_err(|err| err.to_string())?;
    client
        .send(IpcRequest::SetTheme(snapshot.theme.clone()))
        .await
        .map_err(|err| err.to_string())?;
    Ok(())
}

fn send_display_sequence(
    width: u32,
    height: u32,
    refresh_hz: u32,
    flip: bool,
    handle: slint::Weak<SettingsWindow>,
) {
    tokio::spawn(async move {
        let outcome = async {
            let mut client = IpcClient::connect().await.map_err(|e| e.to_string())?;
            client
                .send(IpcRequest::SetDisplayMode {
                    width,
                    height,
                    refresh_hz,
                })
                .await
                .map_err(|e| e.to_string())?;
            if flip {
                client
                    .send(IpcRequest::FlipOutput)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Ok::<(), String>(())
        }
        .await;
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = handle.upgrade() {
                match outcome {
                    Ok(()) => w.set_status_message(
                        format!(
                            "{}: {}×{} @ {} Hz",
                            i18n::tr("Display updated"),
                            width,
                            height,
                            refresh_hz
                        )
                        .into(),
                    ),
                    Err(err) => w.set_status_message(
                        format!("{}: {err}", i18n::tr("Could not change the display")).into(),
                    ),
                }
            }
        });
    });
}

fn clamp_f32(v: f32, lo: f32, hi: f32) -> f32 {
    if !v.is_finite() {
        lo
    } else {
        v.clamp(lo, hi)
    }
}

fn is_valid_hex_color(s: &str) -> bool {
    let hex = s.strip_prefix('#').unwrap_or(s);
    (hex.len() == 6 || hex.len() == 8) && hex.chars().all(|c| c.is_ascii_hexdigit())
}

fn resolve_tab_icon(
    resolver: &mut rix_assets::IconResolver,
    names: &[&str],
    theme: &str,
    is_dark: bool,
    size: u32,
) -> slint::Image {
    let themes: &[&str] = if is_dark {
        &[theme, "breeze-dark", "breeze", "Adwaita", "hicolor"]
    } else {
        &[theme, "breeze", "Adwaita", "hicolor", "breeze-dark"]
    };
    for &t in themes {
        for &name in names {
            if let Some(path) = resolver.resolve(name, t, size, None) {
                if let Ok(img) = slint::Image::load_from_path(&path) {
                    return img;
                }
            }
        }
    }
    slint::Image::default()
}

#[derive(Clone, Copy)]
pub(crate) struct NavItemDef {
    pub tab_id: i32,
    pub name: &'static str,
    pub desc: &'static str,
    pub icon: &'static [&'static str],
    pub section: &'static str,
    pub is_first: bool,
}

pub(crate) const NAV_ITEMS: &[NavItemDef] = &[

    NavItemDef { tab_id: 0, name: "Appearance & style", desc: "Installed themes, blur shaders, accent colors and glass effects", icon: &["preferences-desktop-theme", "preferences-desktop-appearance", "style"], section: "Personalization", is_first: true },
    NavItemDef { tab_id: 11, name: "Desktop wallpaper", desc: "Background wallpaper image and slideshow settings", icon: &["preferences-desktop-wallpaper", "background", "image-x-generic"], section: "Personalization", is_first: false },
    NavItemDef { tab_id: 1, name: "Icons and cursor", desc: "Application icon set, pointer style and cursor size", icon: &["preferences-desktop-icons", "preferences-desktop-theme", "applications-graphics"], section: "Personalization", is_first: false },
    NavItemDef { tab_id: 4, name: "Panel and menu", desc: "Taskbar geometry, widgets and application launcher menu", icon: &["preferences-desktop", "system-run", "applications-system"], section: "Personalization", is_first: false },
    NavItemDef { tab_id: 3, name: "Theme backup & profiles", desc: "Export and import theme archive packages and profiles", icon: &["preferences-desktop-theme-global", "preferences-desktop-color", "document-save"], section: "Personalization", is_first: false },
    NavItemDef { tab_id: 17, name: "Window appearance", desc: "Window frames, shadow effects and per-application rules", icon: &["preferences-system-windows", "preferences-desktop-theme", "window-duplicate"], section: "Personalization", is_first: false },

    NavItemDef { tab_id: 2, name: "Display", desc: "Screen resolution, refresh rate and multi-monitor setup", icon: &["preferences-desktop-display", "video-display", "preferences-system-windows"], section: "Hardware and system", is_first: true },
    NavItemDef { tab_id: 7, name: "Audio", desc: "Sound volume, input/output audio devices and amplification", icon: &["audio-volume-high", "audio-card", "multimedia-volume-control"], section: "Hardware and system", is_first: false },
    NavItemDef { tab_id: 8, name: "Network and internet", desc: "Wi-Fi connections, wired ethernet and network profiles", icon: &["network-wireless", "network-wired", "network-workgroup"], section: "Hardware and system", is_first: false },
    NavItemDef { tab_id: 16, name: "Power", desc: "Battery levels, energy saving and sleep behavior", icon: &["preferences-system-power-management", "battery", "power"], section: "Hardware and system", is_first: false },
    NavItemDef { tab_id: 6, name: "Keyboard", desc: "Keyboard layouts, switching shortcuts and repeat rate", icon: &["input-keyboard", "keyboard", "preferences-desktop-keyboard"], section: "Hardware and system", is_first: false },

    NavItemDef { tab_id: 10, name: "Desktop behavior", desc: "Workspaces, window management and desktop interaction", icon: &["configure", "preferences-system", "settings"], section: "Applications and behavior", is_first: true },
    NavItemDef { tab_id: 9, name: "Autostart", desc: "Applications launched automatically at user login", icon: &["applications-all", "system-run", "system-software-install"], section: "Applications and behavior", is_first: false },
    NavItemDef { tab_id: 15, name: "Default applications", desc: "Preferred web browser, file manager, terminal and editors", icon: &["preferences-desktop-default-applications", "applications-system", "preferences-system"], section: "Applications and behavior", is_first: false },
    NavItemDef { tab_id: 12, name: "Extensions", desc: "System plugins, capture studio and modular extensions", icon: &["preferences-plugin", "application-x-addon", "extension"], section: "Applications and behavior", is_first: false },

    NavItemDef { tab_id: 18, name: "Passwords and keys", desc: "Keyring credentials, Secret Service and security vaults", icon: &["dialog-password", "security-high", "auth-sim"], section: "Security and locale", is_first: true },
    NavItemDef { tab_id: 14, name: "Language & region", desc: "System language, translation catalogs and regional formats", icon: &["preferences-desktop-locale", "config-language", "locale"], section: "Security and locale", is_first: false },

    NavItemDef { tab_id: 13, name: "About RixDE", desc: "System version, desktop components and licenses", icon: &["help-about", "help-browser", "system-help", "dialog-information"], section: "Information", is_first: true },
];

pub(crate) fn refresh_navigation(window: &SettingsWindow, icon_theme: &str) {
    refresh_navigation_filtered(window, icon_theme, "");
}

pub(crate) fn refresh_navigation_filtered(window: &SettingsWindow, icon_theme: &str, query: &str) {
    let is_dark = window.get_color_mode() != "light";
    let mut resolver = rix_assets::IconResolver::default();
    let q = query.trim().to_lowercase();
    let is_filtering = !q.is_empty();

    let items: Vec<NavigationItem> = NAV_ITEMS
        .iter()
        .filter(|item| {
            if !is_filtering {
                return true;
            }
            let name_tr = i18n::tr(item.name).to_lowercase();
            let desc_tr = i18n::tr(item.desc).to_lowercase();
            let sec_tr = i18n::tr(item.section).to_lowercase();
            name_tr.contains(&q)
                || item.name.to_lowercase().contains(&q)
                || desc_tr.contains(&q)
                || item.desc.to_lowercase().contains(&q)
                || sec_tr.contains(&q)
                || item.section.to_lowercase().contains(&q)
        })
        .map(|item| NavigationItem {
            tab_id: item.tab_id,
            name: i18n::tr(item.name),
            section: if is_filtering { "".into() } else { i18n::tr(item.section) },
            is_first_in_section: if is_filtering { false } else { item.is_first },
            icon: resolve_tab_icon(&mut resolver, item.icon, icon_theme, is_dark, 24),
        })
        .collect();

    window.set_navigation_items(ModelRc::from(Rc::new(VecModel::from(items))));
    if !is_filtering {
        update_active_tab_header(window);
    }
}

pub(crate) fn update_active_tab_header(window: &SettingsWindow) {
    let active = window.get_active_tab();
    if let Some(def) = NAV_ITEMS.iter().find(|item| item.tab_id == active) {
        window.set_active_tab_title(i18n::tr(def.name));
        window.set_active_tab_description(i18n::tr(def.desc));
        window.set_active_tab_section(i18n::tr(def.section));
    }
}

fn sync_theme_to_ui(window: &SettingsWindow, theme: &ThemeSettings) {

    if window.get_navigation_icon_theme() != theme.icon_theme {
        refresh_navigation(window, &theme.icon_theme);
        window.set_navigation_icon_theme(theme.icon_theme.clone().into());
    }

    skins::install(window.global::<SkinRenderer>(), window.as_weak());
    theme_bindings::apply(window.global::<Theme>(), window.global::<Palette>(), theme);
    style_editor::refresh(window, theme);
    window.set_panel_opacity(theme.opacity);
    window.set_blur_radius(theme.blur_radius as f32);
    window.set_noise_opacity(theme.noise_opacity);
    window.set_is_aero(theme.blur_radius > 0);
    window.set_accent_color(theme.accent_color.clone().into());
    window.set_cursor_size(theme.cursor_size as i32);
    if let Some(index) = window
        .get_icon_themes()
        .iter()
        .position(|name| name == theme.icon_theme)
    {
        window.set_selected_icon_index(index as i32);
    }
    if let Some(index) = window
        .get_cursor_themes()
        .iter()
        .position(|name| name == theme.cursor_theme)
    {
        window.set_selected_cursor_index(index as i32);
    }
    let e = theme.effects;
    window.set_blur_algorithm(
        if e.algorithm == rix_config::appearance::BlurAlgorithm::DualKawase {
            0
        } else {
            1
        },
    );
    window.set_blur_iterations(e.iterations as i32);
    window.set_blur_offset(e.offset);
    window.set_gaussian_downsample(e.gaussian_downsample as i32);
    window.set_noise_scale(e.noise_scale);
    window.set_noise_bias(e.noise_bias);
    window.set_material_brightness(e.brightness);
    window.set_material_contrast(e.contrast);
    window.set_material_saturation(e.saturation);

    let name_lower = theme.name.to_lowercase();
    let selected_theme = if name_lower.contains("crisp") {
        "crisp"
    } else if name_lower.contains("cyber") {
        "cyber"
    } else {
        "fairywren"
    };
    let mode = if name_lower.contains("oled")
        || theme
            .appearance
            .palette
            .get("background")
            .map(|s| s == "#000000")
            .unwrap_or(false)
    {
        "oled"
    } else if name_lower.contains("light") || !theme.appearance.prefers_dark() {
        "light"
    } else {
        "dark"
    };
    window.set_selected_theme(selected_theme.into());
    window.set_color_mode(mode.into());
    let active_name = match selected_theme {
        "crisp" => "Crisp",
        "cyber" => "Cyber",
        _ => "FairyWren",
    };
    window.set_active_theme_name(active_name.into());
    refresh_navigation(window, &theme.icon_theme);
}

pub(crate) fn get_theme_for(theme_id: &str, mode_id: &str) -> ThemeSettings {
    let (mut theme, display_name) = match theme_id {
        "crisp" => (ThemeSettings::crisp_light(), "Crisp"),
        "cyber" => (ThemeSettings::cyber_dark(), "Cyber"),
        _ => (ThemeSettings::fairywren_dark(), "FairyWren"),
    };

    match mode_id {
        "light" => {
            theme.name = format!("{display_name} Light");
            let light_ref = ThemeSettings::crisp_light();
            theme.appearance.palette = light_ref.appearance.palette;
            theme.opacity = 0.90;
            match theme_id {
                "fairywren" => {
                    theme.accent_color = "#5271ff".into();
                    theme.appearance.palette.insert("accent".into(), "#5271ff".into());
                    theme.appearance.palette.insert("accent_hover".into(), "#6b85ff".into());
                    theme.blur_radius = 24;
                }
                "crisp" => {
                    theme.accent_color = "#2563eb".into();
                    theme.appearance.palette.insert("accent".into(), "#2563eb".into());
                    theme.appearance.palette.insert("accent_hover".into(), "#3b82f6".into());
                    theme.blur_radius = 20;
                }
                "cyber" => {
                    theme.accent_color = "#8b5cf6".into();
                    theme.appearance.palette.insert("accent".into(), "#8b5cf6".into());
                    theme.appearance.palette.insert("accent_hover".into(), "#a78bfa".into());
                    theme.blur_radius = 16;
                }
                _ => {}
            }
        }
        "oled" => {
            theme.name = format!("{display_name} OLED");
            let mut oled_palette = ThemeSettings::cyber_dark().appearance.palette;
            oled_palette.insert("background".into(), "#000000".into());
            oled_palette.insert("window_background".into(), "#000000".into());
            oled_palette.insert("sidebar_background".into(), "#050505".into());
            oled_palette.insert("surface".into(), "#0a0a0af5".into());
            oled_palette.insert("menu_bg".into(), "#000000".into());
            oled_palette.insert("glass_start".into(), "#000000".into());
            oled_palette.insert("glass_end".into(), "#050505".into());
            oled_palette.insert("control_surface".into(), "#111111".into());
            oled_palette.insert("control_border".into(), "#ffffff18".into());
            oled_palette.insert("border".into(), "#ffffff22".into());
            oled_palette.insert("divider".into(), "#ffffff1a".into());
            theme.appearance.palette = oled_palette;
            theme.opacity = 0.98;
            match theme_id {
                "fairywren" => {
                    theme.accent_color = "#5271ff".into();
                    theme.appearance.palette.insert("accent".into(), "#5271ff".into());
                    theme.appearance.palette.insert("accent_hover".into(), "#6b85ff".into());
                    theme.blur_radius = 24;
                }
                "crisp" => {
                    theme.accent_color = "#2563eb".into();
                    theme.appearance.palette.insert("accent".into(), "#2563eb".into());
                    theme.appearance.palette.insert("accent_hover".into(), "#3b82f6".into());
                    theme.blur_radius = 20;
                }
                "cyber" => {
                    theme.accent_color = "#8b5cf6".into();
                    theme.appearance.palette.insert("accent".into(), "#8b5cf6".into());
                    theme.appearance.palette.insert("accent_hover".into(), "#a78bfa".into());
                    theme.blur_radius = 16;
                }
                _ => {}
            }
        }
        _ => {

            theme.name = format!("{display_name} Dark");
            let dark_ref = ThemeSettings::fairywren_dark();
            theme.appearance.palette = dark_ref.appearance.palette;
            theme.opacity = 0.82;
            match theme_id {
                "fairywren" => {
                    theme.accent_color = "#5271ff".into();
                    theme.appearance.palette.insert("accent".into(), "#5271ff".into());
                    theme.appearance.palette.insert("accent_hover".into(), "#6b85ff".into());
                    theme.blur_radius = 24;
                }
                "crisp" => {
                    theme.accent_color = "#2563eb".into();
                    theme.appearance.palette.insert("accent".into(), "#2563eb".into());
                    theme.appearance.palette.insert("accent_hover".into(), "#3b82f6".into());
                    theme.blur_radius = 20;
                }
                "cyber" => {
                    theme.accent_color = "#8b5cf6".into();
                    theme.appearance.palette.insert("accent".into(), "#8b5cf6".into());
                    theme.appearance.palette.insert("accent_hover".into(), "#a78bfa".into());
                    theme.blur_radius = 16;
                }
                _ => {}
            }
        }
    }

    theme
}

fn sync_layout_to_ui(window: &SettingsWindow, layout: &LayoutConfig) {
    window.set_panels(ModelRc::new(VecModel::from(
        layout
            .panels
            .iter()
            .enumerate()
            .map(|(index, _)| format!("{} {}", i18n::tr("Panel"), index + 1).into())
            .collect::<Vec<SharedString>>(),
    )));
    let selected =
        (window.get_selected_panel().max(0) as usize).min(layout.panels.len().saturating_sub(1));
    window.set_selected_panel(selected as i32);
    if let Some(panel) = layout.panels.get(selected) {
        window.set_panel_thickness(panel.thickness as i32);
        window.set_panel_margin(panel.floating_margin as i32);
        window.set_panel_radius(panel.corner_radius as i32);
        window.set_panel_theme_frame(panel.use_theme_frame);
        window.set_panel_floating(panel.floating);
        window.set_panel_screen(panel.screen.clone().into());
        let outputs = window
            .get_panel_screen_ids()
            .iter()
            .filter(|id| id.as_str() != "primary" && id.as_str() != "all")
            .map(|id| id.to_string())
            .collect::<Vec<_>>();
        panel_editor::set_outputs(window, &outputs);
        window.set_panel_position(match panel.position {
            rix_core::Position::Bottom => 0,
            rix_core::Position::Top => 1,
            _ => -1,
        });
        window.set_panel_alignment(match panel.alignment {
            rix_core::Alignment::Start => 0,
            rix_core::Alignment::Center => 1,
            rix_core::Alignment::End => 2,
            _ => -1,
        });
        window.set_panel_length_mode(match panel.length {
            rix_core::PanelLength::Fill => 0,
            rix_core::PanelLength::FitContent => 1,
            rix_core::PanelLength::Fixed(_) => 2,
        });
        if let rix_core::PanelLength::Fixed(length) = panel.length {
            window.set_panel_length(length as i32);
        }
        window.set_panel_exclusive(panel.exclusive_zone);
        panel_editor::load(window, panel);
        window.set_taskbar_labels(
            panel
                .widgets
                .iter()
                .find(|widget| widget.kind == WidgetKind::Taskbar)
                .and_then(|widget| widget.properties.get("show-labels"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        );
    }
    window.set_launcher_width(layout.launcher.width as i32);
    window.set_launcher_alignment(match layout.launcher.alignment {
        rix_core::Alignment::Start => 0,
        rix_core::Alignment::End => 2,
        _ => 1,
    });
    window.set_launcher_height(layout.launcher.height as i32);
    window.set_launcher_icon(layout.launcher.icon.clone().into());
    launcher_editor::load(window, &layout.launcher.footer);
}

pub(crate) fn refresh_localized_models(window: &SettingsWindow, config: Arc<ConfigManager>) {
    refresh_navigation(window, &window.get_navigation_icon_theme());
    window.set_window_title(i18n::tr("RixDE Settings"));
    panel_editor::refresh_labels(window);
    default_apps::refresh(window, config.clone());
    plugins::refresh(window, &config);
    wallpaper::refresh_localized_labels(window);
    window
        .global::<OutputPreferences>()
        .invoke_select_resolution();
}

fn plasma_config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"))
}

fn back_up_if_present(path: &Path, stamp: &str) -> anyhow::Result<()> {
    if path.exists() {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config");
        let backup = path.with_file_name(format!("{file_name}.before-plasma-{stamp}"));
        fs::copy(path, backup)?;
    }
    Ok(())
}

fn import_plasma_into(config: &ConfigManager) -> anyhow::Result<PlasmaSnapshot> {
    let snapshot = PlasmaSnapshot::read_from(&plasma_config_dir(), config.get_theme())?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%3f").to_string();
    for file in ["layout.kdl", "theme.json", "pinned.json"] {
        back_up_if_present(&config.config_dir().join(file), &stamp)?;
    }
    let pins = config.config_dir().join("pinned.json");
    let pins_temp = config.config_dir().join(format!(".pinned-{stamp}.tmp"));
    fs::write(
        &pins_temp,
        serde_json::to_vec_pretty(&snapshot.pinned_apps)?,
    )?;
    fs::rename(&pins_temp, &pins)?;
    config.save_layout(snapshot.layout.clone())?;
    config.save_theme(snapshot.theme.clone())?;
    Ok(snapshot)
}

fn scan_available_zip_archives() -> Vec<String> {
    let dir = get_themes_export_dir();
    let mut list = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();

            let is_zip = path.is_file()
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("zip"))
                    .unwrap_or(false);
            if is_zip {
                if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                    list.push(file_name.to_string());
                }
            }
        }
    }
    list.sort();
    list
}

fn create_settings_window() -> Result<SettingsWindow, slint::PlatformError> {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("femtovg".into())
        .require_opengl_es()
        .select()?;
    slint::set_xdg_app_id("org.rixde.Settings")?;
    let window = SettingsWindow::new()?;
    if let Some(page) =
        std::env::args().find_map(|arg| arg.strip_prefix("--page=").map(str::to_owned))
    {
        let page_name = page.split_once(':').map_or(page.as_str(), |(name, _)| name);
        window.set_active_tab(match page_name {
            "display" => 2,
            "panel" => 4,
            "keyboard" => 6,
            "audio" => 7,
            "network" => 8,
            "autostart" => 9,
            "wallpaper" => 11,
            "plugins" => 12,
            "behavior" => 10,
            "power" => 16,
            "window-appearance" => 17,
            "clavis" | "passwords" => 18,
            "about" => 13,
            "language" => 14,
            "default-apps" => 15,
            _ => 0,
        });
        if let Some(target) = page.strip_prefix("network:") {
            window
                .global::<SystemState>()
                .set_network_editor_request(target.into());
        }
    }

    window.window().set_size(slint::LogicalSize::new(
        window.get_initial_width(),
        window.get_initial_height(),
    ));
    Ok(window)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    info!("Starting Rix Settings Application...");

    let config_manager = Arc::new(ConfigManager::init()?);
    let theme_editor = theme_editor::State::new(config_manager.get_theme());
    if std::env::args().any(|arg| arg == "--import-plasma") {
        let snapshot = import_plasma_into(&config_manager)?;
        if let Err(err) = send_plasma_snapshot_ipc(&snapshot).await {
            tracing::warn!("Plasma settings saved; no live session to notify: {}", err);
        }
        println!(
            "{} {}",
            i18n::tr("The Plasma panel and launcher were imported into"),
            config_manager.config_dir().display()
        );
        return Ok(());
    }
    let window = create_settings_window()?;
    if let Err(error) = i18n::select_language(&config_manager.get_desktop().language) {
        tracing::warn!(%error, "Could not select the configured interface language");
    }
    refresh_navigation(&window, &config_manager.get_theme().icon_theme);
    window.set_window_title(i18n::tr("RixDE Settings"));
    let weak = window.as_weak();
    window.on_active_tab_changed(move |_| {
        if let Some(w) = weak.upgrade() {
            update_active_tab_header(&w);
        }
    });
    dialogs::install(&window);
    system_bridge::install(&window, config_manager.clone());
    window
        .global::<SystemState>()
        .invoke_network_visible(window.get_active_tab() == 8);
    let weak = window.as_weak();
    window
        .global::<SystemState>()
        .on_network_show_prompt(move || {
            if let Some(window) = weak.upgrade() {
                window.set_active_tab(8);
                window.window().set_minimized(false);
                let _ = window.show();
            }
        });
    keyboard::install(&window, config_manager.clone());
    language::install(&window, config_manager.clone());
    power::install(&window, config_manager.clone());
    behavior::install_with_theme(&window, config_manager.clone(), theme_editor.clone());
    window_appearance::install(&window, config_manager.clone());
    default_apps::install(&window, config_manager.clone());
    clavis::install(&window);
    autostart::install(&window, config_manager.clone());
    wallpaper::install(&window, config_manager.clone());
    let _output_timer = display::install(&window);
    let _plugin_timer = plugins::install(&window, config_manager.clone());
    let weak = window.as_weak();
    window
        .global::<SystemState>()
        .on_open_settings(move |page| {
            if let Some(window) = weak.upgrade() {
                window.set_active_tab(match page.as_str() {
                    "keyboard" => 6,
                    "audio" => 7,
                    "network" => 8,
                    "clavis" | "passwords" => 18,
                    _ => 0,
                });
            }
        });
    let weak = window.as_weak();
    window
        .global::<SystemState>()
        .on_keyboard_settings(move || {
            if let Some(window) = weak.upgrade() {
                window.set_active_tab(6);
            }
        });
    window
        .global::<SystemState>()
        .invoke_network_visible(window.get_active_tab() == 8);
    let panel_editor = panel_editor::install(&window, config_manager.clone());
    launcher_editor::install(&window);
    style_editor::install(&window, theme_editor.clone());
    bundled_themes::install(&window, theme_editor.clone());

    let initial_theme = config_manager.get_theme();
    let initial_display = config_manager.get_display();
    panel_editor.load(&window, &config_manager.get_layout());

    theme_editor.load(&window, &initial_theme);

    let scanned_icons = scan_icon_themes();
    let selected_icon_idx = scanned_icons
        .iter()
        .position(|t| t == &initial_theme.icon_theme)
        .unwrap_or(0) as i32;
    let icon_models: Vec<SharedString> = scanned_icons.iter().map(|s| s.as_str().into()).collect();
    window.set_icon_themes(ModelRc::from(Rc::new(VecModel::from(icon_models))));
    window.set_selected_icon_index(selected_icon_idx);

    let scanned_cursors = scan_cursor_themes();
    let selected_cursor_idx = scanned_cursors
        .iter()
        .position(|c| c == &initial_theme.cursor_theme)
        .unwrap_or(0) as i32;
    let cursor_models: Vec<SharedString> =
        scanned_cursors.iter().map(|s| s.as_str().into()).collect();
    window.set_cursor_themes(ModelRc::from(Rc::new(VecModel::from(cursor_models))));
    window.set_selected_cursor_index(selected_cursor_idx);
    window.set_cursor_size(initial_theme.cursor_size as i32);

    window.set_display_width(initial_display.width as i32);
    window.set_display_height(initial_display.height as i32);
    window.set_display_hz(initial_display.refresh_hz as i32);
    window.set_display_flipped(initial_display.flipped);

    let archives = scan_available_zip_archives();
    let archive_models: Vec<SharedString> = archives.iter().map(|s| s.as_str().into()).collect();
    window.set_available_archives(ModelRc::from(Rc::new(VecModel::from(archive_models))));

    window.set_version(env!("CARGO_PKG_VERSION").into());

    let cm = config_manager.clone();
    let editor = theme_editor.clone();
    let h = window.as_weak();
    window.on_apply_preset(move |preset_name| {
        info!("Applying preset: {}", preset_name);
        let (mode, mut new_theme) = match preset_name.as_str() {
            "aero_glass" => ("dark", ThemeSettings::aero_glass()),
            "cyber_dark" | "oled" => ("oled", ThemeSettings::cyber_dark()),
            "crisp_light" | "light" => ("light", ThemeSettings::crisp_light()),
            "fairywren" | "fairywren_dark" | "fairywren-dark" | "dark" => ("dark", ThemeSettings::fairywren_dark()),
            other => {
                if let Some(w) = h.upgrade() {
                    w.set_status_message(format!("{}: {other}", i18n::tr("Unknown preset")).into());
                }
                return;
            }
        };
        if let Some(w) = h.upgrade() {
            w.set_color_mode(mode.into());
        }

        let curr = cm.get_theme();
        new_theme.icon_theme = curr.icon_theme;
        new_theme.cursor_theme = curr.cursor_theme;
        new_theme.cursor_size = curr.cursor_size;

        if let Some(w) = h.upgrade() {
            jobs::save_theme(&w, editor.clone(), new_theme);
        }
    });

    let cm = config_manager.clone();
    let editor = theme_editor.clone();
    let h = window.as_weak();
    window.on_select_theme_and_mode(move |theme_id, mode_id| {
        info!("Select theme: {}, mode: {}", theme_id, mode_id);
        let mut new_theme = get_theme_for(&theme_id, &mode_id);
        let curr = cm.get_theme();
        new_theme.icon_theme = curr.icon_theme;
        new_theme.cursor_theme = curr.cursor_theme;
        new_theme.cursor_size = curr.cursor_size;
        if let Some(w) = h.upgrade() {
            w.set_selected_theme(theme_id.clone());
            w.set_color_mode(mode_id.clone());
            jobs::save_theme(&w, editor.clone(), new_theme);
        }
    });

    let editor = theme_editor.clone();
    let h = window.as_weak();
    window.on_apply_appearance(move |opacity, blur, noise, is_aero, accent| {
        info!(
            "Applying appearance: opacity={}, blur={}, noise={}",
            opacity, blur, noise
        );
        let mut curr = editor.baseline();
        curr.opacity = clamp_f32(opacity, 0.0, 1.0);
        let blur_clamped = clamp_f32(blur, 1.0, 128.0) as u32;
        curr.blur_radius = if is_aero { blur_clamped } else { 0 };
        curr.noise_opacity = clamp_f32(noise, 0.0, 0.5);
        if let Some(w) = h.upgrade() {
            curr.effects = rix_config::appearance::BackdropSettings {
                algorithm: if w.get_blur_algorithm() == 0 {
                    rix_config::appearance::BlurAlgorithm::DualKawase
                } else {
                    rix_config::appearance::BlurAlgorithm::Gaussian
                },
                iterations: w.get_blur_iterations() as u32,
                offset: w.get_blur_offset(),
                gaussian_downsample: w.get_gaussian_downsample() as u32,
                noise_scale: w.get_noise_scale(),
                noise_bias: w.get_noise_bias(),
                brightness: w.get_material_brightness(),
                contrast: w.get_material_contrast(),
                saturation: w.get_material_saturation(),
            };
        }
        let accent_str = accent.to_string();
        if is_valid_hex_color(&accent_str) {
            curr.accent_color = accent_str;
        } else if let Some(w) = h.upgrade() {
            w.set_status_message(
                format!(
                    "{}: {accent_str}",
                    i18n::tr("Invalid accent; keeping the previous value")
                )
                .into(),
            );
        }

        if let Some(w) = h.upgrade() {
            jobs::save_theme(&w, editor.clone(), curr);
        }
    });

    let editor = theme_editor.clone();
    let h = window.as_weak();
    window.on_apply_icons_cursor(move |icon_theme, cursor_theme, cursor_size| {
        info!(
            "Applying icons: '{}', cursor: '{}', size: {}",
            icon_theme, cursor_theme, cursor_size
        );
        let mut curr = editor.baseline();
        curr.icon_theme = icon_theme.to_string();
        curr.cursor_theme = cursor_theme.to_string();
        curr.cursor_size = (cursor_size as u32).clamp(8, 96);

        if let Some(w) = h.upgrade() {
            jobs::save_theme(&w, editor.clone(), curr);
        }
    });

    let cm = config_manager.clone();
    let h = window.as_weak();
    window.on_apply_display(move |width, height, hz, flipped| {
        info!(
            "Applying display mode: {}x{}@{}Hz flipped={}",
            width, height, hz, flipped
        );
        let disp_w = (width as u32).clamp(640, 7680);
        let disp_h = (height as u32).clamp(480, 4320);
        let disp_hz = (hz as u32).clamp(30, 360);
        let old_flipped = cm.get_display().flipped;
        if let Some(w) = h.upgrade() {
            w.set_status_message(i18n::tr("Applying display settings…"));
        }
        send_display_sequence(disp_w, disp_h, disp_hz, old_flipped != flipped, h.clone());
    });

    let cm = config_manager.clone();
    let h = window.as_weak();
    window.on_export_theme_zip(
        move |theme_name, inc_panel, inc_colors, inc_icons, inc_cursor| {
            let name_clean = sanitize_theme_name(&theme_name);
            let export_dir = get_themes_export_dir();
            let target_zip = resolve_export_path(&export_dir, &name_clean);

            let selection = ThemeComponentsSelection {
                panel: inc_panel,
                colors: inc_colors,
                icons: inc_icons,
                cursor: inc_cursor,
            };

            let curr_theme = cm.get_theme();

            let Some(w) = h.upgrade() else { return };
            let cm = cm.clone();
            jobs::run(
                &w,
                move || {
                    export_theme_zip(&target_zip, &name_clean, selection, &curr_theme, &cm)?;
                    Ok((target_zip, scan_available_zip_archives()))
                },
                |w, (path, archives)| {
                    w.set_available_archives(ModelRc::from(Rc::new(VecModel::from(
                        archives
                            .into_iter()
                            .map(SharedString::from)
                            .collect::<Vec<_>>(),
                    ))));
                    w.set_status_message(
                        format!("{}: {}", i18n::tr("Theme saved"), path.display()).into(),
                    );
                },
            );
        },
    );

    let cm = config_manager.clone();
    let h = window.as_weak();
    let editor = panel_editor.clone();
    let theme_state = theme_editor.clone();
    window.on_import_theme_zip(move |archive_name| {
        let export_dir = get_themes_export_dir();
        let selected = std::path::PathBuf::from(archive_name.as_str());
        let candidate = if selected.is_absolute() {
            Some(selected)
        } else {
            resolve_import_path(&export_dir, archive_name.as_str())
        };
        let target_zip = match candidate {
            Some(p) => p,
            None => {
                if let Some(w) = h.upgrade() {
                    w.set_status_message(
                        format!("{}: {archive_name}", i18n::tr("Invalid archive name")).into(),
                    );
                }
                return;
            }
        };

        let Some(w) = h.upgrade() else { return };
        let cm = cm.clone();
        let editor = editor.clone();
        let theme_state = theme_state.clone();
        jobs::run(
            &w,
            move || {
                let manifest = import_theme_zip(&target_zip, &cm)?;
                Ok((
                    manifest.name,
                    cm.get_theme(),
                    cm.get_layout(),
                    scan_icon_themes(),
                    scan_cursor_themes(),
                ))
            },
            move |w, (name, theme, layout, icons, cursors)| {
                theme_state.load(w, &theme);
                editor.load(w, &layout);
                w.set_selected_icon_index(
                    icons
                        .iter()
                        .position(|v| v == &theme.icon_theme)
                        .unwrap_or(0) as i32,
                );
                w.set_selected_cursor_index(
                    cursors
                        .iter()
                        .position(|v| v == &theme.cursor_theme)
                        .unwrap_or(0) as i32,
                );
                w.set_icon_themes(ModelRc::from(Rc::new(VecModel::from(
                    icons
                        .into_iter()
                        .map(SharedString::from)
                        .collect::<Vec<_>>(),
                ))));
                w.set_cursor_themes(ModelRc::from(Rc::new(VecModel::from(
                    cursors
                        .into_iter()
                        .map(SharedString::from)
                        .collect::<Vec<_>>(),
                ))));
                send_ipc_fire_and_forget(IpcRequest::SetTheme(theme));
                send_ipc_fire_and_forget(IpcRequest::SetLayout(layout));
                w.set_status_message(format!("{}: {name}", i18n::tr("Theme imported")).into());
            },
        );
    });

    let cm = config_manager.clone();
    let h = window.as_weak();
    let editor = panel_editor.clone();
    window.on_import_plasma(move || {
        let Some(w) = h.upgrade() else { return };
        let cm = cm.clone();
        let editor = editor.clone();
        let theme_state = theme_editor.clone();
        jobs::run(
            &w,
            move || import_plasma_into(&cm),
            move |w, snapshot| {
                editor.load(w, &snapshot.layout);
                theme_state.load(w, &snapshot.theme);
                tokio::spawn(async move {
                    if let Err(err) = send_plasma_snapshot_ipc(&snapshot).await {
                        tracing::warn!("Plasma import live update failed: {err}");
                    }
                });
                w.set_status_message(i18n::tr("Appearance imported from KDE Plasma"));
            },
        );
    });

    let h = window.as_weak();
    window.on_search_changed(move |query| {
        if let Some(w) = h.upgrade() {
            let icon_theme = w.get_navigation_icon_theme().to_string();
            refresh_navigation_filtered(&w, &icon_theme, &query);
        }
    });

    let h = window.as_weak();
    window.on_search_accepted(move |query| {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return;
        }
        if let Some(w) = h.upgrade() {
            if let Some(item) = NAV_ITEMS.iter().find(|item| {
                let name_tr = i18n::tr(item.name).to_lowercase();
                let desc_tr = i18n::tr(item.desc).to_lowercase();
                let sec_tr = i18n::tr(item.section).to_lowercase();
                name_tr.contains(&q)
                    || item.name.to_lowercase().contains(&q)
                    || desc_tr.contains(&q)
                    || item.desc.to_lowercase().contains(&q)
                    || sec_tr.contains(&q)
                    || item.section.to_lowercase().contains(&q)
            }) {
                w.set_active_tab(item.tab_id);
                w.set_active_tab_title(i18n::tr(item.name));
                w.set_active_tab_description(i18n::tr(item.desc));
                w.set_active_tab_section(i18n::tr(item.section));
            }
        }
    });

    window.run()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_theme_and_mode_combinations_are_valid() {
        let themes = ["fairywren", "crisp", "cyber"];
        let modes = ["light", "dark", "oled"];

        for theme_id in themes {
            for mode_id in modes {
                let theme = get_theme_for(theme_id, mode_id);
                theme
                    .validate()
                    .unwrap_or_else(|e| panic!("Theme {} mode {} failed validation: {e:?}", theme_id, mode_id));

                match mode_id {
                    "light" => {
                        assert!(
                            !theme.appearance.prefers_dark(),
                            "Expected light mode for {theme_id} to prefer light"
                        );
                    }
                    "oled" => {
                        assert_eq!(
                            theme.appearance.palette.get("background").map(|s| s.as_str()),
                            Some("#000000"),
                            "Expected OLED mode for {theme_id} to have #000000 background"
                        );
                        assert_eq!(theme.opacity, 0.98);
                    }
                    "dark" => {
                        assert!(
                            theme.appearance.prefers_dark(),
                            "Expected dark mode for {theme_id} to prefer dark"
                        );
                        assert_ne!(
                            theme.appearance.palette.get("background").map(|s| s.as_str()),
                            Some("#000000"),
                            "Expected dark mode for {theme_id} to not be pure black"
                        );
                    }
                    _ => unreachable!(),
                }
            }
        }
    }
}
