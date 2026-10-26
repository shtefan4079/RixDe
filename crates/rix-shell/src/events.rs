use std::rc::Rc;
use std::sync::Arc;

use rix_config::ConfigManager;
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use tracing::{info, warn};

use crate::apps::{set_active_icon_theme, AppInfo, ApplicationCatalog};
use crate::pins::load_pinned_execs;
use crate::{
    apply_layout_to_ui, apply_shell_theme, build_taskbar_items, menu_categories, set_taskbar,
    update_menu_results, RixPanelWindow, TaskbarItemData,
};

pub fn fire_and_forget_ipc(req: rix_ipc::IpcRequest) {
    tokio::spawn(async move {
        match rix_ipc::IpcClient::connect().await {
            Ok(mut c) => {
                if let Err(e) = c.send(req).await {
                    warn!("IPC fire-and-forget failed: {}", e);
                }
            }
            Err(e) => warn!("IPC connect failed: {}", e),
        }
    });
}

async fn sync_window_snapshot(
    h: &slint::Weak<RixPanelWindow>,
    apps: &Arc<Vec<AppInfo>>,
    grouped: bool,
) {
    let result = match rix_ipc::IpcClient::connect().await {
        Ok(mut client) => client.send(rix_ipc::IpcRequest::ListWindows).await,
        Err(err) => Err(err),
    };
    let windows = match result {
        Ok(rix_ipc::IpcResponse::Windows(windows)) => windows,
        Ok(other) => {
            warn!("Unexpected ListWindows response: {:?}", other);
            return;
        }
        Err(err) => {
            warn!("Could not synchronize taskbar windows: {}", err);
            return;
        }
    };
    info!(
        "Synchronizing {} compositor windows with taskbar",
        windows.len()
    );
    let h = h.clone();
    let apps = apps.clone();
    crate::launch::observe_windows(&windows, &apps);
    if let Err(err) = slint::invoke_from_event_loop(move || {
        let Some(w) = h.upgrade() else { return };
        crate::tasks::snapshot(&w, windows, &apps, grouped);
    }) {
        warn!("Could not update taskbar on Slint event loop: {}", err);
    }
}

fn refresh_app_menu(
    h: &slint::Weak<RixPanelWindow>,
    apps: Arc<Vec<AppInfo>>,
    config: Arc<ConfigManager>,
) {
    let h = h.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = h.upgrade() {
            refresh_app_menu_contents(
                &window,
                &apps,
                &config.get_layout(),
                config.get_desktop().clipboard.enabled,
            );
        }
    });
}

thread_local! {
    static PENDING_APP_MENU: std::cell::RefCell<
        Option<(Vec<AppInfo>, rix_config::LayoutConfig, bool)>,
    > = const { std::cell::RefCell::new(None) };
}

fn refresh_app_menu_contents(
    window: &RixPanelWindow,
    apps: &[AppInfo],
    layout: &rix_config::LayoutConfig,
    clipboard_enabled: bool,
) {
    if !window.get_is_app_menu_open() {
        PENDING_APP_MENU.with(|pending| {
            *pending.borrow_mut() = Some((apps.to_vec(), layout.clone(), clipboard_enabled));
        });
        return;
    }
    apply_app_menu_contents(window, apps, layout, clipboard_enabled);
}

fn apply_app_menu_contents(
    window: &RixPanelWindow,
    apps: &[AppInfo],
    layout: &rix_config::LayoutConfig,
    clipboard_enabled: bool,
) {
    apply_layout_to_ui(window, layout, apps, clipboard_enabled);
    window.set_categories(ModelRc::from(Rc::new(VecModel::from(menu_categories(
        window, apps,
    )))));
    update_menu_results(
        window,
        apps,
        window.get_selected_category().as_str(),
        window.get_app_search_query().as_str(),
    );
}

pub(crate) fn flush_pending_app_menu(window: &RixPanelWindow) {
    let pending = PENDING_APP_MENU.with(|pending| pending.borrow_mut().take());
    if let Some((apps, layout, clipboard_enabled)) = pending {
        apply_app_menu_contents(window, &apps, &layout, clipboard_enabled);
    }
}

