use super::*;
#[test]
fn absolute_application_icons_reject_fifo_and_keep_regular_svg_readable() {
    let root = tempfile::tempdir().unwrap();
    let fifo = root.path().join("application-icon.svg");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(App::absolute_icon(&fifo).is_none());
    let (_window, mut app, _receiver) = app(root.path().into());
    let fallback = app.icon(fifo.to_str().unwrap());
    assert!(fallback
        .to_rgba8()
        .unwrap()
        .as_slice()
        .iter()
        .any(|pixel| pixel.a != 0));
    let svg = root.path().join("regular.svg");
    std::fs::write(&svg, b"<svg xmlns='http://www.w3.org/2000/svg' width='8' height='8'><rect width='8' height='8' fill='#00ff00'/></svg>").unwrap();
    let icon = App::absolute_icon(&svg).unwrap().to_rgba8().unwrap();
    assert!(icon
        .as_slice()
        .iter()
        .all(|pixel| pixel.g == 255 && pixel.a == 255));
    let compressed = root.path().join("oversized.svgz");
    let mut encoder = flate2::write::GzEncoder::new(
        std::fs::File::create(&compressed).unwrap(),
        flate2::Compression::default(),
    );
    let chunk = [b' '; 4096];
    for _ in 0..4097 {
        std::io::Write::write_all(&mut encoder, &chunk).unwrap();
    }
    encoder.finish().unwrap();
    assert!(
        App::absolute_icon(&compressed).is_none(),
        "Compressed icons must share the decoded byte bound"
    );
}
#[test]
fn changed_or_failed_system_clipboard_never_pastes_a_cached_cut() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("cut.txt");
    std::fs::write(&source, "keep source").unwrap();
    let (_window, mut app, receiver) = app(root.path().into());
    for error in [
        "The clipboard contains no files",
        "Invalid file clipboard",
        "Clipboard owner did not finish the file transfer in time",
        "Wayland connection failed",
    ] {
        app.clipboard = vec![source.clone()];
        app.cut = true;
        app.message(Message::ClipboardError(error.into()));
        assert!(app.clipboard.is_empty());
        assert!(!app.cut);
        assert!(app.operation.is_none());
        assert!(receiver.try_recv().is_err());
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "keep source");
    }
}
#[test]
fn empty_paste_result_clears_cut_state_without_an_operation() {
    let root = tempfile::tempdir().unwrap();
    let (_window, mut app, receiver) = app(root.path().into());
    app.cut = true;
    app.clipboard = vec![root.path().join("gone")];

    app.message(Message::Clipboard {
        paths: vec![],
        cut: true,
        destination: root.path().into(),
    });
    assert!(app.clipboard.is_empty());
    assert!(!app.cut);
    assert!(app.operation.is_none());
    assert!(receiver.try_recv().is_err());
}
#[tokio::test]
async fn external_show_items_classifies_locations_on_a_worker() {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("folder");
    std::fs::create_dir(&folder).unwrap();
    let file = folder.join("file.txt");
    std::fs::write(&file, "data").unwrap();
    let uri = |path: &std::path::Path| url::Url::from_file_path(path).unwrap().to_string();
    let (sender, receiver) = mpsc::channel();
    let manager = FileManager {
        sender,
        properties_metadata_request: Arc::new(AtomicU64::new(0)),
    };
    manager
        .send(vec![uri(&file), uri(&folder)], true)
        .await
        .unwrap();
    let Ok(Message::Incoming(locations)) = receiver.try_recv() else {
        panic!("expected a classified incoming message");
    };

    assert_eq!(locations.len(), 2);
    assert_eq!(locations[0].folder, folder);
    assert_eq!(locations[0].selected.as_deref(), Some(file.as_path()));
    assert_eq!(locations[1].folder, root.path());
    assert_eq!(locations[1].selected.as_deref(), Some(folder.as_path()));
    manager.send(vec![uri(&folder)], false).await.unwrap();
    let Ok(Message::Incoming(locations)) = receiver.try_recv() else {
        panic!("expected a classified incoming message");
    };

    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].folder, folder);
    assert!(locations[0].selected.is_none());
}
#[tokio::test]
async fn navigation_resolves_parent_segments_before_file_operations() {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("folder");
    std::fs::create_dir(&folder).unwrap();
    let source = root.path().join("file");
    std::fs::write(&source, "data").unwrap();
    let (_window, mut app, receiver) = app(folder.join(".."));
    app.tabs[0].selected.insert(folder.join("../file"));
    app.load();
    settle(&mut app, &receiver).await;
    assert_eq!(
        app.tabs[0].path,
        std::fs::canonicalize(root.path()).unwrap()
    );
    assert!(app.tabs[0].selected.contains(&source));
    app.action("rename");
    app.confirm("renamed".into());
    settle(&mut app, &receiver).await;
    assert_eq!(
        std::fs::read_to_string(root.path().join("renamed")).unwrap(),
        "data"
    );
}

