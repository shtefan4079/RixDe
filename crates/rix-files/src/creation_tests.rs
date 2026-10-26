use super::*;

fn app_at(path: PathBuf) -> (FilesWindow, App, mpsc::Receiver<Message>) {
    i_slint_backend_testing::init_no_event_loop();
    let window = FilesWindow::new().unwrap();
    let (sender, receiver) = mpsc::channel();
    let app = App::new(
        &window,
        vec![Tab::new(1, path, false, SortBy::Name)],
        Preferences::default(),
        ThemeSettings::default(),
        sender,
        Arc::new(AtomicBool::new(false)),
    );
    (window, app, receiver)
}

async fn settle(app: &mut App, receiver: &mpsc::Receiver<Message>) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        for message in receiver.try_iter() {
            app.message(message);
        }
        if app.operation.is_none() && !app.tabs[app.active].loading {
            return;
        }
        assert!(Instant::now() < deadline, "Creation worker did not finish");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn actions_and_confirmation_create_real_files_and_folders_in_raw_byte_parents() {
    use std::os::unix::ffi::OsStringExt;
    let root = tempfile::tempdir().unwrap();
    let parent = root
        .path()
        .join(std::ffi::OsString::from_vec(b"parent-\xff".to_vec()));
    std::fs::create_dir(&parent).unwrap();
    let (_window, mut app, receiver) = app_at(parent.clone());
    app.action("new-file");
    assert_eq!(app.dialog_kind, 6);
    assert_eq!(app.dialog_paths, vec![parent.clone()]);
    app.confirm("created.txt".into());
    settle(&mut app, &receiver).await;
    let file = parent.join("created.txt");
    assert!(file.is_file());
    assert_eq!(std::fs::metadata(&file).unwrap().len(), 0);
    assert!(app.tabs[0].entries.iter().any(|entry| entry.path == file));

    app.action("new-folder");
    assert_eq!(app.dialog_kind, 1);
    app.confirm("created folder".into());
    settle(&mut app, &receiver).await;
    assert!(parent.join("created folder").is_dir());
}

#[tokio::test]
async fn creation_keeps_the_dialog_destination_when_navigation_changes() {
    let root = tempfile::tempdir().unwrap();
    let chosen = root.path().join("chosen");
    let later = root.path().join("later");
    std::fs::create_dir(&chosen).unwrap();
    std::fs::create_dir(&later).unwrap();
    let (_window, mut app, receiver) = app_at(chosen.clone());
    for (action, name) in [("new-file", "file"), ("new-folder", "folder")] {
        app.navigate(chosen.clone());
        settle(&mut app, &receiver).await;
        app.action(action);
        app.navigate(later.clone());
        settle(&mut app, &receiver).await;
        assert_eq!(app.dialog_paths, vec![chosen.clone()]);
        app.confirm(name.into());
        settle(&mut app, &receiver).await;
        assert!(chosen.join(name).exists());
        assert!(!later.join(name).exists());
        assert_eq!(app.tabs[app.active].path, later);
    }
}

#[tokio::test]
async fn collisions_and_invalid_names_report_errors_without_replacing_or_escaping() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("parent");
    std::fs::create_dir(&parent).unwrap();
    let existing = parent.join("existing");
    std::fs::write(&existing, "preserve").unwrap();
    let (window, mut app, receiver) = app_at(parent.clone());
    for action in ["new-file", "new-folder"] {
        for name in ["existing", "../escape", "nested/file", "nul\0byte"] {
            app.action(action);
            app.confirm(name.into());
            settle(&mut app, &receiver).await;
            assert_eq!(app.dialog_kind, 4);
            assert!(!window.get_dialog_text().is_empty());
            assert_eq!(std::fs::read_to_string(&existing).unwrap(), "preserve");
            assert!(!parent.join("nested").exists());
            assert!(!root.path().join("escape").exists());
            app.dismiss();
        }
    }
}

#[tokio::test]
async fn disappeared_destination_never_falls_back_to_the_current_directory() {
    let root = tempfile::tempdir().unwrap();
    let chosen = root.path().join("chosen");
    let later = root.path().join("later");
    std::fs::create_dir(&chosen).unwrap();
    std::fs::create_dir(&later).unwrap();
    let (window, mut app, receiver) = app_at(chosen.clone());
    app.action("new-file");
    std::fs::remove_dir(&chosen).unwrap();
    app.navigate(later.clone());
    settle(&mut app, &receiver).await;
    app.confirm("file".into());
    settle(&mut app, &receiver).await;
    assert_eq!(app.dialog_kind, 4);
    assert!(!window.get_dialog_text().is_empty());
    assert!(!later.join("file").exists());
    assert!(!chosen.exists());
}

#[tokio::test]
async fn dismissed_and_duplicate_confirmations_cannot_create_stale_files() {
    let root = tempfile::tempdir().unwrap();
    let (_window, mut app, receiver) = app_at(root.path().into());
    app.action("new-file");
    app.dismiss();
    app.confirm("dismissed".into());
    assert!(app.operation.is_none());
    assert!(!root.path().join("dismissed").exists());
    app.action("new-file");
    app.confirm("chosen".into());
    app.confirm("duplicate".into());
    settle(&mut app, &receiver).await;
    assert!(root.path().join("chosen").is_file());
    assert!(!root.path().join("duplicate").exists());
}

#[test]
fn cancelling_a_queued_creation_preserves_the_directory() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let root = tempfile::tempdir().unwrap();
        let (_window, mut app, receiver) = app_at(root.path().into());
        let (started_sender, started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            started_sender.send(()).unwrap();
            release_receiver.recv().unwrap();
        });
        started_receiver
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        app.action("new-file");
        app.confirm("cancelled".into());
        app.action("cancel-operation");
        release_sender.send(()).unwrap();
        blocker.await.unwrap();
        settle(&mut app, &receiver).await;
        assert!(!root.path().join("cancelled").exists());
        assert!(app.notice.as_ref().unwrap().contains("cancel"));
    });
}

#[tokio::test]
async fn advertised_creation_shortcuts_open_the_corresponding_dialogs() {
    let root = tempfile::tempdir().unwrap();
    let (_window, mut app, _receiver) = app_at(root.path().into());
    app.key("N", true, true, false);
    assert_eq!(app.dialog_kind, 1);
    assert_eq!(app.dialog_paths, vec![root.path().to_path_buf()]);
    app.dismiss();
    app.key("n", true, false, true);
    assert_eq!(app.dialog_kind, 6);
    assert_eq!(app.dialog_paths, vec![root.path().to_path_buf()]);
}
