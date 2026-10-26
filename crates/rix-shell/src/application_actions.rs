use crate::*;
pub fn install(
    window: &RixPanelWindow,
    config_mgr: Arc<ConfigManager>,
    all_apps: impl Into<apps::ApplicationCatalog>,
) {
    let all_apps = all_apps.into();
    let window_handle = window.as_weak();

    {
        let h = window_handle.clone();
        let apps = all_apps.clone();
        let cfg = config_mgr.clone();
        window.on_taskbar_context_menu(move |id, x, y| {
            let apps = apps.snapshot();
            if let Some(w) = h.upgrade() {
                let items = read_taskbar(&w);
                if let Some(item) = items.iter().find(|i| i.id == id) {
                    let mut menu = Vec::new();
                    let can_launch = find_app(&apps, item.app_id.as_str()).is_some();
                    if item.is_pinned || can_launch {
                        menu.push(ContextMenuItemData {
                            id: if item.is_pinned {
                                "unpin".into()
                            } else {
                                "pin".into()
                            },
                            label: if item.is_pinned {
                                i18n::tr("Unpin from panel")
                            } else {
                                i18n::tr("Pin to panel")
                            },
                            is_destructive: false,
                            ..Default::default()
                        });
                    }
                    if let Some(app) = find_app(&apps, item.app_id.as_str()) {
                        let favorite = cfg.get_layout().launcher.favorites.iter().any(|id| {
                            find_app(&apps, id)
                                .is_some_and(|entry| entry.desktop_id == app.desktop_id)
                        });
                        menu.push(ContextMenuItemData {
                            id: if favorite { "unfavorite" } else { "favorite" }.into(),
                            label: i18n::tr(if favorite {
                                "Remove from launcher favorites"
                            } else {
                                "Add to launcher favorites"
                            }),
                            ..Default::default()
                        });
                    }
                    if let Some(app) = find_app(&apps, item.app_id.as_str()) {
                        append_launch_actions(&mut menu, app);
                    }
                    if item.is_running {
                        if !menu.is_empty() {
                            menu.push(ContextMenuItemData {
                                separator: true,
                                ..Default::default()
                            });
                        }
                        menu.push(ContextMenuItemData {
                            id: if item.windows.row_count() > 1 {
                                "close_all"
                            } else {
                                "close"
                            }
                            .into(),
                            label: i18n::tr(if item.windows.row_count() > 1 {
                                "Close all windows"
                            } else {
                                "Close"
                            }),
                            is_destructive: true,
                            ..Default::default()
                        });
                    }
                    w.set_context_target_windows(ModelRc::from(Rc::new(VecModel::from(
                        item.windows
                            .iter()
                            .map(|member| member.id)
                            .collect::<Vec<_>>(),
                    ))));
                    w.set_preview_visible(false);
                    w.set_system_popup("".into());
                    w.set_desktop_context(false);
                    w.invoke_tray_menu_closed();
                    w.set_context_is_tray(false);
                    w.set_context_can_go_back(false);
                    w.set_context_prefer_below(w.get_panel_at_top());
                    popup::set_items(&w, menu);
                    w.set_context_menu_target_id(id);
                    w.set_context_menu_target_exec(item.app_id.clone());
                    w.set_context_menu_x(x);
                    w.set_context_menu_top(y);
                    w.set_context_menu_bottom(
                        w.window().size().height as f32 / w.window().scale_factor() - y,
                    );
                    w.set_is_app_menu_open(false);
                    w.set_is_context_menu_open(true);
                }
            }
        });
    }
    {
        let h = window_handle.clone();
        let apps = all_apps.clone();
        let cfg = config_mgr.clone();
        window.on_app_context_menu(move |exec, x, y| {
            let apps = apps.snapshot();
            if let Some(w) = h.upgrade() {
                let desktop_id = find_app(&apps, exec.as_str()).map(|app| app.desktop_id.as_str());
                let pinned = read_taskbar(&w).iter().any(|i| {
                    i.is_pinned && (i.app_id == exec || desktop_id.is_some_and(|id| i.app_id == id))
                });
                let favorite =
                    desktop_id.is_some_and(|id| {
                        cfg.get_layout().launcher.favorites.iter().any(|entry| {
                            find_app(&apps, entry).is_some_and(|app| app.desktop_id == id)
                        })
                    });
                let mut menu = Vec::new();
                if let Some(app) = find_app(&apps, exec.as_str()) {
                    append_launch_actions(&mut menu, app);
                    menu.push(ContextMenuItemData {
                        separator: true,
                        ..Default::default()
                    });
                }
                menu.push(ContextMenuItemData {
                    id: if pinned { "unpin_app" } else { "pin_app" }.into(),
                    label: i18n::tr(if pinned {
                        "Unpin from panel"
                    } else {
                        "Pin to panel"
                    }),
                    ..Default::default()
                });
                w.set_context_target_windows(ModelRc::default());
                w.set_context_menu_target_id(0);
                w.set_desktop_context(false);
                menu.push(ContextMenuItemData {
                    id: if favorite { "unfavorite" } else { "favorite" }.into(),
                    label: i18n::tr(if favorite {
                        "Remove from favorites"
                    } else {
                        "Add to launcher favorites"
                    }),
                    ..Default::default()
                });
                w.invoke_tray_menu_closed();
                w.set_context_is_tray(false);
                w.set_context_can_go_back(false);
                w.set_context_prefer_below(true);
                popup::set_items(&w, menu);
                w.set_context_menu_target_exec(desktop_id.unwrap_or(exec.as_str()).into());
                w.set_context_menu_x(x);
                w.set_context_menu_top(y);
                w.set_context_menu_bottom(
                    w.window().size().height as f32 / w.window().scale_factor() - y,
                );
                w.set_is_context_menu_open(true);
            }
        });
    }
    {
        let h = window_handle.clone();
        let cfg = config_mgr.clone();
        let apps = all_apps.clone();
        window.on_context_menu_selected(move |action, target_id, target_exec| {
            let catalog = apps.clone();
            let apps = catalog.snapshot();
            if let Some(w) = h.upgrade() {
                popup::close(&w);
                let mut items = read_taskbar(&w);
                let target_id = items
                    .iter()
                    .find(|item| {
                        item.id == target_id
                            || item.windows.iter().any(|member| {
                                w.get_context_target_windows()
                                    .iter()
                                    .any(|id| member.id == id)
                            })
                    })
                    .map(|item| item.id)
                    .unwrap_or(target_id);
                if let Some(action) = action.strip_prefix("session:") {
                    w.invoke_power_action(action.into());
                    popup::close(&w);
                    return;
                }
                if let Some(id) = action.strip_prefix("desktop-action:") {
                    if let Some(app) = find_app(&apps, target_exec.as_str()) {
                        launch::spawn_desktop_action(app, &cfg.get_desktop().language, id);
                    }
                    return;
                }
                match action.as_str() {
                    "panel-settings" => {
                        panel_editor::set_open(&w, true);
                        return;
                    }
                    "wallpaper-settings" => {
                        w.global::<crate::SystemState>()
                            .invoke_open_settings("wallpaper".into());
                        return;
                    }
                    "shell-settings" => {
                        w.invoke_tray_clicked();
                        return;
                    }
                    "panel-edit" => {
                        w.invoke_toggle_edit_mode();
                        return;
                    }
                    "pin" => {
                        if let Some(item) = items.iter_mut().find(|i| i.id == target_id) {
                            if let Some(app) = find_app(&apps, item.app_id.as_str()) {
                                item.app_id = app.desktop_id.clone().into();
                                item.is_pinned = true;
                            }
                        }
                    }
                    "unpin" | "unpin_app" => {
                        let pos = if action.as_str() == "unpin" {
                            items.iter().position(|i| i.id == target_id)
                        } else {
                            let desktop_id = find_app(&apps, target_exec.as_str())
                                .map(|app| app.desktop_id.as_str());
                            items.iter().position(|i| {
                                i.app_id == target_exec
                                    || desktop_id.is_some_and(|id| i.app_id == id)
                            })
                        };
                        if let Some(pos) = pos {
                            if items[pos].is_running {
                                items[pos].is_pinned = false;
                            } else {
                                items.remove(pos);
                            }
                        }
                    }
                    "pin_app" => {
                        let exec_str = target_exec.as_str();
                        if let Some(app) = find_app(&apps, exec_str) {
                            if let Some(item) = items.iter_mut().find(|item| {
                                find_app(&apps, item.app_id.as_str())
                                    .is_some_and(|candidate| candidate.desktop_id == app.desktop_id)
                            }) {
                                item.is_pinned = true;
                                item.app_id = app.desktop_id.clone().into();
                            } else {
                                items.extend(build_taskbar_items(
                                    &apps,
                                    std::slice::from_ref(&app.desktop_id),
                                ));
                            }
                        }
                    }
                    "favorite" | "unfavorite" => {
                        if let Some(app) = find_app(&apps, target_exec.as_str()) {
                            let desktop_id = app.desktop_id.clone();
                            let favorite = action == "favorite";
                            let weak = w.as_weak();
                            let catalog = catalog.clone();
                            let clipboard_enabled = cfg.get_desktop().clipboard.enabled;
                            tokio::spawn(async move {
                                let result = async {
                                    let mut client = rix_ipc::IpcClient::connect().await?;
                                    let rix_ipc::IpcResponse::Layout(base) =
                                        client.send(rix_ipc::IpcRequest::GetLayout).await?
                                    else {
                                        anyhow::bail!("Unexpected layout response");
                                    };
                                    let mut updated = base.clone();
                                    let apps = catalog.snapshot();
                                    updated.launcher.favorites.retain(|id| {
                                        find_app(&apps, id)
                                            .is_none_or(|app| app.desktop_id != desktop_id)
                                    });
                                    if favorite {
                                        updated.launcher.favorites.push(desktop_id);
                                    }
                                    let rix_ipc::IpcResponse::Layout(layout) = client
                                        .send(rix_ipc::IpcRequest::EditLayout { base, updated })
                                        .await?
                                    else {
                                        anyhow::bail!("Unexpected config write response");
                                    };
                                    Ok::<_, anyhow::Error>(layout)
                                }
                                .await;
                                let _ = weak.upgrade_in_event_loop(move |w| match result {
                                    Ok(layout) => {
                                        let apps = catalog.snapshot();
                                        apply_layout_to_ui(&w, &layout, &apps, clipboard_enabled);
                                        update_menu_results(
                                            &w,
                                            &apps,
                                            w.get_selected_category().as_str(),
                                            w.get_app_search_query().as_str(),
                                        );
                                        w.set_launcher_message("".into());
                                    }
                                    Err(error) => w.set_launcher_message(
                                        format!(
                                            "{}: {error}",
                                            i18n::tr("Could not save favorites")
                                        )
                                        .into(),
                                    ),
                                });
                            });
                        }
                        return;
                    }
                    "launch" => {
                        if let Some(app) = find_app(&apps, target_exec.as_str()) {
                            launch::open_app(app, &apps, &cfg.get_desktop().language);
                        }
                        return;
                    }
                    "new_window" => {
                        if let Some(app) = find_app(&apps, target_exec.as_str()) {
                            spawn_app(app, &cfg.get_desktop().language);
                        }
                        return;
                    }
                    "close" | "close_all" => {
                        let ids: Vec<_> = w.get_context_target_windows().iter().collect();
                        for id in
                            ids.into_iter()
                                .take(if action == "close_all" { usize::MAX } else { 1 })
                        {
                            fire_and_forget_ipc(rix_ipc::IpcRequest::CloseWindow(id as u64));
                        }
                        return;
                    }
                    _ => return,
                }
                if let Err(error) = save_pinned_execs(&cfg, &collect_pinned_execs(&items)) {
                    w.set_launcher_message(
                        format!(
                            "{}: {error}",
                            i18n::tr("Could not save pinned applications")
                        )
                        .into(),
                    );
                    tracing::error!(%error, "Could not save pinned applications");
                    return;
                }
                set_taskbar(&w, items);
            }
        });
    }
    {
        let h = window_handle.clone();
        window.on_close_context_menu(move || {
            if let Some(w) = h.upgrade() {
                popup::close(&w);
            }
        });
    }

    {
        let h = window_handle.clone();
        let apps = all_apps.clone();
        let cfg = config_mgr.clone();
        window.on_taskbar_item_clicked(move |id| {
            let apps = apps.snapshot();
            if let Some(w) = h.upgrade() {
                let items = read_taskbar(&w);
                if let Some(item) = items
                    .iter()
                    .find(|i| i.id == id && i.windows.row_count() > 1)
                {
                    crate::tasks::open_group(&w, item);
                    return;
                }

                w.invoke_preview_dismiss();
                let target = items.iter().find(|i| i.id == id).map(|i| {
                    (
                        i.id,
                        i.title.to_string(),
                        i.app_id.to_string(),
                        i.is_active,
                        i.is_running,
                        i.is_pinned,
                    )
                });
                if let Some((tid, title, app_id, was_active, is_running, is_pinned)) = target {
                    if is_running && was_active {
                        fire_and_forget_ipc(rix_ipc::IpcRequest::MinimizeWindow(tid as u64));
                    } else if is_running {
                        fire_and_forget_ipc(rix_ipc::IpcRequest::FocusWindow(tid as u64));
                    } else if is_pinned {
                        info!("Launching pinned app: {}", title);
                        if let Some(app) = find_app(&apps, &app_id) {
                            spawn_app(app, &cfg.get_desktop().language);
                        }
                    }
                }
            }
        });
    }
}

fn append_launch_actions(menu: &mut Vec<ContextMenuItemData>, app: &AppInfo) {
    menu.push(ContextMenuItemData {
        id: "launch".into(),
        label: i18n::tr("Open"),
        ..Default::default()
    });
    menu.extend(
        rix_services::applications::desktop_actions(&app.desktop_file)
            .into_iter()
            .map(|action| ContextMenuItemData {
                id: format!("desktop-action:{}", action.id).into(),
                label: action.name.into(),
                ..Default::default()
            }),
    );
}
