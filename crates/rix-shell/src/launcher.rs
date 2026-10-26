use crate::{
    apps::{load_icon_at_size, load_slint_image},
    LauncherActionData, PlaceData, RixPanelWindow,
};
use rix_config::{ConfigManager, LauncherConfig};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::rc::Rc;
use std::sync::Arc;

pub fn set_open(window: &RixPanelWindow, open: bool) {
    if open {
        window.set_system_popup("".into());
        window.set_menu_focus_generation(window.get_menu_focus_generation().wrapping_add(1));
        window.invoke_tray_menu_closed();
        window.set_is_context_menu_open(false);
    }
    window.set_is_app_menu_open(open);
    if open {

        crate::events::flush_pending_app_menu(window);
    }
}

pub fn dismiss_transient(window: &RixPanelWindow) {
    if !window.get_launcher_pinned() {
        set_open(window, false);
    }
}

pub fn apply(window: &RixPanelWindow, config: &LauncherConfig) {
    let size = window
        .global::<crate::Theme>()
        .get_icon_size()
        .round()
        .max(1.) as u32;
    let mut groups: [Vec<LauncherActionData>; 2] = Default::default();
    for item in config.footer.iter().filter(|item| item.visible) {
        let icon = load_icon_at_size(&item.icon, size);
        groups[usize::from(item.end)].push(LauncherActionData {
            action: item.action.as_str().into(),
            label: item.label.as_str().into(),
            has_icon: icon.is_some(),
            icon: icon.unwrap_or_default(),
            icon_only: item.icon_only,
        });
    }
    let [start, end] = groups;
    window.set_launcher_start_actions(ModelRc::from(Rc::new(VecModel::from(start))));
    window.set_launcher_end_actions(ModelRc::from(Rc::new(VecModel::from(end))));
    load_places(window);
}
fn load_places(window: &RixPanelWindow) {
    let places = rix_services::places::list()
        .into_iter()
        .map(|place| {
            let (label, icon) = match place.key.as_str() {
                "HOME" => ("Home folder", "user-home"),
                "DESKTOP" => ("Desktop", "user-desktop"),
                "DOCUMENTS" => ("Documents", "folder-documents"),
                "DOWNLOAD" => ("Downloads", "folder-downloads"),
                "MUSIC" => ("Music", "folder-music"),
                "PICTURES" => ("Pictures", "folder-pictures"),
                "VIDEOS" => ("Videos", "folder-videos"),
                _ => (place.key.as_str(), "folder"),
            };
            let icon = load_slint_image(icon);
            PlaceData {
                name: crate::i18n::tr(label),
                path: place.path.to_string_lossy().as_ref().into(),
                has_icon: icon.is_some(),
                icon: icon.unwrap_or_default(),
            }
        })
        .collect::<Vec<_>>();
    window.set_places(ModelRc::from(Rc::new(VecModel::from(places))));
}
fn setup_security_actions(window: &RixPanelWindow) {
    let actions = [
        ("lock", "Lock", "system-lock-screen", false),
        ("logout", "Log out…", "system-log-out", false),
        ("taskmanager", "Task Manager", "utilities-system-monitor", false),
        ("reboot", "Restart…", "system-reboot", true),
        ("poweroff", "Shut down…", "system-shutdown", true),
    ]
    .into_iter()
    .map(|(id, label, icon_name, danger)| {
        let icon = load_slint_image(icon_name);
        crate::SecurityActionItem {
            id: id.into(),
            label: crate::i18n::tr(label),
            icon: icon.clone().unwrap_or_default(),
            has_icon: icon.is_some(),
            danger,
        }
    })
    .collect::<Vec<_>>();
    window.set_security_actions(ModelRc::from(Rc::new(VecModel::from(actions))));
}