#[tokio::test]
async fn paste_uses_the_destination_captured_before_navigation() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.txt");
    let chosen = root.path().join("chosen");
    let later = root.path().join("later");
    std::fs::write(&source, "content").unwrap();
    std::fs::create_dir(&chosen).unwrap();
    std::fs::create_dir(&later).unwrap();
    let (_window, mut app, receiver) = app(chosen.clone());
    app.navigate(later.clone());
    app.message(Message::Clipboard {
        paths: vec![source.clone()],
        cut: false,
        destination: chosen.clone(),
    });
    settle(&mut app, &receiver).await;
    assert_eq!(
        std::fs::read_to_string(chosen.join("source.txt")).unwrap(),
        "content"
    );
    assert!(!later.join("source.txt").exists());
    assert!(source.exists());
}
#[tokio::test]
async fn rapid_preferences_changes_persist_the_last_choice() {
    let root = tempfile::tempdir().unwrap();
    let (_window, mut app, _receiver) = app(root.path().into());
    app.preference_directory = root.path().join("settings");
    for i in 0..200 {
        app.prefs.show_hidden = i % 2 == 0;
        app.prefs.bookmarks = vec![root.path().join(format!("folder-{i}"))];
        app.save();
    }
    let deadline = Instant::now() + Duration::from_secs(4);
    while app.preference_writes.lock().unwrap().running {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let saved: Preferences = serde_json::from_slice(
        &std::fs::read(app.preference_directory.join("files.json")).unwrap(),
    )
    .unwrap();
    assert!(!saved.show_hidden);
    assert_eq!(saved.bookmarks, vec![root.path().join("folder-199")]);
}
#[tokio::test]
async fn arrows_start_at_first_file_and_ctrl_tab_cycles_tabs() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a"), "").unwrap();
    std::fs::write(root.path().join("b"), "").unwrap();
    let (_window, mut app, receiver) = app(root.path().into());
    app.load();
    settle(&mut app, &receiver).await;
    app.key(
        &char::from(slint::platform::Key::DownArrow).to_string(),
        false,
        false,
        false,
    );
    assert_eq!(app.tabs[0].focus, Some(0));
    app.key(
        &char::from(slint::platform::Key::DownArrow).to_string(),
        false,
        false,
        false,
    );
    assert_eq!(app.tabs[0].focus, Some(1));
    app.add_tab(root.path().into());
    settle(&mut app, &receiver).await;
    assert_eq!(app.active, 1);
    app.key(
        &char::from(slint::platform::Key::Tab).to_string(),
        true,
        false,
        false,
    );
    assert_eq!(app.active, 0);
    app.key(
        &char::from(slint::platform::Key::Tab).to_string(),
        true,
        true,
        false,
    );
    assert_eq!(app.active, 1);
}

