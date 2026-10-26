use crate::{PluginChoice, PluginSettings, SettingsWindow};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::{rc::Rc, sync::Arc};
pub fn refresh(window: &SettingsWindow, config: &rix_config::ConfigManager) {
    refresh_enabled(window, &config.get_desktop().enabled_plugins);
}
fn refresh_enabled(window: &SettingsWindow, enabled: &[String]) {
    let rows = rix_services::plugins::discover()
        .into_iter()
        .map(|plugin| {
            let capabilities = plugin
                .manifest
                .capabilities
                .iter()
                .map(|c| {
                    match c {
                        rix_services::plugins::Capability::Wallpaper => {
                            crate::i18n::tr("Wallpaper")
                        }
                        rix_services::plugins::Capability::DesktopControl => {
                            crate::i18n::tr("Desktop control")
                        }
                        rix_services::plugins::Capability::ScreenCapture => {
                            crate::i18n::tr("Screen capture")
                        }
                        rix_services::plugins::Capability::RemoteInput => {
                            crate::i18n::tr("Remote input")
                        }
                    }
                    .to_string()
                })
                .collect::<Vec<_>>()
                .join(" · ");
            PluginChoice {
                enabled: enabled.contains(&plugin.manifest.id),
                available: plugin.unavailable.is_empty(),
                has_settings: !plugin.manifest.settings.is_empty(),
                id: plugin.manifest.id.into(),
                name: plugin.manifest.name.into(),
                description: plugin.manifest.description.into(),
                capabilities: capabilities.into(),
                status: if plugin.unavailable.is_empty() {
                    String::new()
                } else {
                    format!(
                        "{}: {}",
                        crate::i18n::tr("Install required components"),
                        plugin.unavailable.join(", ")
                    )
                }
                .into(),
            }
        })
        .collect::<Vec<_>>();
    window
        .global::<PluginSettings>()
        .set_entries(ModelRc::from(Rc::new(VecModel::from(rows))));
}
fn spawn(mut command: std::process::Command) -> anyhow::Result<()> {
    let mut child = command.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn localized_status(message: &str) -> String {
    if let Some(detail) = message.strip_prefix("Process exited: ") {
        format!("{} {detail}", crate::i18n::tr("Process exited:"))
    } else if let Some(detail) = message.strip_prefix("Missing: ") {
        format!("{} {detail}", crate::i18n::tr("Missing:"))
    } else {
        crate::i18n::tr(message).to_string()
    }
}

pub fn install(window: &SettingsWindow, config: Arc<rix_config::ConfigManager>) -> slint::Timer {
    refresh(window, &config);
    window
        .global::<PluginSettings>()
        .set_capture_fps(config.get_desktop().screen_cast_fps as i32);
    let weak = window.as_weak();
    let cfg = config.clone();
    window
        .global::<PluginSettings>()
        .on_apply_capture_rate(move || {
            if let Some(w) = weak.upgrade() {
                let base = cfg.get_desktop();
                let mut desktop = base.clone();
                desktop.screen_cast_fps = w.global::<PluginSettings>().get_capture_fps() as u32;
                crate::jobs::run(
                    &w,
                    move || crate::jobs::commit_desktop(base, desktop),
                    |w, _| {
                        w.set_status_message(crate::i18n::tr(
                            "Frame rate saved; the capture extension will restart",
                        ));
                    },
                );
            }
        });
    let weak = window.as_weak();
    let cfg = config.clone();
    window.global::<PluginSettings>().on_reload(move || {
        if let Some(w) = weak.upgrade() {
            refresh(&w, &cfg);
        }
    });
    let weak = window.as_weak();
    let cfg = config.clone();
    window
        .global::<PluginSettings>()
        .on_toggle(move |id, enabled| {
            if let Some(w) = weak.upgrade() {
                let base = cfg.get_desktop();
                let mut desktop = base.clone();
                desktop.enabled_plugins.retain(|item| item != id.as_str());
                if enabled {
                    desktop.enabled_plugins.push(id.into());
                }
                crate::jobs::run(
                    &w,
                    move || crate::jobs::commit_desktop(base, desktop),
                    |w, desktop| {
                        w.global::<PluginSettings>().set_error("".into());
                        w.set_status_message(crate::i18n::tr("Extensions saved"));
                        refresh_enabled(w, &desktop.enabled_plugins);
                    },
                );
            }
        });
    let weak = window.as_weak();
    window.global::<PluginSettings>().on_settings(move |id| {
        if let Some(w) = weak.upgrade() {
            if let Some(plugin) = rix_services::plugins::discover()
                .iter()
                .find(|p| p.manifest.id == id.as_str())
            {
                if let Err(e) = plugin.command(true).and_then(spawn) {
                    w.global::<PluginSettings>().set_error(e.to_string().into());
                }
            }
        }
    });
    let weak = window.as_weak();
    window.global::<PluginSettings>().on_open_folder(move || {
        if let Some(w) = weak.upgrade() {
            let result = (|| -> anyhow::Result<()> {
                let dir = rix_services::plugins::directories().remove(0);
                std::fs::create_dir_all(&dir)?;
                let mut command = std::process::Command::new("gio");
                command.arg("open").arg(dir);
                spawn(command)
            })();
            if let Err(e) = result {
                w.global::<PluginSettings>().set_error(e.to_string().into());
            }
        }
    });
    let weak = window.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_secs(2),
        move || {
            let Some(w) = weak.upgrade() else { return };
            if w.get_active_tab() != 12 {
                return;
            }
            let weak = weak.clone();
            tokio::spawn(async move {
                let result = async {
                    let mut client = rix_ipc::IpcClient::connect().await?;
                    client.send(rix_ipc::IpcRequest::GetPlugins).await
                }
                .await;
                let _ = weak.upgrade_in_event_loop(move |w| {
                    if let Ok(rix_ipc::IpcResponse::Plugins(states)) = result {
                        let rows = w.global::<PluginSettings>().get_entries();
                        for (index, mut row) in rows.iter().enumerate() {
                            if !row.available {
                                continue;
                            }
                            row.status = states
                                .iter()
                                .find(|s| s.id == row.id.as_str())
                                .map(|s| {
                                    if s.running {
                                        crate::i18n::tr("Running").to_string()
                                    } else {
                                        localized_status(&s.message)
                                    }
                                })
                                .unwrap_or_else(|| {
                                    if row.enabled {
                                        crate::i18n::tr("Waiting to start").to_string()
                                    } else {
                                        crate::i18n::tr("Disabled").to_string()
                                    }
                                })
                                .into();
                            if rows.row_data(index).as_ref() != Some(&row) {
                                rows.set_row_data(index, row);
                            }
                        }
                    }
                });
            });
        },
    );
    timer
}