pub fn install(window: &RixPanelWindow, config: Arc<ConfigManager>) {
    let weak = window.as_weak();
    let locale_config = config.clone();
    window.on_open_place(move |path| {
        let Some(window) = weak.upgrade() else { return };
        let mut command = std::process::Command::new("gio");
        command.arg("open").arg(path.as_str());
        rix_services::locale::apply_to_command(&mut command, &locale_config.get_desktop().language);
        match crate::launch::spawn_process(command) {
            Ok(_) => dismiss_transient(&window),
            Err(error) => window.set_launcher_message(
                format!("{}: {error}", crate::i18n::tr("Could not open folder")).into(),
            ),
        }
    });
    let weak = window.as_weak();
    window.on_session_dialog_confirmed({
        let weak = weak.clone();
        move |action| {
            if let Some(window) = weak.upgrade() {
                window.set_session_dialog_visible(false);
                execute_power_action(&window, action.as_str());
            }
        }
    });
    window.on_session_dialog_cancelled({
        let weak = weak.clone();
        move || {
            if let Some(window) = weak.upgrade() {
                window.set_session_dialog_visible(false);
            }
        }
    });

    let weak = window.as_weak();
    window.on_security_dismissed({
        let weak = weak.clone();
        move || {
            if let Some(window) = weak.upgrade() {
                window.set_security_screen_visible(false);
            }
        }
    });

    let weak = window.as_weak();
    window.on_security_action({
        let weak = weak.clone();
        move |action| {
            let Some(window) = weak.upgrade() else { return };
            window.set_security_screen_visible(false);
            match action.as_str() {
                "cancel" => {}
                "lock" => {
                    execute_power_action(&window, "lock");
                }
                "logout" => {
                    window.invoke_power_action("logout".into());
                }
                "taskmanager" => {
                    let command = std::process::Command::new("missioncenter");
                    if crate::launch::spawn_process(command).is_err() {
                        let _ = crate::launch::spawn_process(std::process::Command::new("rix-settings"));
                    }
                }
                "reboot" => {
                    window.invoke_power_action("reboot".into());
                }
                "poweroff" => {
                    window.invoke_power_action("poweroff".into());
                }
                _ => {
                    execute_power_action(&window, action.as_str());
                }
            }
        }
    });

    let weak = window.as_weak();
    window.on_power_action(move |action| {
        if action == "security" {
            if let Some(window) = weak.upgrade() {
                crate::popup::dismiss_all(&window);
                setup_security_actions(&window);
                window.set_security_screen_visible(true);
            }
            return;
        }
        if action == "leave" {
            if let Some(window) = weak.upgrade() {
                window.set_desktop_context(false);
                crate::popup::set_items(
                    &window,
                    [
                        ("lock", "Lock", "system-lock-screen"),
                        ("logout", "Log out…", "system-log-out"),
                        ("suspend", "Sleep", "system-suspend"),
                        ("hibernate", "Hibernate", "system-suspend-hibernate"),
                        ("reboot", "Restart…", "system-reboot"),
                        ("poweroff", "Shut down…", "system-shutdown"),
                    ]
                    .into_iter()
                    .filter(|(id, _, _)| {
                        !window
                            .get_launcher_start_actions()
                            .iter()
                            .chain(window.get_launcher_end_actions().iter())
                            .any(|visible| visible.action == *id)
                    })
                    .map(|(id, label, icon)| {
                        let image = load_slint_image(icon);
                        crate::ContextMenuItemData {
                            id: format!("session:{id}").into(),
                            label: crate::i18n::tr(label),
                            icon: image.clone().unwrap_or_default(),
                            has_icon: image.is_some(),
                            ..Default::default()
                        }
                    })
                    .collect(),
                );
                let alignment = window.get_menu_alignment();
                let menu_width = window.get_menu_width() as f32;
                let left = (window.get_launcher_x() + window.get_launcher_width() * alignment
                    - menu_width * alignment)
                    .clamp(0., (window.get_panel_width() as f32 - menu_width).max(0.));
                let theme = window.global::<crate::Theme>();
                let context_width = if window.get_context_menu_width() > 0. {
                    window.get_context_menu_width()
                } else {
                    window.global::<crate::MenuStyle>().get_minimum_width()
                }
                .min((window.get_panel_width() as f32 - theme.get_spacing_xs() * 2.).max(0.));
                window.set_context_menu_x(left + menu_width - context_width);
                let panel = (window.get_thickness() + window.get_effective_panel_margin()) as f32;
                let footer = window.global::<crate::Theme>().get_menu_footer_height();
                window.set_context_menu_bottom(panel + footer);
                window.set_context_menu_top(
                    panel
                        + (window.get_menu_height() as f32)
                            .min(window.get_available_height() as f32 - panel)
                        - footer,
                );
                window.set_context_is_tray(false);
                window.set_context_menu_target_id(0);
                window.set_context_menu_target_exec("".into());
                window.set_context_target_windows(ModelRc::default());
                window.set_context_can_go_back(false);
                window.set_context_prefer_below(window.get_panel_at_top());
                window.set_is_context_menu_open(true);
            }
            return;
        }
        let Some(window) = weak.upgrade() else { return };
        match action.as_str() {
            "reboot" | "poweroff" | "logout" => {
                crate::popup::dismiss_all(&window);
                let (title, message, confirm, is_danger, icon_name) = match action.as_str() {
                    "reboot" => (
                        crate::i18n::tr("Restart"),
                        crate::i18n::tr("Are you sure you want to restart the computer?"),
                        crate::i18n::tr("Restart"),
                        true,
                        "system-reboot",
                    ),
                    "poweroff" => (
                        crate::i18n::tr("Shut down"),
                        crate::i18n::tr("Are you sure you want to shut down the computer?"),
                        crate::i18n::tr("Shut down"),
                        true,
                        "system-shutdown",
                    ),
                    _ => (
                        crate::i18n::tr("Log out"),
                        crate::i18n::tr("Are you sure you want to log out of the current session?"),
                        crate::i18n::tr("Log out"),
                        false,
                        "system-log-out",
                    ),
                };
                let icon = load_slint_image(icon_name);
                window.set_session_dialog_has_icon(icon.is_some());
                if let Some(img) = icon {
                    window.set_session_dialog_icon(img);
                }
                window.set_session_dialog_action(action.into());
                window.set_session_dialog_title(title);
                window.set_session_dialog_message(message);
                window.set_session_dialog_confirm_label(confirm);
                window.set_session_dialog_is_danger(is_danger);
                window.set_session_dialog_visible(true);
            }
            _ => {
                execute_power_action(&window, action.as_str());
            }
        }
    });
}