#[tokio::test]
async fn modifier_events_select_files_and_create_folder_through_the_ui() {
    use slint::platform::{Key, WindowEvent};
    let root = tempfile::tempdir().unwrap();
    for name in ["first", "second", "third"] {
        std::fs::write(root.path().join(name), "").unwrap();
    }
    let (window, mut state, receiver) = app(root.path().into());
    state.load();
    settle(&mut state, &receiver).await;
    let state = Rc::new(RefCell::new(state));
    let handler = state.clone();
    window.on_keyboard(move |text, ctrl, shift, alt| {
        handler.borrow_mut().key(&text, ctrl, shift, alt)
    });
    let handler = state.clone();
    window.on_dismiss_dialog(move || handler.borrow_mut().dismiss());
    window.show().unwrap();
    window.invoke_focus_files();
    let press = |text: slint::SharedString| {
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        window
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text });
    };
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Control.into(),
    });
    press("a".into());
    assert_eq!(state.borrow().tabs[0].selected.len(), 3);
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Control.into(),
    });
    press(Key::Escape.into());
    assert!(state.borrow().tabs[0].selected.is_empty());
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Control.into(),
    });
    press("\u{1}".into());
    assert_eq!(
        state.borrow().tabs[0].selected.len(),
        3,
        "Ctrl control character must select all files"
    );
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Shift.into(),
    });
    press("N".into());
    assert_eq!(window.get_dialog_kind(), 1);
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Shift.into(),
    });
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Control.into(),
    });
    press(Key::Escape.into());
    assert_eq!(window.get_dialog_kind(), 0);
}
pub(crate) fn app(path: PathBuf) -> (FilesWindow, App, mpsc::Receiver<Message>) {
    i_slint_backend_testing::init_no_event_loop();
    let window = FilesWindow::new().unwrap();
    let (sender, receiver) = mpsc::channel();
    let theme = ThemeSettings::default();
    theme_bindings::apply(window.global::<Theme>(), window.global::<Palette>(), &theme);
    let app = App::new(
        &window,
        vec![Tab::new(1, path, false, SortBy::Name)],
        Preferences::default(),
        theme,
        sender,
        Arc::new(AtomicBool::new(false)),
    );
    (window, app, receiver)
}

#[tokio::test]
async fn select_all_during_a_rectangle_survives_the_next_refresh() {
    use slint::platform::{Key, WindowEvent};
    let root = tempfile::tempdir().unwrap();
    for name in ["first", "second", "third"] {
        std::fs::write(root.path().join(name), "").unwrap();
    }
    let (window, mut state, receiver) = app(root.path().into());
    state.load();
    settle(&mut state, &receiver).await;
    state.tabs[0].select(0, false, false);
    state.begin_rectangle(false, false);
    let state = Rc::new(RefCell::new(state));
    let handler = state.clone();
    window.on_keyboard(move |text, ctrl, shift, alt| {
        handler.borrow_mut().key(&text, ctrl, shift, alt)
    });
    window.show().unwrap();
    window.invoke_focus_files();
    for text in [slint::SharedString::from(Key::Control), "a".into()] {
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text });
    }
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text: "a".into() });
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Control.into(),
    });
    window.on_keyboard(|_, _, _, _| {});
    let mut app = Rc::try_unwrap(state)
        .unwrap_or_else(|_| panic!("Keyboard handler was removed"))
        .into_inner();
    assert_eq!(app.tabs[0].selected.len(), 3);
    app.load();
    settle(&mut app, &receiver).await;
    assert_eq!(
        app.tabs[0].selected.len(),
        3,
        "Refresh must not restore the gesture's old one-file selection"
    );
}
async fn settle(app: &mut App, receiver: &mpsc::Receiver<Message>) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        for message in receiver.try_iter() {
            app.message(message);
        }
        if !app.tabs[app.active].loading && app.operation.is_none() {
            break;
        }
        assert!(Instant::now() < deadline, "Workers failed to finish");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
