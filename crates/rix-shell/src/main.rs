slint::include_modules!();

mod appearance;
mod application_actions;
mod binding;
mod capture;
mod clipboard;
mod task_audio;
mod task_order;
mod tasks;
type SystemHost = RixPanelWindow;
pub mod apps;
#[path = "../../../ui/common/i18n.rs"]
mod i18n;
mod launcher;
mod layout;
mod menu_model;
mod panel_editor;
mod popup;
mod preview;
mod profiling;
#[path = "../../../ui/common/skins.rs"]
mod skins;
mod switcher;
#[path = "../../../ui/common/system_bridge.rs"]
mod system_bridge;
mod tray;
mod tray_menu;
#[cfg(test)]
mod ui_tests;
use appearance::apply_shell_theme;
pub mod events;
mod heartbeat;
pub mod launch;
pub mod pins;

#[cfg(test)]
use apps::scan_applications;
use apps::{
    convert_apps_to_menu_data, load_slint_image, set_active_icon_theme, AppInfo, ApplicationCatalog,
};
use events::{fire_and_forget_ipc, spawn_ipc_subscription};
use launch::spawn_app;
use pins::{collect_pinned_execs, load_pinned_execs, save_pinned_execs};

use chrono::Local;
use rix_config::{ConfigManager, LayoutConfig};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info};

thread_local! {
    static SHELL_SURFACE: std::cell::RefCell<Option<rix_shell_adapter::SurfaceController>> = const { std::cell::RefCell::new(None) };
}

fn install_surface_geometry(
    window: &RixPanelWindow,
    surface: &rix_shell_adapter::SurfaceController,
) {
    window
        .global::<MenuStyle>()
        .set_native_popups(surface.supports_native_popups());
    let radius = window.as_weak();
    surface.set_popup_radius_provider(move || {
        radius
            .upgrade()
            .map(|w| w.global::<Theme>().get_context_menu_radius())
            .unwrap_or(4.)
    });
    let regions = window.as_weak();
    surface.set_region_provider(move || {
        regions
            .upgrade()
            .map(|w| {
                w.get_surface_regions()
                    .iter()
                    .map(|r| rix_shell_adapter::SurfaceRect {
                        x: r.x,
                        y: r.y,
                        width: r.width,
                        height: r.height,
                        radius: r.radius,
                        opacity: r.opacity,
                        clip: (r.clip_width > 0. && r.clip_height > 0.).then_some(
                            rix_shell_adapter::SurfaceClip {
                                x: r.clip_x,
                                y: r.clip_y,
                                width: r.clip_width,
                                height: r.clip_height,
                            },
                        ),
                    })
                    .collect()
            })
            .unwrap_or_default()
    });
}

static NEXT_PIN_ID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

fn next_pin_id() -> i32 {
    NEXT_PIN_ID.fetch_sub(1, std::sync::atomic::Ordering::Relaxed)
}

use apps::find_app;

pub(crate) fn build_taskbar_items(
    all_apps: &[AppInfo],
    pinned_execs: &[String],
) -> Vec<TaskbarItemData> {
    let mut items = Vec::new();
    for exec in pinned_execs {
        if let Some(app) = find_app(all_apps, exec) {
            let icon = load_slint_image(&app.icon_name);
            let has_icon = icon.is_some();
            let badge = app.name.chars().next().unwrap_or(' ').to_string();
            items.push(TaskbarItemData {
                id: next_pin_id(),
                title: app.name.clone().into(),
                app_id: app.desktop_id.clone().into(),
                badge: badge.into(),
                icon: icon.unwrap_or_default(),
                has_icon,
                is_active: false,
                is_running: false,
                is_minimized: false,
                is_maximized: false,
                is_pinned: true,
                ..Default::default()
            });
        }
    }
    items
}