pub async fn handle_ipc_event(
    h_ipc: &slint::Weak<RixPanelWindow>,
    event: rix_ipc::IpcEvent,
    apps: &Arc<Vec<AppInfo>>,
    config: &Arc<ConfigManager>,
) -> bool {
    let event = match event {
        rix_ipc::IpcEvent::Shell { target, event } => {
            if !crate::binding::matches(&target) {
                if matches!(
                    event,
                    rix_ipc::ShellEvent::LauncherToggle
                        | rix_ipc::ShellEvent::LauncherSet(true)
                        | rix_ipc::ShellEvent::DesktopContextMenu { .. }
                ) {
                    let _ = h_ipc
                        .upgrade_in_event_loop(|window| crate::popup::dismiss_transient(&window));
                }
                return true;
            }
            match event {
                rix_ipc::ShellEvent::LauncherToggle => rix_ipc::IpcEvent::LauncherToggleRequested,
                rix_ipc::ShellEvent::LauncherSet(open) => rix_ipc::IpcEvent::LauncherToggled(open),
                rix_ipc::ShellEvent::DesktopContextMenu { x, y } => {
                    rix_ipc::IpcEvent::DesktopContextMenu { x, y }
                }
                rix_ipc::ShellEvent::SwitcherChanged(state) => {
                    rix_ipc::IpcEvent::SwitcherChanged(state)
                }
                rix_ipc::ShellEvent::PowerAction(action) => {
                    let h = h_ipc.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = h.upgrade() {
                            w.invoke_power_action(action.into());
                        }
                    });
                    return true;
                }
            }
        }
        event => event,
    };
    if matches!(event, rix_ipc::IpcEvent::WindowsChanged) {
        sync_window_snapshot(h_ipc, apps, config.get_desktop().group_tasks).await;
        return true;
    }
    match event {
        rix_ipc::IpcEvent::Shell { .. } => unreachable!("Shell event was resolved above"),
        rix_ipc::IpcEvent::DesktopChanged(desktop) => {
            let apps = apps.clone();
            let language = desktop.language.clone();
            let clipboard_enabled = desktop.clipboard.enabled;
            let layout = config.get_layout();
            let _ = h_ipc.upgrade_in_event_loop(move |w| {
                if let Err(error) = crate::i18n::select_language(&language) {
                    tracing::warn!(%error, "Could not apply the updated interface language");
                }
                crate::tasks::regroup(&w, &apps, desktop.group_tasks);
                refresh_app_menu_contents(&w, &apps, &layout, clipboard_enabled);
                w.global::<crate::SystemState>().invoke_calendar_today();
            });
        }
        rix_ipc::IpcEvent::ClipboardChanged => crate::clipboard::refresh(h_ipc.clone()),
        rix_ipc::IpcEvent::CaptureChanged(active) => {
            let h = h_ipc.clone();
            let _ = h.upgrade_in_event_loop(move |w| crate::capture::apply(&w, active));
        }
        rix_ipc::IpcEvent::KeyboardChanged(state) => {
            let h = h_ipc.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = h.upgrade() {
                    crate::system_bridge::apply_keyboard(&window, state);
                }
            });
        }
        rix_ipc::IpcEvent::EditModeChanged(is_edit) => {
            let h = h_ipc.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = h.upgrade() {
                    if is_edit {
                        crate::popup::dismiss_all(&w);
                    }
                    w.set_is_edit_mode(is_edit);
                }
            });
        }
        rix_ipc::IpcEvent::LayoutChanged(layout) => {
            let h = h_ipc.clone();
            let apps_for_ui = apps.clone();
            let pinned = load_pinned_execs(config);
            let clipboard_enabled = config.get_desktop().clipboard.enabled;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = h.upgrade() {
                    apply_layout_to_ui(&window, &layout, &apps_for_ui, clipboard_enabled);
                    let previous: Vec<TaskbarItemData> =
                        window.get_taskbar_items().iter().collect();
                    let current_pins: Vec<String> = previous
                        .iter()
                        .filter(|item| item.is_pinned)
                        .map(|item| item.app_id.to_string())
                        .collect();
                    if current_pins != pinned {
                        let mut items = build_taskbar_items(&apps_for_ui, &pinned);
                        for item in &mut items {
                            if let Some(old) = previous
                                .iter()
                                .find(|old| old.is_pinned && old.app_id == item.app_id)
                            {
                                item.id = old.id;
                                item.is_active = old.is_active;
                                item.is_running = old.is_running;
                                item.is_minimized = old.is_minimized;
                                item.is_maximized = old.is_maximized;
                            }
                        }
                        items.extend(previous.into_iter().filter(|item| !item.is_pinned));
                        set_taskbar(&window, items);
                    }
                }
            });
            sync_window_snapshot(h_ipc, apps, config.get_desktop().group_tasks).await;
        }
        rix_ipc::IpcEvent::SwitcherChanged(state) => {
            if state
                .target
                .as_ref()
                .is_some_and(|t| !crate::binding::matches(t))
            {
                return true;
            }
            let h = h_ipc.clone();
            let cfg = config.clone();
            let apps = apps.clone();
            let _ = h.upgrade_in_event_loop(move |window| {
                crate::switcher::snapshot(&window, state, &cfg, &apps)
            });
        }
        rix_ipc::IpcEvent::DesktopContextMenu { x, y } => {
            let h = h_ipc.clone();
            let _ = h.upgrade_in_event_loop(move |window| {
                crate::popup::open_desktop(&window, x as f32, y as f32)
            });
        }
        rix_ipc::IpcEvent::ShellPopupsDismissed => {
            let h = h_ipc.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = h.upgrade() {
                    w.invoke_cancel_taskbar_drag();
                    crate::popup::dismiss_transient(&w);
                }
            });
        }
        rix_ipc::IpcEvent::LauncherToggleRequested => {
            let h = h_ipc.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = h.upgrade() {
                    crate::launcher::set_open(&w, !w.get_is_app_menu_open());
                }
            });
        }
        rix_ipc::IpcEvent::LauncherToggled(open) => {
            let h = h_ipc.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = h.upgrade() {
                    crate::launcher::set_open(&w, open);
                }
            });
        }
        rix_ipc::IpcEvent::WindowOpened(_)
        | rix_ipc::IpcEvent::WindowClosed(_)
        | rix_ipc::IpcEvent::WindowFocused(_)
        | rix_ipc::IpcEvent::ShowDesktopChanged(_)
        | rix_ipc::IpcEvent::WindowsChanged => {}
        rix_ipc::IpcEvent::IconThemeChanged(theme_name) => {
            set_active_icon_theme(&theme_name);
            info!("Icon theme changed via IPC: '{}'", theme_name);
            refresh_app_menu(h_ipc, apps.clone(), config.clone());
            sync_window_snapshot(h_ipc, apps, config.get_desktop().group_tasks).await;
        }
        rix_ipc::IpcEvent::ThemeChanged(theme) => {
            set_active_icon_theme(&theme.icon_theme);
            let h = h_ipc.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = h.upgrade() {
                    apply_shell_theme(&w, &theme);
                }
            });
            refresh_app_menu(h_ipc, apps.clone(), config.clone());
            sync_window_snapshot(h_ipc, apps, config.get_desktop().group_tasks).await;
        }

        rix_ipc::IpcEvent::DisplayChanged(_) | rix_ipc::IpcEvent::PowerChanged(_) => {}
    }
    true
}