#[tokio::test]
async fn stale_snapshots_do_not_replace_new_navigation_and_watcher_observes_changes() {
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    std::fs::write(a.join("old"), "").unwrap();
    std::fs::write(b.join("new"), "").unwrap();
    let (_window, mut app, receiver) = app(a.clone());
    app.load();
    let old_generation = app.tabs[0].generation;
    app.navigate(b.clone());
    settle(&mut app, &receiver).await;
    app.message(Message::Loaded {
        id: 1,
        generation: old_generation,
        resolved_path: None,
        result: Ok(
            fs::read_directory(&a, false, SortBy::Name, false, &AtomicBool::new(false)).unwrap(),
        ),
        trash: vec![],
        search_limited: false,
    });
    assert_eq!(app.tabs[0].entries[0].name, "new");
    app.dirty.store(false, Ordering::Relaxed);
    std::fs::write(b.join("second"), "").unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !app.dirty.load(Ordering::Relaxed) {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    app.load();
    settle(&mut app, &receiver).await;
    assert_eq!(app.tabs[0].entries.len(), 2);
}
#[tokio::test]
async fn permanent_delete_requires_confirmation_and_keeps_frozen_targets() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("chosen");
    let other = root.path().join("other");
    std::fs::write(&file, "selected").unwrap();
    std::fs::write(&other, "preserved").unwrap();
    let (_window, mut app, receiver) = app(root.path().into());
    app.load();
    settle(&mut app, &receiver).await;
    let index = app.tabs[0]
        .entries
        .iter()
        .position(|e| e.path == file)
        .unwrap();
    app.tabs[0].select(index, false, false);
    app.action("delete");
    assert!(file.exists());
    assert_eq!(app.dialog_paths, vec![file.clone()]);
    app.tabs[0].selected.clear();
    app.tabs[0].selected.insert(other.clone());
    app.confirm(String::new());
    settle(&mut app, &receiver).await;
    assert!(!file.exists());
    assert!(other.exists());
}
#[tokio::test]
async fn trash_view_delete_key_triggers_permanent_delete_dialog() {
    let (_window, mut app, _receiver) = app(PathBuf::from("trash:"));
    let trashed_file = PathBuf::from("/home/user/.local/share/Trash/files/test.txt");
    app.tabs[0].entries = vec![rix_files::fs::Entry {
        name: "test.txt".into(),
        path: trashed_file.clone(),
        size: 10,
        modified: Some(std::time::SystemTime::UNIX_EPOCH),
        is_dir: false,
        is_symlink: false,
        kind: "text/plain".into(),
        icon_name: "text-plain".into(),
        icon_names: vec![],
    }];
    app.tabs[0].selected.insert(trashed_file.clone());

    app.action("trash");
    assert_eq!(app.dialog_kind, 3);
    assert_eq!(app.dialog_paths, vec![trashed_file.clone()]);
    app.dismiss();

    app.tabs[0].selected.insert(trashed_file.clone());
    app.key(
        &char::from(slint::platform::Key::Delete).to_string(),
        false,
        false,
        false,
    );
    assert_eq!(app.dialog_kind, 3);
    assert_eq!(app.dialog_paths, vec![trashed_file]);
}
#[tokio::test]
async fn hidden_files_are_marked_as_hidden() {
    let root = tempfile::tempdir().unwrap();
    let hidden = root.path().join(".hidden_file");
    let normal = root.path().join("normal_file");
    std::fs::write(&hidden, "hidden").unwrap();
    std::fs::write(&normal, "normal").unwrap();
    let (_window, mut app, receiver) = app(root.path().into());
    app.tabs[0].show_hidden = true;
    app.load();
    settle(&mut app, &receiver).await;
    let hidden_entry = app.tabs[0]
        .entries
        .iter()
        .find(|e| e.name == ".hidden_file")
        .unwrap();
    assert!(hidden_entry.name.starts_with('.'));
    let normal_entry = app.tabs[0]
        .entries
        .iter()
        .find(|e| e.name == "normal_file")
        .unwrap();
    assert!(!normal_entry.name.starts_with('.'));
}
#[tokio::test]
async fn ipc_theme_snapshot_events_and_reconnect_update_the_same_window() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("ipc.sock");
    let config = Arc::new(rix_config::ConfigManager::init_in(root.path().join("config")).unwrap());
    let mut light = ThemeSettings::crisp_light();
    light.icon_theme = "test-icons".into();
    config.save_theme(light.clone()).unwrap();
    let server = Arc::new(rix_ipc::IpcServer::new_with_path(
        config.clone(),
        socket.clone(),
    ));
    let owner = server.clone();
    let task = tokio::spawn(async move {
        owner.run().await.unwrap();
    });
    let (window, mut app, receiver) = app(root.path().into());
    let subscription = theme_service_at(app.sender.clone(), socket.clone());
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.theme != light {
        for message in receiver.try_iter() {
            app.message(message);
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        window.global::<Theme>().get_font_family().as_str(),
        light.appearance.font_family
    );
    assert_eq!(app.theme.icon_theme, "test-icons");
    let mut updated = light.clone();
    updated.accent_color = "#c056ff".into();
    config.save_theme(updated.clone()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.theme != updated {
        for message in receiver.try_iter() {
            app.message(message);
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        window.global::<Theme>().get_accent(),
        slint::Color::from_rgb_u8(0xc0, 0x56, 0xff)
    );
    task.abort();
    let _ = task.await;
    drop(server);

    let next = ThemeSettings::cyber_dark();
    config.save_theme(next.clone()).unwrap();
    let owner = Arc::new(rix_ipc::IpcServer::new_with_path(config, socket));
    let task = tokio::spawn(async move {
        owner.run().await.unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(7);
    while app.theme != next {
        for message in receiver.try_iter() {
            app.message(message);
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    subscription.abort();
    task.abort();
}

#[tokio::test]
async fn ipc_application_theme_rule_updates_the_open_window_and_restores_live_theme() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("ipc.sock");
    let config = Arc::new(rix_config::ConfigManager::init_in(root.path().join("config")).unwrap());
    let server = Arc::new(rix_ipc::IpcServer::new_with_path(
        config.clone(),
        socket.clone(),
    ));
    let owner = server.clone();
    let server_task = tokio::spawn(async move {
        owner.run().await.unwrap();
    });
    let (window, mut app, receiver) = app(root.path().into());
    let subscription = theme_service_at(app.sender.clone(), socket);
    let mut custom = ThemeSettings::cyber_dark();
    custom.accent_color = "#d04cff".into();
    let mut desktop = config.get_desktop();
    desktop
        .window_appearance
        .rules
        .push(rix_config::WindowAppearanceRule {
            app_id: APPLICATION_ID.into(),
            use_desktop_material: false,
            theme: Some(custom.clone()),
        });
    config.save_desktop(desktop.clone()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.desktop_material_enabled || app.theme.name != custom.name {
        for message in receiver.try_iter() {
            app.message(message);
        }
        assert!(
            Instant::now() < deadline,
            "Rule must update the existing Rix Files window"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!window.get_desktop_material_enabled());
    assert!(!window.global::<Theme>().get_is_aero());
    assert_eq!(
        window.global::<Theme>().get_accent(),
        slint::Color::from_rgb_u8(0xd0, 0x4c, 0xff)
    );
    let mut next = ThemeSettings::crisp_light();
    next.accent_color = "#167cba".into();
    config.save_theme(next.clone()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.desktop_theme != next {
        for message in receiver.try_iter() {
            app.message(message);
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        window.global::<Theme>().get_accent(),
        slint::Color::from_rgb_u8(0xd0, 0x4c, 0xff),
        "Saved application theme must survive a global theme change"
    );
    desktop.window_appearance.rules.clear();
    config.save_desktop(desktop).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.theme != next || !app.desktop_material_enabled {
        for message in receiver.try_iter() {
            app.message(message);
        }
        assert!(
            Instant::now() < deadline,
            "Removing a rule must restore the latest desktop theme"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(window.get_desktop_material_enabled());
    assert_eq!(
        window.global::<Theme>().get_accent(),
        slint::Color::from_rgb_u8(0x16, 0x7c, 0xba)
    );
    subscription.abort();
    server_task.abort();
}

#[test]
fn time_limited_search_shows_a_status_note() {
    let root = tempfile::tempdir().unwrap();
    let (window, mut app, _receiver) = app(root.path().into());
    app.tabs[0].query = "needle".into();
    app.tabs[0].search_limited = true;
    app.render();
    assert!(
        window.get_status().contains("search limited by time"),
        "status was {}",
        window.get_status()
    );
}
