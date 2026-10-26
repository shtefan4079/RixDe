use super::*;

async fn settle(app: &mut App, receiver: &mpsc::Receiver<Message>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        for message in receiver.try_iter() {
            app.message(message);
        }
        if app.operation.is_none() && !app.tabs[app.active].loading {
            return;
        }
        assert!(Instant::now() < deadline, "Undo worker did not finish");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn keyboard_navigation_preserves_multiselection_and_supports_page_keys() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..24 {
        std::fs::write(root.path().join(format!("file-{index:02}.txt")), "data").unwrap();
    }
    let (window, mut app, receiver) = tests::app(root.path().into());
    app.prefs.grid_view = false;
    app.load();
    settle(&mut app, &receiver).await;
    app.tabs[0].select(0, false, false);
    app.tabs[0].select(2, true, false);
    let chosen = app.tabs[0].selected.clone();
    let key = |value| char::from(value).to_string();
    app.key(&key(slint::platform::Key::DownArrow), true, false, false);
    assert_eq!(app.tabs[0].focus, Some(3));
    assert_eq!(
        app.tabs[0].selected, chosen,
        "Ctrl+arrow moves focus without replacing selection"
    );
    app.key(" ", true, false, false);
    assert!(app.tabs[0].selected.contains(&app.tabs[0].entries[3].path));
    app.key(" ", true, false, false);
    assert_eq!(app.tabs[0].selected, chosen);
    let page = window.get_visible_rows().max(1) as usize;
    app.key(&key(slint::platform::Key::PageDown), false, false, false);
    assert_eq!(app.tabs[0].focus, Some((3 + page).min(23)));
    app.key(&key(slint::platform::Key::Home), false, false, false);
    app.key(&key(slint::platform::Key::DownArrow), false, true, false);
    assert_eq!(app.tabs[0].selected.len(), 2);
    app.key("a", true, false, false);
    assert_eq!(app.tabs[0].selected.len(), 24);
    app.key("a", true, true, false);
    assert!(app.tabs[0].selected.is_empty());
}

#[test]
fn controller_shortcuts_undo_redo_real_files_and_preserve_external_edits() {
    if std::env::var_os("RIX_FILES_UNDO_CONTROLLER_FIXTURE").is_none() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&data).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "controller::undo_tests::controller_shortcuts_undo_redo_real_files_and_preserve_external_edits", "--nocapture"])
            .env("RIX_FILES_UNDO_CONTROLLER_FIXTURE", "1")
            .env("HOME", &home).env("XDG_DATA_HOME", &data)
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let (window, mut app, receiver) = tests::app(home.clone());
        app.load();
        settle(&mut app, &receiver).await;
        app.key("n", true, false, true);
        assert_eq!(app.dialog_kind, 6);
        app.confirm("created.txt".into());
        settle(&mut app, &receiver).await;
        let original = home.join("created.txt");
        assert!(original.is_file());
        assert!(window.get_can_undo());
        app.key("z", true, false, false);
        settle(&mut app, &receiver).await;
        assert!(!original.exists());
        assert!(window.get_can_redo());
        app.key("z", true, true, false);
        settle(&mut app, &receiver).await;
        assert!(original.is_file());
        app.key("n", true, true, false);
        app.confirm("created-folder".into());
        settle(&mut app, &receiver).await;
        let folder = home.join("created-folder");
        app.navigate(folder.clone());
        settle(&mut app, &receiver).await;
        app.key("z", true, false, false);
        settle(&mut app, &receiver).await;
        assert!(!folder.exists());
        assert_eq!(
            app.tabs[0].path, home,
            "Undoing the currently open folder returns to its existing parent"
        );
        app.key("y", true, false, false);
        settle(&mut app, &receiver).await;
        assert!(folder.is_dir());
        let index = app.tabs[0]
            .entries
            .iter()
            .position(|entry| entry.path == original)
            .unwrap();
        app.tabs[0].select(index, false, false);
        app.key(
            &char::from(slint::platform::Key::F2).to_string(),
            false,
            false,
            false,
        );
        app.confirm("renamed.txt".into());
        settle(&mut app, &receiver).await;
        let renamed = home.join("renamed.txt");
        assert!(renamed.is_file());
        assert!(!original.exists());
        app.key("\u{1a}", true, false, false);
        settle(&mut app, &receiver).await;
        assert!(original.is_file());
        assert!(!renamed.exists());
        app.key("y", true, false, false);
        settle(&mut app, &receiver).await;
        assert!(renamed.is_file());
        std::fs::write(&renamed, "edited by another application").unwrap();
        app.key("z", true, false, false);
        settle(&mut app, &receiver).await;
        assert_eq!(
            app.dialog_kind, 4,
            "History must report an external edit instead of modifying it"
        );
        assert_eq!(
            std::fs::read_to_string(&renamed).unwrap(),
            "edited by another application"
        );
        assert!(!original.exists());
    });
}