pub fn spawn_ipc_subscription(
    window_handle: slint::Weak<RixPanelWindow>,
    apps: impl Into<ApplicationCatalog>,
    config: Arc<ConfigManager>,
) {
    let catalog = apps.into();
    tokio::spawn(async move {
        let mut backoff = std::time::Duration::from_secs(1);
        loop {
            match rix_ipc::IpcClient::subscribe().await {
                Ok(mut sub) => {
                    let apps = catalog.snapshot();
                    info!("rix-shell subscribed to IPC events");
                    backoff = std::time::Duration::from_secs(1);
                    crate::clipboard::refresh(window_handle.clone());
                    sync_window_snapshot(&window_handle, &apps, config.get_desktop().group_tasks)
                        .await;
                    if let Ok(mut client) = rix_ipc::IpcClient::connect().await {
                        if let Ok(rix_ipc::IpcResponse::CaptureStatus(active)) =
                            client.send(rix_ipc::IpcRequest::GetCaptureStatus).await
                        {
                            handle_ipc_event(
                                &window_handle,
                                rix_ipc::IpcEvent::CaptureChanged(active),
                                &apps,
                                &config,
                            )
                            .await;
                        }
                        if let Ok(rix_ipc::IpcResponse::Switcher(state)) =
                            client.send(rix_ipc::IpcRequest::GetSwitcher).await
                        {
                            handle_ipc_event(
                                &window_handle,
                                rix_ipc::IpcEvent::SwitcherChanged(state),
                                &apps,
                                &config,
                            )
                            .await;
                        }
                    }
                    loop {
                        match sub.next_event().await {
                            Ok(event) => {
                                let apps = catalog.snapshot();
                                if !handle_ipc_event(&window_handle, event, &apps, &config).await {
                                    break;
                                }
                            }
                            Err(e) => {
                                warn!("IPC subscription lost ({}), reconnecting...", e);
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("IPC subscribe failed ({}), retry in {:?}", e, backoff);
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
        }
    });
}

pub fn spawn_catalog_subscription(
    window: slint::Weak<RixPanelWindow>,
    catalog: ApplicationCatalog,
    config: Arc<ConfigManager>,
) {
    tokio::spawn(async move {
        let mut updates = catalog.subscribe();
        loop {
            let apps = updates.borrow_and_update().clone();
            let layout = config.get_layout();
            let pinned = load_pinned_execs(&config);
            let grouped = config.get_desktop().group_tasks;
            let clipboard = config.get_desktop().clipboard.enabled;
            let result = window.upgrade_in_event_loop(move |window| {
                crate::apps::invalidate_application_icons();
                refresh_app_menu_contents(&window, &apps, &layout, clipboard);
                let previous = crate::read_taskbar(&window);
                let mut pins = build_taskbar_items(&apps, &pinned);
                for pin in &mut pins {
                    if let Some(old) = previous
                        .iter()
                        .find(|item| item.is_pinned && item.app_id == pin.app_id)
                    {
                        pin.id = old.id;
                    }
                }
                pins.extend(previous.into_iter().filter(|item| !item.is_pinned));
                set_taskbar(&window, pins);
                crate::tasks::regroup(&window, &apps, grouped);
            });
            if result.is_err() || updates.changed().await.is_err() {
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str) -> AppInfo {
        AppInfo {
            desktop_file: format!("/unused/{id}.desktop").into(),
            name: id.into(),
            desktop_id: id.into(),
            exec: String::new(),
            icon_name: String::new(),
            icon_path: None,
            categories: vec!["Utility".into()],
            startup_wm_class: String::new(),
        }
    }

    #[test]
    fn closed_launcher_defers_catalogue_refreshes_until_opened() {
        i_slint_backend_testing::init_no_event_loop();
        let window = RixPanelWindow::new().unwrap();
        window.set_selected_category("all".into());
        let apps = vec![app("com.example.First"), app("com.example.Second")];
        let layout = rix_config::LayoutConfig::default_layout();
        refresh_app_menu_contents(&window, &apps, &layout, false);
        assert_eq!(
            window.get_apps().row_count(),
            0,
            "a closed menu must not rebuild its results"
        );
        assert_eq!(window.get_categories().row_count(), 0);
        crate::launcher::set_open(&window, true);
        assert_eq!(window.get_apps().row_count(), 2);
        assert!(window.get_categories().row_count() > 0);
    }
}