pub(crate) fn apply_layout_to_ui(
    window: &RixPanelWindow,
    layout: &LayoutConfig,
    apps: &[AppInfo],
    clipboard_enabled: bool,
) {
    let modules = tray::system_modules(
        &layout.services.tray_modules,
        clipboard_enabled,
        window.get_capture_active(),
    );
    window.set_system_modules(ModelRc::from(Rc::new(VecModel::from(
        modules.into_iter().map(Into::into).collect::<Vec<_>>(),
    ))));

    window.set_previews_enabled(layout.services.previews);
    window
        .global::<SystemState>()
        .set_maximum_volume(layout.services.maximum_volume as i32);
    window
        .global::<SystemState>()
        .set_week_start(layout.services.week_start as i32);
    if let Some(panel) = binding::panel(layout) {
        panel_editor::apply(window, panel);
        window.set_thickness(panel.thickness as i32);
        window.set_floating(panel.floating);
        window.set_corner_radius(panel.corner_radius as i32);
        window.set_use_theme_frame(panel.use_theme_frame);
        window.set_floating_margin(panel.floating_margin as i32);
        SHELL_SURFACE.with(|surface| {
            if let Some(surface) = surface.borrow().as_ref() {
                surface.set_spec(rix_shell_adapter::LayerSurfaceSpec {
                    output: binding::output(),
                    anchors: rix_shell_adapter::ShellAnchors::from_position(panel.position),
                    exclusive_zone: window.get_panel_reserved_extent(),
                    keyboard_interactive: window.get_taskbar_drag_active()
                        || window.get_is_app_menu_open()
                        || window.get_is_context_menu_open()
                        || window.get_is_edit_mode()
                        || !window.get_system_popup().is_empty(),
                    ..Default::default()
                });
            }
        });
        let show_labels = panel
            .widgets
            .iter()
            .find(|widget| widget.kind.as_str() == "taskbar")
            .and_then(|widget| widget.properties.get("show-labels"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        window.set_taskbar_show_labels(show_labels);
        layout::apply(window, panel);
        set_taskbar(window, window.get_taskbar_items().iter().collect());
    }

    launcher::apply(window, &layout.launcher);
    window.set_menu_width(layout.launcher.width as i32);
    window.set_menu_alignment(match layout.launcher.alignment {
        rix_core::Alignment::Start => 0.,
        rix_core::Alignment::End => 1.,
        _ => 0.5,
    });
    window.set_menu_height(layout.launcher.height as i32);
    let icon = apps::load_icon_at_size(
        &layout.launcher.icon,
        (window.global::<Theme>().get_panel_launcher_icon_size() * window.window().scale_factor())
            .round() as u32,
    );
    window.set_has_launcher_icon(icon.is_some());
    window.set_launcher_icon(icon.unwrap_or_default());

    let favorite_apps: Vec<AppInfo> = layout
        .launcher
        .favorites
        .iter()
        .filter_map(|id| find_app(apps, id).cloned())
        .collect();
    window.set_favorite_apps(ModelRc::from(Rc::new(VecModel::from(
        convert_apps_to_menu_data(
            &favorite_apps,
            (window.global::<Theme>().get_favorite_icon_size() * window.window().scale_factor())
                .round() as u32,
        ),
    ))));
}

pub(crate) fn menu_categories(window: &RixPanelWindow, apps: &[AppInfo]) -> Vec<MenuCategoryData> {
    let size =
        (window.global::<Theme>().get_icon_size() * window.window().scale_factor()).round() as u32;
    [
        ("Development", "applications-development"),
        ("Game", "applications-games"),
        ("Graphics", "applications-graphics"),
        ("Network", "applications-internet"),
        ("AudioVideo", "applications-multimedia"),
        ("Office", "applications-office"),
        ("Settings", "applications-system"),
        ("System", "applications-system"),
        ("Utility", "applications-utilities"),
        ("Wine", "wine"),
    ]
    .into_iter()
    .filter(|(key, _)| {
        apps.iter()
            .any(|app| app.categories.iter().any(|c| c == key))
    })
    .map(|(key, icon_name)| {
        let icon = apps::load_icon_at_size(icon_name, size);
        MenuCategoryData {
            key: key.into(),
            name: i18n::tr(match key {
                "Development" => "Development",
                "Game" => "Games",
                "Graphics" => "Graphics",
                "Network" => "Internet",
                "AudioVideo" => "Multimedia",
                "Office" => "Office",
                "Settings" => "Settings",
                "System" => "System",
                "Utility" => "Utilities",
                _ => "Wine",
            }),
            has_icon: icon.is_some(),
            icon: icon.unwrap_or_default(),
        }
    })
    .collect()
}

pub(crate) fn update_menu_results(
    window: &RixPanelWindow,
    apps: &[AppInfo],
    category: &str,
    query: &str,
) {

    let request =
        menu_model::MenuRequest::new(apps.to_vec(), category.to_string(), query.to_string());
    if !menu_model::enqueue(request) {
        apply_menu_results(window, apps, category, query);
    }
}

pub(crate) fn apply_menu_results(
    window: &RixPanelWindow,
    apps: &[AppInfo],
    category: &str,
    query: &str,
) {
    let query = query.trim().to_lowercase();
    let selected: Vec<_> = apps
        .iter()
        .filter(|app| {
            if !query.is_empty() {
                app.name.to_lowercase().contains(&query)
                    || app.desktop_id.to_lowercase().contains(&query)
                    || app.exec.to_lowercase().contains(&query)
            } else {
                category == "all" || app.categories.iter().any(|entry| entry == category)
            }
        })
        .cloned()
        .collect();
    menu_model::set_results(window, selected);
}

pub(crate) fn set_taskbar(window: &RixPanelWindow, items: Vec<TaskbarItemData>) {
    let items = task_order::apply(window, items);
    let width = if items.is_empty() {
        0
    } else {
        items
            .iter()
            .map(|item| {
                if window.get_taskbar_show_labels() && item.is_active {
                    window.global::<Theme>().get_task_label_width().round() as i32
                } else {
                    window.global::<Theme>().get_task_width().round() as i32
                }
            })
            .sum::<i32>()
            + (items.len().saturating_sub(1) as i32
                * window.global::<Theme>().get_task_spacing().round() as i32)
    };
    window.set_taskbar_width(width);
    let current = window.get_taskbar_items();
    if let Some(model) = current.as_any().downcast_ref::<VecModel<TaskbarItemData>>() {

        while model.row_count() > items.len() {
            model.remove(model.row_count() - 1);
        }
        for (index, item) in items.into_iter().enumerate() {
            if index < model.row_count() {
                model.set_row_data(index, item);
            } else {
                model.push(item);
            }
        }
    } else {
        window.set_taskbar_items(ModelRc::from(Rc::new(VecModel::from(items))));
    }
    layout::reflow(window);
}

fn read_taskbar(window: &RixPanelWindow) -> Vec<TaskbarItemData> {
    window.get_taskbar_items().iter().collect()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    #[derive(clap::Parser)]
    struct Args {
        #[arg(long, hide = true, conflicts_with_all = ["panel", "output"])]
        launch_desktop: Option<std::path::PathBuf>,
        #[arg(long, hide = true, requires = "launch_desktop")]
        desktop_action: Option<String>,
        #[arg(long)]
        panel: Option<String>,
        #[arg(long)]
        output: Option<String>,
    }
    let args = <Args as clap::Parser>::parse();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    if let Some(path) = args.launch_desktop {
        rix_services::resources::prepare_application(false)?;
        rix_services::resources::application_scope(false).await;
        if let Some(action) = args.desktop_action {
            return rix_services::applications::launch_desktop_action(&path, &action);
        }
        return launch::launch_desktop(&path);
    }
    info!("Starting Rix Shell...");

    let config_mgr = Arc::new(ConfigManager::init()?);
    let layout = config_mgr.get_layout();
    let panel_id = args
        .panel
        .or_else(|| layout.panels.first().map(|p| p.id.clone()))
        .ok_or_else(|| anyhow::anyhow!("No panel configured"))?;
    anyhow::ensure!(
        layout.panels.iter().any(|p| p.id == panel_id),
        "Configured panel does not exist"
    );
    binding::initialize(panel_id, args.output.unwrap_or_default());
    set_active_icon_theme(&config_mgr.get_theme().icon_theme);

    let surface = rix_shell_adapter::install(rix_shell_adapter::LayerSurfaceSpec {
        output: binding::output(),
        ..Default::default()
    })?;
    SHELL_SURFACE.with(|s| *s.borrow_mut() = Some(surface.clone()));
    let window = RixPanelWindow::new()?;
    if let Err(error) = i18n::select_language(&config_mgr.get_desktop().language) {
        tracing::warn!(%error, "Could not select the configured interface language");
    }
    install_surface_geometry(&window, &surface);
    popup::install(&window);
    tray::install(&window);
    launcher::install(&window, config_mgr.clone());
    let weak = window.as_weak();
    surface.on_output_size_changed(move |width, height| {
        if let Some(window) = weak.upgrade() {

            window.set_panel_width(width as i32);
            window.set_available_height(height as i32);
        }
    });
    let weak = window.as_weak();
    surface.on_focus_changed(move |focused| {
        if !focused {
            if let Some(window) = weak.upgrade() {
                window.invoke_cancel_taskbar_drag();
                popup::dismiss_transient(&window);
            }
        }
    });
    let fullscreen_surface = surface.clone();
    window.on_surface_output_fullscreen_changed(move |fullscreen| {
        fullscreen_surface.set_output_fullscreen(fullscreen)
    });
    let overlay_surface = surface.clone();
    window.on_surface_overlay_changed(move |active| overlay_surface.set_overlay_active(active));
    let reserved_surface = surface.clone();
    window.on_surface_exclusive_zone_changed(move |extent| {
        reserved_surface.set_exclusive_zone(extent)
    });
    window
        .on_surface_state_changed(move |interactive| surface.set_keyboard_interactive(interactive));
    let weak = window.as_weak();
    window.on_reflow_panel(move || {
        if let Some(window) = weak.upgrade() {
            layout::reflow(&window);
        }
    });
    let window_handle = window.as_weak();
    apply_shell_theme(&window, &config_mgr.get_theme());

    let all_apps = ApplicationCatalog::watch();
    menu_model::install(&window);

    task_order::install(&window, config_mgr.clone());

    set_taskbar(
        &window,
        build_taskbar_items(&all_apps.snapshot(), &load_pinned_execs(&config_mgr)),
    );

    let clock_timer = slint::Timer::default();
    window.set_categories(ModelRc::from(Rc::new(VecModel::from(menu_categories(
        &window,
        &all_apps.snapshot(),
    )))));
    window.set_user_name(
        std::env::var("USER")
            .unwrap_or_else(|_| "User".into())
            .into(),
    );
    window.set_user_initial(
        std::env::var("USER")
            .ok()
            .and_then(|name| name.chars().next())
            .unwrap_or('U')
            .to_uppercase()
            .to_string()
            .into(),
    );

    {
        let clock_config = config_mgr.clone();
        let tick = move |w: &RixPanelWindow| {
            let appearance = clock_config.get_theme().appearance;
            let now = Local::now();
            w.set_time_str(now.format(&appearance.time_format).to_string().into());
            w.set_date_str(now.format(&appearance.date_format).to_string().into());
        };
        if let Some(w) = window_handle.upgrade() {
            tick(&w);
        }
        let clock_handle = window.as_weak();
        clock_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(1),
            move || {
                if let Some(w) = clock_handle.upgrade() {
                    tick(&w);
                }
            },
        );
    }

    apply_layout_to_ui(
        &window,
        &config_mgr.get_layout(),
        &all_apps.snapshot(),
        config_mgr.get_desktop().clipboard.enabled,
    );
    window.set_panel_width(config_mgr.get_display().width as i32);

    spawn_ipc_subscription(window_handle.clone(), all_apps.clone(), config_mgr.clone());
    events::spawn_catalog_subscription(window_handle.clone(), all_apps.clone(), config_mgr.clone());

    panel_editor::install(&window, config_mgr.clone(), all_apps.clone());
    system_bridge::install(&window, config_mgr.clone());
    let weak = window.as_weak();
    window
        .global::<SystemState>()
        .on_network_show_prompt(move || {
            let Some(window) = weak.upgrade() else { return };
            if window.get_system_popup() != "network" {
                popup::dismiss_all(&window);
                window.set_is_edit_mode(false);
                let anchor = if let Some(tray) = window.get_widgets().iter().find(|w| w.kind == "tray") {
                    let modules = window.get_system_modules();
                    let items_count = window.get_tray_items().row_count();
                    let total = items_count + modules.row_count();
                    let cell_width = if total > 0 { tray.width / total as f32 } else { tray.width };
                    let module_idx = modules.iter().position(|m| m == "network").unwrap_or(0);
                    window.get_panel_offset() + tray.x + (items_count + module_idx) as f32 * cell_width + cell_width / 2.0
                } else {
                    let width = window.window().size().width as f32 / window.window().scale_factor();
                    width / 2.0
                };
                window.set_system_anchor(anchor);
                window.set_system_popup("network".into());
            }
        });
    task_audio::install(&window, all_apps.clone());
    clipboard::install(&window);
    let _preview_timer = preview::install(&window);
    let _switcher_timer = switcher::install(&window);
    window
        .on_preview_activate(|id| fire_and_forget_ipc(rix_ipc::IpcRequest::FocusWindow(id as u64)));
    window.on_preview_close_window(|id| {
        fire_and_forget_ipc(rix_ipc::IpcRequest::CloseWindow(id as u64))
    });
    let weak = window.as_weak();
    window.on_open_system_popup(move |kind, mut x| {
        if let Some(window) = weak.upgrade() {
            let close = window.get_system_popup() == kind;
            popup::dismiss_all(&window);
            window.set_is_edit_mode(false);
            if !close {
                if x <= 0.0 {
                    if kind == "calendar" {
                        if let Some(clock) = window.get_widgets().iter().find(|w| w.kind == "clock") {
                            x = window.get_panel_offset() + clock.x + clock.width / 2.0;
                        }
                    } else if let Some(tray) = window.get_widgets().iter().find(|w| w.kind == "tray") {
                        let modules = window.get_system_modules();
                        let items_count = window.get_tray_items().row_count();
                        let total = items_count + modules.row_count();
                        let cell_width = if total > 0 { tray.width / total as f32 } else { tray.width };
                        if let Some(module_idx) = modules.iter().position(|m| m == kind.as_str()) {
                            x = window.get_panel_offset() + tray.x + (items_count + module_idx) as f32 * cell_width + cell_width / 2.0;
                        }
                    }
                }
                window.set_system_anchor(x);
                window.set_system_popup(kind);
            }
        }
    });

    {
        let h = window_handle.clone();
        window.on_toggle_app_menu(move || {
            if let Some(w) = h.upgrade() {
                launcher::set_open(&w, !w.get_is_app_menu_open());
            }
        });
    }
    {
        let h = window_handle.clone();
        window.on_open_app_menu(move || {
            if let Some(w) = h.upgrade() {
                launcher::set_open(&w, true);
            }
        });
    }
    {
        let h = window_handle.clone();
        window.on_close_app_menu(move || {
            if let Some(w) = h.upgrade() {
                launcher::set_open(&w, false);
            }
        });
    }
    {
        let h = window_handle.clone();
        let apps = all_apps.clone();
        window.on_app_menu_closed(move || {
            let apps = apps.snapshot();
            if let Some(w) = h.upgrade() {
                w.set_app_search_query("".into());
                update_menu_results(&w, &apps, w.get_selected_category().as_str(), "");
            }
        });
    }
    {

        let h = window_handle.clone();
        let apps = all_apps.clone();
        window.on_search_query_changed(move |query| {
            let apps = apps.snapshot();
            if let Some(w) = h.upgrade() {
                update_menu_results(
                    &w,
                    &apps,
                    w.get_selected_category().as_str(),
                    query.as_str(),
                );
            }
        });
    }
    {
        let h = window_handle.clone();
        let apps = all_apps.clone();
        window.on_category_selected(move |category| {
            let apps = apps.snapshot();
            if let Some(w) = h.upgrade() {
                w.set_selected_category(category.clone());
                w.set_app_search_query("".into());
                update_menu_results(&w, &apps, category.as_str(), "");
            }
        });
    }
    {
        let h = window_handle.clone();
        let apps = all_apps.clone();
        let config = config_mgr.clone();
        window.on_launch_app(move |exec| {
            let apps = apps.snapshot();
            if let Some(w) = h.upgrade() {
                launcher::dismiss_transient(&w);
            }
            if let Some(app) = find_app(&apps, exec.as_str()) {
                launch::launch_from_menu(app, &apps, &config.get_desktop().language);
            } else {
                tracing::warn!(desktop_id = %exec, "Application launch target is absent from the catalog");
            }
        });
    }

    capture::install(&window);
    application_actions::install(&window, config_mgr.clone(), all_apps.clone());

    let open_settings = |page: Option<&str>| {
        let binary = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(|dir| dir.join("rix-settings")))
            .filter(|path| path.is_file())
            .unwrap_or_else(|| "rix-settings".into());
        let mut command = std::process::Command::new(&binary);
        if let Some(page) = page {
            command.arg(format!("--page={page}"));
        }
        if let Err(err) = launch::spawn_process(command) {
            error!("Failed to open RixDE settings from {:?}: {}", binary, err);
        }
    };
    window
        .global::<SystemState>()
        .on_open_settings(move |page| open_settings(Some(page.as_str())));
    window
        .global::<SystemState>()
        .on_keyboard_settings(move || open_settings(Some("keyboard")));
    window.on_tray_clicked(move || open_settings(None));
    window.on_launcher_settings(move || open_settings(Some("panel")));

    window.on_show_desktop_clicked(|| {
        tokio::spawn(async {
            let Ok(mut client) = rix_ipc::IpcClient::connect().await else {
                return;
            };
            if let Err(error) = client.send(rix_ipc::IpcRequest::ShowDesktop).await {
                tracing::warn!(%error, "Could not toggle the desktop");
            }
        });
    });

    let _frame_profile = profiling::start();
    let _shell_heartbeat = heartbeat::start();
    window.run()?;
    Ok(())
}
