use super::*;

#[test]
fn configured_builtin_places_include_only_standard_sidebar_folders() {
    let root = tempfile::tempdir().unwrap();
    let folders = [
        ("DESKTOP", "Desktop"),
        ("DOWNLOAD", "Downloads"),
        ("DOCUMENTS", "Documents"),
        ("PICTURES", "Pictures"),
        ("MUSIC", "Music"),
        ("VIDEOS", "Videos"),
        ("PROJECTS", "Projects"),
        ("TEMPLATES", "Templates"),
        ("PUBLICSHARE", "Public"),
    ];
    let mut content = String::new();
    for (key, name) in folders {
        std::fs::create_dir(root.path().join(name)).unwrap();
        content.push_str(&format!("XDG_{key}_DIR=\"$HOME/{name}\"\n"));
    }
    let places = configured_places(root.path(), &content);
    assert_eq!(places.len(), 6);
    assert_eq!(
        places
            .iter()
            .map(|place| place.name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "Desktop",
            "Downloads",
            "Documents",
            "Pictures",
            "Music",
            "Videos"
        ]
    );
    assert!(places.iter().all(|place| place.builtin));
    for omitted in ["Projects", "Templates", "Public"] {
        assert!(root.path().join(omitted).is_dir());
        assert!(!places
            .iter()
            .any(|place| place.path == root.path().join(omitted)));
    }
    let favorites = Preferences {
        bookmarks: vec![root.path().join("Projects"), root.path().join("Templates")],
        ..Default::default()
    };
    let sidebar = super::places(&favorites);
    for favorite in &favorites.bookmarks {
        assert!(sidebar
            .iter()
            .any(|place| &place.path == favorite && !place.builtin));
    }
}

fn app_at(root: &Path, prefs: Preferences) -> (FilesWindow, App, mpsc::Receiver<Message>) {
    i_slint_backend_testing::init_no_event_loop();
    let window = FilesWindow::new().unwrap();
    let (sender, receiver) = mpsc::channel();
    let mut app = App::new(
        &window,
        vec![Tab::new(1, root.into(), false, SortBy::Name)],
        prefs,
        ThemeSettings::default(),
        sender,
        Arc::new(AtomicBool::new(false)),
    );
    app.preference_directory = root.join("test-preferences");
    (window, app, receiver)
}

