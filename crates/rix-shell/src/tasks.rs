use crate::{AppInfo, RixPanelWindow, TaskWindowData, TaskbarItemData};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::{cell::RefCell, rc::Rc};
thread_local! { static WINDOWS: RefCell<Vec<rix_ipc::WindowInfo>> = const { RefCell::new(Vec::new()) }; }
fn identity(apps: &[AppInfo], id: &str) -> String {
    crate::find_app(apps, id)
        .map(|a| a.desktop_id.clone())
        .unwrap_or_else(|| id.trim_end_matches(".desktop").to_ascii_lowercase())
}

pub fn existing_window(app: &AppInfo, apps: &[AppInfo]) -> Option<u64> {
    WINDOWS.with(|cache| {
        preferred_window(
            &cache.borrow(),
            app,
            apps,
            crate::binding::output().as_deref(),
        )
    })
}

fn preferred_window(
    windows: &[rix_ipc::WindowInfo],
    app: &AppInfo,
    apps: &[AppInfo],
    output: Option<&str>,
) -> Option<u64> {
    windows
        .iter()
        .filter(|window| identity(apps, &window.app_id).eq_ignore_ascii_case(&app.desktop_id))
        .max_by_key(|window| {
            (
                window.is_active,
                !window.is_minimized,
                output.is_none_or(|output| window.outputs.iter().any(|name| name == output)),
                window.id,
            )
        })
        .map(|window| window.id)
}
pub fn snapshot(
    window: &RixPanelWindow,
    windows: Vec<rix_ipc::WindowInfo>,
    apps: &[AppInfo],
    grouped: bool,
) {
    window.set_fullscreen_window(
        windows
            .iter()
            .any(|w| crate::binding::foreground_window_on_output(w) && w.is_fullscreen),
    );
    window.set_maximized_window(windows.iter().any(|w| {
        crate::binding::foreground_window_on_output(w) && (w.is_maximized || w.is_fullscreen)
    }));
    let has_visible_windows = windows.iter().any(|w| {
        !w.is_minimized
            && (crate::binding::foreground_window_on_output(w)
                || crate::binding::output().as_deref().is_none_or(|own| {
                    w.outputs.is_empty() || w.outputs.iter().any(|name| name == own)
                }))
    });
    window.set_is_on_desktop(!has_visible_windows);
    WINDOWS.with(|cache| *cache.borrow_mut() = windows.clone());
    let icon_size = crate::apps::task_icon_size(window);
    let mut items: Vec<_> = window
        .get_taskbar_items()
        .iter()
        .filter(|i| i.is_pinned)
        .collect();
    for item in &mut items {
        let app = crate::find_app(apps, item.app_id.as_str());
        item.title = app
            .map(|a| a.name.as_str())
            .unwrap_or(item.app_id.as_str())
            .into();
        let icon = app.and_then(|a| crate::apps::load_icon_at_size(&a.icon_name, icon_size));
        item.has_icon = icon.is_some();
        item.icon = icon.unwrap_or_default();
        item.is_running = false;
        item.is_active = false;
        item.is_minimized = false;
        item.is_maximized = false;

    }
    let mut grouped_members: Vec<Vec<TaskWindowData>> = vec![Vec::new(); items.len()];
    for win in windows {
        let key = identity(apps, &win.app_id);
        let index = items.iter().position(|i| {
            (!i.is_running || grouped)
                && !key.is_empty()
                && identity(apps, i.app_id.as_str()) == key
        });
        let member = TaskWindowData {
            id: win.id as i32,
            title: win.title.clone().into(),
            active: win.is_active,
        };
        if let Some(index) = index {
            {
                let item = &mut items[index];
                if !item.is_running || win.is_active {
                    item.id = win.id as i32;
                    item.title = win.title.into();
                    item.is_minimized = win.is_minimized;
                    item.is_maximized = win.is_maximized;
                }
                item.is_running = true;
                item.is_active |= win.is_active;
            }
            grouped_members[index].push(member);
        } else {
            let icon = crate::find_app(apps, &win.app_id)
                .and_then(|a| crate::apps::load_icon_at_size(&a.icon_name, icon_size))
                .or_else(|| crate::apps::load_icon_at_size(&win.app_id, icon_size));
            grouped_members.push(vec![member]);
            items.push(TaskbarItemData {
                id: win.id as i32,
                title: win.title.into(),
                app_id: key.into(),
                icon: icon.clone().unwrap_or_default(),
                has_icon: icon.is_some(),
                is_active: win.is_active,
                is_running: true,
                is_minimized: win.is_minimized,
                is_maximized: win.is_maximized,
                ..Default::default()
            });
        }
    }
    for (item, members) in items.iter_mut().zip(grouped_members) {
        set_windows(item, members);
    }
    for item in &mut items {
        if !item.is_running && item.id >= 0 {
            item.id = crate::next_pin_id();
        }
    }
    crate::set_taskbar(window, items);
    crate::task_audio::refresh(window, apps);
}