fn execute_power_action(window: &RixPanelWindow, action: &str) {
    let weak = window.as_weak();
    let action = action.to_string();
    tokio::spawn(async move {
        let result = async {
            if matches!(action.as_str(), "lock" | "suspend" | "hibernate") {
                let mut client = rix_ipc::IpcClient::connect().await?;
                match client.send(rix_ipc::IpcRequest::LockSession).await? {
                    rix_ipc::IpcResponse::Ok => {}
                    rix_ipc::IpcResponse::Error(message) => {
                        anyhow::bail!(crate::i18n::tr(&message))
                    }
                    _ => anyhow::bail!(crate::i18n::tr(
                        "The lock screen did not confirm readiness"
                    )),
                }
                if action == "lock" {
                    return Ok(());
                }
            }
            rix_services::session::request(action.as_str()).await
        }
        .await;
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = weak.upgrade() {
                match result {
                    Ok(()) => window.set_is_app_menu_open(false),
                    Err(error) => {
                        window.set_launcher_message(crate::i18n::tr(&error.to_string()))
                    }
                }
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn power_actions_open_native_session_dialog() {
        let window = RixPanelWindow::new().unwrap();
        let config = Arc::new(ConfigManager::init().unwrap());
        install(&window, config);
        assert!(!window.get_session_dialog_visible());

        window.invoke_power_action("reboot".into());
        assert!(window.get_session_dialog_visible());
        assert_eq!(window.get_session_dialog_action(), "reboot");
        assert!(window.get_session_dialog_is_danger());

        window.invoke_session_dialog_cancelled();
        assert!(!window.get_session_dialog_visible());

        window.invoke_power_action("poweroff".into());
        assert!(window.get_session_dialog_visible());
        assert_eq!(window.get_session_dialog_action(), "poweroff");
        assert!(window.get_session_dialog_is_danger());

        window.invoke_power_action("logout".into());
        assert!(window.get_session_dialog_visible());
        assert_eq!(window.get_session_dialog_action(), "logout");
        assert!(!window.get_session_dialog_is_danger());

        window.invoke_session_dialog_confirmed("logout".into());
        assert!(!window.get_session_dialog_visible());

        window.invoke_power_action("security".into());
        assert!(window.get_security_screen_visible());
        window.invoke_security_dismissed();
        assert!(!window.get_security_screen_visible());
    }
}