async fn saved(app: &App) -> Preferences {
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.preference_writes.lock().unwrap().running {
        assert!(
            Instant::now() < deadline,
            "Preference worker did not finish"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    serde_json::from_slice(&std::fs::read(app.preference_directory.join("files.json")).unwrap())
        .unwrap()
}

#[tokio::test]
async fn unpin_bookmarks_changes_only_the_sidebar_and_saves_raw_paths() {
    use std::os::unix::ffi::OsStringExt;
    let root = tempfile::tempdir().unwrap();
    let chosen = root
        .path()
        .join(std::ffi::OsString::from_vec(b"chosen-\xff".to_vec()));
    let retained = root.path().join("retained");
    std::fs::create_dir(&chosen).unwrap();
    std::fs::create_dir(&retained).unwrap();
    std::fs::write(chosen.join("safe"), "preserve contents").unwrap();
    let prefs = Preferences {
        bookmarks: vec![chosen.clone(), retained.clone()],
        ..Default::default()
    };
    let (_window, mut app, _receiver) = app_at(root.path(), prefs);
    let index = app
        .places
        .iter()
        .position(|place| place.path == chosen)
        .unwrap();
    assert!(!app.places[index].builtin);
    app.unpin_place(index);
    assert!(!app.places.iter().any(|place| place.path == chosen));
    assert_eq!(app.prefs.bookmarks, vec![retained.clone()]);
    assert!(app.prefs.hidden_places.is_empty());
    assert_eq!(
        std::fs::read_to_string(chosen.join("safe")).unwrap(),
        "preserve contents"
    );
    let saved = saved(&app).await;
    assert_eq!(saved.bookmarks, vec![retained]);
    app.unpin_place(usize::MAX);
    assert!(chosen.is_dir());
}

#[tokio::test]
async fn builtins_can_be_hidden_and_restored_without_recreating_removed_bookmarks() {
    let root = tempfile::tempdir().unwrap();
    let bookmark = root.path().join("kept-favorite");
    let removed = root.path().join("removed-favorite");
    std::fs::create_dir(&bookmark).unwrap();
    std::fs::create_dir(&removed).unwrap();
    let prefs = Preferences {
        bookmarks: vec![bookmark.clone(), removed.clone()],
        ..Default::default()
    };
    let (_window, mut app, _receiver) = app_at(root.path(), prefs);
    for path in [home(), PathBuf::from("/"), PathBuf::from("trash:")] {
        let index = app
            .places
            .iter()
            .position(|place| place.path == path)
            .unwrap();
        assert!(app.places[index].builtin);
        app.unpin_place(index);
        assert!(!app.places.iter().any(|place| place.path == path));
        assert!(app.prefs.hidden_places.contains(&path));
    }
    let index = app
        .places
        .iter()
        .position(|place| place.path == removed)
        .unwrap();
    app.unpin_place(index);
    let hidden = saved(&app).await;
    assert_eq!(hidden.hidden_places, app.prefs.hidden_places);
    assert_eq!(hidden.bookmarks, vec![bookmark.clone()]);
    assert!(removed.is_dir());

    app.action("restore-places");
    assert!(app.prefs.hidden_places.is_empty());
    assert_eq!(app.prefs.bookmarks, vec![bookmark.clone()]);
    for path in [
        home(),
        PathBuf::from("/"),
        PathBuf::from("trash:"),
        bookmark,
    ] {
        assert!(app.places.iter().any(|place| place.path == path));
    }
    assert!(!app.places.iter().any(|place| place.path == removed));
    assert!(saved(&app).await.hidden_places.is_empty());
}

#[tokio::test]
async fn rebookmarking_a_hidden_builtin_restores_one_visible_entry() {
    let root = tempfile::tempdir().unwrap();
    let (_window, mut app, _receiver) = app_at(root.path(), Preferences::default());
    let index = app
        .places
        .iter()
        .position(|place| place.path == Path::new("/"))
        .unwrap();
    app.unpin_place(index);
    app.tabs[0].path = "/".into();
    app.action("bookmark");
    assert!(!app.prefs.hidden_places.contains(&PathBuf::from("/")));
    assert_eq!(
        app.places
            .iter()
            .filter(|place| place.path == Path::new("/"))
            .count(),
        1
    );
    saved(&app).await;
}

#[test]
fn sidebar_highlights_the_deepest_place_and_marks_the_favorites_group() {
    let root = tempfile::tempdir().unwrap();
    let favorite = root.path().join("favorite");
    let prefs = Preferences {
        bookmarks: vec![favorite.clone()],
        ..Default::default()
    };
    let (window, mut app, _receiver) = app_at(root.path(), prefs);
    app.tabs[0].path = favorite.join("nested");
    app.render();
    let favorite_index = app
        .places
        .iter()
        .position(|place| place.path == favorite)
        .unwrap();
    assert_eq!(
        active_place(&app.places, &app.tabs[0].path),
        Some(favorite_index)
    );
    let model = window.get_places();
    let item = model.row_data(favorite_index).unwrap();
    assert!(item.active);
    assert!(item.group_start);
    assert!(item.removable);
    assert!(app.places[..favorite_index]
        .iter()
        .all(|place| place.builtin));
    assert_eq!(model.iter().filter(|item| item.active).count(), 1);
    app.tabs[0].path = "trash:".into();
    app.render();
    let trash = app
        .places
        .iter()
        .position(|place| place.path == Path::new("trash:"))
        .unwrap();
    assert!(window.get_places().row_data(trash).unwrap().active);
}

#[test]
fn disk_results_follow_the_current_folder_and_ignore_previous_requests() {
    let root = tempfile::tempdir().unwrap();
    let (window, mut app, _receiver) = app_at(root.path(), Preferences::default());
    let usage = storage::DiskUsage {
        mount_path: "/".into(),
        source: "/dev/example".into(),
        total_bytes: 100 * 1024 * 1024 * 1024,
        used_bytes: 60 * 1024 * 1024 * 1024,
        available_bytes: 35 * 1024 * 1024 * 1024,
        usage_ratio: 0.65,
    };
    app.disk_request = Some(2);
    app.message(Message::Disk {
        request: 1,
        path: root.path().into(),
        result: Ok(usage.clone()),
    });
    assert_eq!(app.disk_request, Some(2));
    assert!(!window.get_disk_ready());
    app.message(Message::Disk {
        request: 2,
        path: "/previous-folder".into(),
        result: Ok(usage.clone()),
    });
    assert!(app.disk_path.is_none());
    app.disk_request = Some(3);
    app.message(Message::Disk {
        request: 3,
        path: root.path().into(),
        result: Ok(usage),
    });
    assert!(window.get_disk_ready());
    assert_eq!(window.get_disk_usage(), "65%");
    assert_eq!(window.get_disk_mount(), "/");
    assert!(window
        .get_disk_detail()
        .replace('\u{00a0}', " ")
        .contains("35.0 GiB"));
    app.tabs[0].path = "/another-folder".into();
    app.render();
    assert!(
        !window.get_disk_ready(),
        "A new tab must not display the previous disk's capacity"
    );
    assert!(window.get_disk_mount().is_empty());
}

#[test]
fn responsive_grid_rows_and_keyboard_share_the_current_column_count() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..11 {
        std::fs::write(root.path().join(format!("file-{index:02}")), "data").unwrap();
    }
    let prefs = Preferences {
        grid_view: true,
        ..Default::default()
    };
    let (window, mut app, _receiver) = app_at(root.path(), prefs);
    app.tabs[0].install_entries(
        fs::read_directory(
            root.path(),
            false,
            SortBy::Name,
            false,
            &AtomicBool::new(false),
        )
        .unwrap(),
    );
    assert_eq!(app.grid_columns, 5);
    app.update_grid_columns(3);
    let rows = window.get_grid_rows();
    assert_eq!(rows.row_count(), 4);
    for index in 0..4 {
        let row = rows.row_data(index).unwrap();
        assert_eq!(row.start, (index * 3) as i32);
        assert_eq!(row.items.row_count(), 3);
    }
    assert!(rows
        .row_data(3)
        .unwrap()
        .items
        .row_data(2)
        .unwrap()
        .name
        .is_empty());
    app.tabs[0].select(0, false, false);
    app.key(
        &char::from(slint::platform::Key::DownArrow).to_string(),
        false,
        false,
        false,
    );
    assert_eq!(app.tabs[0].focus, Some(3));
    app.key(
        &char::from(slint::platform::Key::UpArrow).to_string(),
        false,
        false,
        false,
    );
    assert_eq!(app.tabs[0].focus, Some(0));
    app.update_grid_columns(7);
    assert_eq!(window.get_grid_rows().row_count(), 2);
    assert_eq!(window.get_grid_rows().row_data(1).unwrap().start, 7);
    app.key(
        &char::from(slint::platform::Key::DownArrow).to_string(),
        false,
        false,
        false,
    );
    assert_eq!(app.tabs[0].focus, Some(7));
    app.update_grid_columns(0);
    assert_eq!(app.grid_columns, 1);
    app.update_grid_columns(1000);
    assert_eq!(app.grid_columns, 24);
}

#[tokio::test]
async fn view_buttons_set_explicit_modes_and_blank_area_clears_selection() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file"), "data").unwrap();
    let (window, mut app, _receiver) = app_at(root.path(), Preferences::default());
    app.tabs[0].install_entries(
        fs::read_directory(
            root.path(),
            false,
            SortBy::Name,
            false,
            &AtomicBool::new(false),
        )
        .unwrap(),
    );
    app.tabs[0].select(0, false, false);
    app.action("clear-selection");
    assert!(app.tabs[0].selected.is_empty());
    assert!(app.tabs[0].focus.is_none());
    assert!(app.tabs[0].anchor.is_none());
    assert_eq!(window.get_selection_count(), 0);
    app.action("grid");
    assert!(app.prefs.grid_view);
    assert!(window.get_grid_view());
    app.action("grid");
    assert!(app.prefs.grid_view);
    app.action("list");
    assert!(!app.prefs.grid_view);
    assert!(!window.get_grid_view());
    app.action("list");
    assert!(!app.prefs.grid_view);
    assert!(!saved(&app).await.grid_view);
}