fn set_windows(item: &mut TaskbarItemData, members: Vec<TaskWindowData>) {
    if item.windows.row_count() == members.len()
        && item.windows.iter().zip(&members).all(|(old, new)| {
            old.id == new.id && old.active == new.active && old.title == new.title
        })
    {
        return;
    }
    if let Some(model) = item
        .windows
        .as_any()
        .downcast_ref::<VecModel<TaskWindowData>>()
    {
        while model.row_count() > members.len() {
            model.remove(model.row_count() - 1);
        }
        for (index, member) in members.into_iter().enumerate() {
            if index < model.row_count() {
                let old = model.row_data(index).unwrap();
                if old.id != member.id || old.active != member.active || old.title != member.title {
                    model.set_row_data(index, member);
                }
            } else {
                model.push(member);
            }
        }
    } else {
        item.windows = ModelRc::from(Rc::new(VecModel::from(members)));
    }
}
pub fn regroup(window: &RixPanelWindow, apps: &[AppInfo], grouped: bool) {
    let windows = WINDOWS.with(|cache| cache.borrow().clone());
    snapshot(window, windows, apps, grouped);
}
pub fn open_group(window: &RixPanelWindow, item: &TaskbarItemData) {
    if window.get_preview_visible()
        && window.get_preview_pinned()
        && item.windows.iter().any(|w| w.id == window.get_preview_id())
    {
        window.set_preview_visible(false);
        return;
    }
    crate::launcher::dismiss_transient(window);
    crate::popup::close(window);
    window.set_is_edit_mode(false);
    let mut x = window.get_panel_offset();
    if let Some(widget) = window.get_widgets().iter().find(|w| w.kind == "taskbar") {
        x += widget.x;
    }
    for task in window.get_taskbar_items().iter() {
        let width = if window.get_taskbar_show_labels() && task.is_active {
            window.global::<crate::Theme>().get_task_label_width()
        } else {
            window.global::<crate::Theme>().get_task_width()
        };
        if task.id == item.id {
            x += width / 2.;
            break;
        }
        x += width + window.global::<crate::Theme>().get_task_spacing();
    }
    window.set_system_popup("".into());
    window.invoke_preview_open_group(item.id, x);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> AppInfo {
        AppInfo {
            desktop_file: "/unavailable/fixture.desktop".into(),
            name: "Fixture chat".into(),
            desktop_id: "org.rixde.ChatFixture".into(),
            exec: "/unavailable/fixture-chat".into(),
            icon_name: String::new(),
            icon_path: None,
            categories: Vec::new(),
            startup_wm_class: "FixtureChat".into(),
        }
    }

    fn window(id: u64, app_id: &str, output: &str, minimized: bool) -> rix_ipc::WindowInfo {
        rix_ipc::WindowInfo {
            id,
            title: "Fixture chat".into(),
            app_id: app_id.into(),
            outputs: vec![output.into()],
            foreground_outputs: None,
            is_active: false,
            is_minimized: minimized,
            is_maximized: false,
            is_fullscreen: false,
        }
    }

    #[test]
    fn panel_returns_when_switching_out_of_fullscreen_without_minimizing_the_game() {
        i_slint_backend_testing::init_no_event_loop();
        let panel = RixPanelWindow::new().unwrap();
        panel.set_panel_width(1280);
        panel.set_available_height(720);
        panel.set_thickness(40);
        panel.set_floating_margin(12);
        let weak = panel.as_weak();
        panel.on_reflow_panel(move || {
            if let Some(panel) = weak.upgrade() {
                crate::layout::reflow(&panel);
            }
        });
        let fullscreen_changes = Rc::new(RefCell::new(Vec::new()));
        let changes = fullscreen_changes.clone();
        panel.on_surface_output_fullscreen_changed(move |fullscreen| {
            changes.borrow_mut().push(fullscreen);
        });
        let launcher_clicks = Rc::new(std::cell::Cell::new(0));
        let clicks = launcher_clicks.clone();
        panel.on_toggle_app_menu(move || clicks.set(clicks.get() + 1));
        panel.show().unwrap();
        crate::layout::apply(&panel, &rix_config::PanelConfig::default());
        for legacy in [false, true] {
            let mut game = window(1, "game", "DP-1", false);
            game.is_fullscreen = true;
            let mut editor = window(2, "editor", "DP-1", false);
            for _ in 0..3 {
                game.is_active = true;
                editor.is_active = false;
                game.foreground_outputs = (!legacy).then(|| vec!["DP-1".into()]);
                editor.foreground_outputs = (!legacy).then(Vec::new);
                snapshot(&panel, vec![game.clone(), editor.clone()], &[], true);
                slint::platform::update_timers_and_animations();
                assert!(panel.get_fullscreen_window());
                assert!(panel.get_maximized_window());
                assert_eq!(panel.get_surface_regions().row_data(0).unwrap().height, 0.);

                game.is_active = false;
                editor.is_active = true;
                game.foreground_outputs = (!legacy).then(Vec::new);
                editor.foreground_outputs = (!legacy).then(|| vec!["DP-1".into()]);
                snapshot(&panel, vec![game.clone(), editor.clone()], &[], true);
                slint::platform::update_timers_and_animations();
                assert!(!panel.get_fullscreen_window());
                assert!(!panel.get_maximized_window());
                assert_eq!(panel.get_panel_reserved_extent(), 52);
                let region = panel.get_surface_regions().row_data(0).unwrap();
                assert_eq!(region.height, 40.);
                assert_eq!(region.y, 680.);
                assert!(!panel.get_is_on_desktop());
                assert!(!panel.get_effective_panel_floating());

                let mut minimized_game = game.clone();
                let mut minimized_editor = editor.clone();
                minimized_game.is_minimized = true;
                minimized_editor.is_minimized = true;
                snapshot(&panel, vec![minimized_game, minimized_editor], &[], true);
                slint::platform::update_timers_and_animations();
                assert!(panel.get_is_on_desktop());
                assert!(panel.get_effective_panel_floating());
                assert_eq!(panel.get_panel_reserved_extent(), 52);

                snapshot(&panel, vec![game.clone(), editor.clone()], &[], true);
                slint::platform::update_timers_and_animations();
                let region = panel.get_surface_regions().row_data(0).unwrap();
                let launcher = panel
                    .get_widgets()
                    .iter()
                    .find(|widget| widget.kind == "launcher")
                    .unwrap();
                let position = slint::LogicalPosition::new(
                    panel.get_panel_offset() + launcher.x + launcher.width / 2.,
                    region.y + region.height / 2.,
                );
                let before_click = launcher_clicks.get();
                for event in [
                    slint::platform::WindowEvent::PointerMoved { position },
                    slint::platform::WindowEvent::PointerPressed {
                        position,
                        button: slint::platform::PointerEventButton::Left,
                    },
                    slint::platform::WindowEvent::PointerReleased {
                        position,
                        button: slint::platform::PointerEventButton::Left,
                    },
                ] {
                    panel.window().dispatch_event(event);
                }
                assert_eq!(launcher_clicks.get(), before_click + 1);
            }
        }
        assert_eq!(&*fullscreen_changes.borrow(), &[true, false].repeat(6));
    }

    #[test]
    fn launcher_selects_matching_windows_across_outputs_and_minimization() {
        let app = app();
        let apps = vec![app.clone()];
        let mut windows = vec![
            window(1, "other-chat", "DP-1", false),
            window(2, "FixtureChat", "DP-1", true),
        ];

        assert_eq!(
            preferred_window(&windows[..1], &app, &apps, Some("DP-1")),
            None
        );
        assert_eq!(
            preferred_window(&windows, &app, &apps, Some("DP-1")),
            Some(2)
        );
        windows.push(window(3, "org.rixde.ChatFixture", "DP-2", false));
        assert_eq!(
            preferred_window(&windows, &app, &apps, Some("DP-1")),
            Some(3)
        );
        windows.push(window(4, "FixtureChat", "DP-1", false));
        assert_eq!(
            preferred_window(&windows, &app, &apps, Some("DP-1")),
            Some(4)
        );
        windows[2].is_active = true;
        assert_eq!(
            preferred_window(&windows, &app, &apps, Some("DP-1")),
            Some(3)
        );
    }

    #[test]
    fn opening_running_app_sends_focus_without_launching_its_unavailable_executable() {
        use std::{
            io::{BufRead, BufReader, Write},
            os::unix::net::UnixListener,
            time::{Duration, Instant},
        };
        const FIXTURE: &str = "RIX_OPEN_APP_FIXTURE";
        if std::env::var_os(FIXTURE).is_none() {
            let root = tempfile::tempdir().unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "tasks::tests::opening_running_app_sends_focus_without_launching_its_unavailable_executable", "--nocapture"])
                .env(FIXTURE, "1")
                .env("RIX_IPC_SOCKET", root.path().join("fixture.sock"))
                .spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success());
                    return;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("Existing-window launch fixture timed out");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let listener = UnixListener::bind(std::env::var_os("RIX_IPC_SOCKET").unwrap()).unwrap();
        let request = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: rix_ipc::IpcRequest = serde_json::from_str(&line).unwrap();
            stream.write_all(b"{\"status\":\"ok\"}\n").unwrap();
            request
        });
        let app = app();
        WINDOWS.with(|cache| *cache.borrow_mut() = vec![window(42, "FixtureChat", "DP-1", true)]);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        crate::launch::open_app(&app, std::slice::from_ref(&app), "en");
        assert_eq!(
            request.join().unwrap(),
            rix_ipc::IpcRequest::FocusWindow(42)
        );
    }
}