#[tokio::test]
async fn alt_enter_shows_the_selected_items_properties() {
    let root = tempfile::tempdir().unwrap();
    let chosen = root.path().join("chosen");
    std::fs::write(&chosen, "data").unwrap();
    let (window, mut app, receiver) = app_at(root.path(), Preferences::default());
    app.tabs[0].install_entries(
        fs::read_directory(
            root.path(),
            false,
            SortBy::Name,
            false,
            &AtomicBool::new(false),
        )
        .unwrap(),
    );
    app.tabs[0].select(0, false, false);
    app.key(
        &char::from(slint::platform::Key::Return).to_string(),
        false,
        false,
        true,
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.dialog_kind != 10 {
        for message in receiver.try_iter() {
            app.message(message);
        }
        assert!(
            Instant::now() < deadline,
            "Properties worker did not finish"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(app.dialog_paths, vec![chosen.clone()]);
    assert!(window
        .global::<PropertiesView>()
        .get_location()
        .as_str()
        .contains(chosen.to_string_lossy().as_ref()));
}

#[tokio::test]
async fn context_open_targets_the_clicked_selected_entry_without_losing_multiselection() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("a");
    let clicked = root.path().join("b");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&clicked).unwrap();
    std::fs::write(first.join("first-folder-file"), "a").unwrap();
    std::fs::write(clicked.join("clicked-folder-file"), "b").unwrap();
    let (_window, mut app, receiver) = app_at(root.path(), Preferences::default());
    app.tabs[0].install_entries(
        fs::read_directory(
            root.path(),
            false,
            SortBy::Name,
            false,
            &AtomicBool::new(false),
        )
        .unwrap(),
    );
    app.tabs[0].select(1, false, false);
    app.tabs[0].select(0, true, false);
    assert_eq!(app.tabs[0].focus, Some(0));
    assert_eq!(
        app.tabs[0].selected_paths(),
        vec![first.clone(), clicked.clone()]
    );
    app.context_entry(1);
    assert_eq!(app.tabs[0].focus, Some(1));
    assert_eq!(app.tabs[0].selected_paths(), vec![first, clicked.clone()]);
    app.action("open");
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.tabs[0].loading {
        for message in receiver.try_iter() {
            app.message(message);
        }
        assert!(
            Instant::now() < deadline,
            "Context-open folder did not finish loading"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(app.tabs[0].path, clicked);
    assert_eq!(app.tabs[0].entries.len(), 1);
    assert_eq!(app.tabs[0].entries[0].name, "clicked-folder-file");
}

#[tokio::test]
async fn sidebar_width_and_collapse_state_work_and_persist() {
    let root = tempfile::tempdir().unwrap();
    let prefs = Preferences {
        sidebar_width: Some(300.0),
        sidebar_collapsed: true,
        ..Default::default()
    };
    let (window, app, _receiver) = app_at(root.path(), prefs);
    assert_eq!(window.get_sidebar_width(), 300.0);
    assert!(window.get_sidebar_collapsed());
    assert_eq!(window.get_effective_sidebar_width(), 0.0);

    window.set_sidebar_collapsed(false);
    assert_eq!(window.get_effective_sidebar_width(), 300.0);

    window.set_sidebar_width(250.0);
    app.save();
    let saved = saved(&app).await;
    assert_eq!(saved.sidebar_width, Some(250.0));
    assert_eq!(saved.sidebar_collapsed, false);
}

#[tokio::test]
async fn disk_and_place_context_actions() {
    let root = tempfile::tempdir().unwrap();
    let folder_a = root.path().join("folder_a");
    std::fs::create_dir(&folder_a).unwrap();
    let prefs = Preferences {
        bookmarks: vec![folder_a.clone()],
        ..Default::default()
    };
    let (_window, mut app, receiver) = app_at(root.path(), prefs);

    assert_eq!(app.tabs.len(), 1);

    app.action("open-disk-tab");
    assert_eq!(app.tabs.len(), 2);
    assert_eq!(app.tabs[1].path, app.disk_target_path());

    app.action("disk-properties");
    let disk_paths = loop {
        match receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("Message::Properties for disk")
        {
            Message::Properties { paths, .. } => break paths,
            _ => continue,
        }
    };
    assert_eq!(disk_paths, vec![app.disk_target_path()]);

    let place_index = app.places.iter().position(|p| p.path == folder_a).unwrap();
    app.action_on_place("open-tab", place_index);
    assert_eq!(app.tabs.len(), 3);
    assert_eq!(app.tabs[2].path, folder_a);

    app.action_on_place("properties", place_index);
    let place_paths = loop {
        match receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("Message::Properties for place")
        {
            Message::Properties { paths, .. } => break paths,
            _ => continue,
        }
    };
    assert_eq!(place_paths, vec![folder_a]);
}

#[tokio::test]
async fn tab_context_actions_duplicate_close_others_and_close_right() {
    let root = tempfile::tempdir().unwrap();
    let path1 = root.path().join("t1");
    let path2 = root.path().join("t2");
    let path3 = root.path().join("t3");
    let path4 = root.path().join("t4");
    for p in [&path1, &path2, &path3, &path4] {
        std::fs::create_dir(p).unwrap();
    }
    let (_window, mut app, _receiver) = app_at(root.path(), Preferences::default());
    app.tabs = vec![
        Tab::new(1, path1.clone(), false, SortBy::Name),
        Tab::new(2, path2.clone(), false, SortBy::Name),
        Tab::new(3, path3.clone(), false, SortBy::Name),
        Tab::new(4, path4.clone(), false, SortBy::Name),
    ];
    app.active = 1;
    assert_eq!(app.tabs.len(), 4);

    app.action_on_tab("duplicate", 1);
    assert_eq!(app.tabs.len(), 5);
    assert_eq!(app.tabs[4].path, path2);

    app.action_on_tab("close-right", 1);
    assert_eq!(app.tabs.len(), 2);
    assert_eq!(app.tabs[0].path, path1);
    assert_eq!(app.tabs[1].path, path2);

    app.action_on_tab("close-others", 1);
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(app.tabs[0].path, path2);
    assert_eq!(app.active, 0);
}
