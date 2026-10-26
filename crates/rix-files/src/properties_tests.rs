use super::*;

fn service() -> (FileManager, mpsc::Receiver<Message>) {
    let (sender, receiver) = mpsc::channel();
    (
        FileManager {
            sender,
            properties_metadata_request: Arc::new(AtomicU64::new(0)),
        },
        receiver,
    )
}

#[tokio::test]
async fn properties_use_requested_byte_exact_paths_and_describe_links_without_following() {
    use std::os::unix::{ffi::OsStringExt, fs::symlink};
    let root = tempfile::tempdir().unwrap();
    let file = root
        .path()
        .join(std::ffi::OsString::from_vec(b"raw-\xff #.txt".to_vec()));
    std::fs::write(&file, "file content").unwrap();
    let link = root.path().join("link");
    symlink(root.path().join("missing-target"), &link).unwrap();
    let (service, receiver) = service();
    service
        .show_item_properties(
            vec![
                url::Url::from_file_path(&file).unwrap().into(),
                url::Url::from_file_path(&link).unwrap().into(),
            ],
            String::new(),
        )
        .await
        .unwrap();
    let Message::Properties { paths, result, .. } = receiver.try_recv().unwrap() else {
        panic!("Expected a properties response");
    };
    assert_eq!(paths, vec![file.clone(), link.clone()]);
    let properties = result.unwrap();
    assert_eq!(properties[0].path, file);
    assert_eq!(properties[0].kind, "File");
    assert_eq!(properties[0].size, 12);
    assert_eq!(properties[1].path, link);
    assert_eq!(properties[1].kind, "Symbolic link");
    assert_eq!(
        properties[1].link_target.as_deref(),
        Some(root.path().join("missing-target").as_path())
    );
    assert!(properties[0].modified.is_some());
}

#[tokio::test]
async fn dbus_methods_reject_ambiguous_uris_and_enforce_request_limits() {
    let (service, receiver) = service();
    for uri in [
        "https://example.com/file",
        "file://remote/tmp/file",
        "file:///tmp/file?query",
        "file:///tmp/file#fragment",
        "file:///tmp/%00",
        "/tmp/plain-path",
    ] {
        let result = service
            .show_item_properties(vec![uri.into()], String::new())
            .await;
        assert!(
            matches!(result, Err(zbus::fdo::Error::InvalidArgs(_))),
            "Accepted {uri}"
        );
        assert!(service
            .show_items(vec![uri.into()], String::new())
            .await
            .is_err());
        assert!(service
            .show_folders(vec![uri.into()], String::new())
            .await
            .is_err());
    }
    assert!(matches!(
        service.show_item_properties(vec![], String::new()).await,
        Err(zbus::fdo::Error::InvalidArgs(_))
    ));
    assert!(matches!(
        service
            .show_item_properties(vec!["file:///tmp/file".into(); 13], String::new())
            .await,
        Err(zbus::fdo::Error::LimitsExceeded(_))
    ));
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn missing_properties_targets_return_an_explicit_ui_error() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing");
    let (service, receiver) = service();
    service
        .show_item_properties(
            vec![url::Url::from_file_path(&missing).unwrap().into()],
            String::new(),
        )
        .await
        .unwrap();
    let Message::Properties { paths, result, .. } = receiver.try_recv().unwrap() else {
        panic!("Expected a properties response");
    };
    assert_eq!(paths, vec![missing.clone()]);
    assert!(result
        .err()
        .unwrap()
        .contains(&missing.to_string_lossy().to_string()));
}

#[tokio::test]
async fn properties_dialog_keeps_frozen_targets_when_selection_changes() {
    i_slint_backend_testing::init_no_event_loop();
    let root = tempfile::tempdir().unwrap();
    let chosen = root.path().join("chosen");
    let other = root.path().join("other");
    std::fs::write(&chosen, "chosen data").unwrap();
    std::fs::write(&other, "other data").unwrap();
    let window = FilesWindow::new().unwrap();
    let (sender, receiver) = mpsc::channel();
    let service_sender = sender.clone();
    let mut app = App::new(
        &window,
        vec![Tab::new(1, root.path().into(), false, SortBy::Name)],
        Preferences::default(),
        ThemeSettings::default(),
        sender,
        Arc::new(AtomicBool::new(false)),
    );
    let service = FileManager {
        sender: service_sender,
        properties_metadata_request: app.properties_metadata_request.clone(),
    };
    service
        .show_item_properties(
            vec![url::Url::from_file_path(&chosen).unwrap().into()],
            String::new(),
        )
        .await
        .unwrap();
    app.tabs[0].selected.insert(other.clone());
    app.message(receiver.try_recv().unwrap());
    assert_eq!(app.dialog_kind, 10);
    assert_eq!(app.dialog_paths, vec![chosen.clone()]);
    assert_eq!(
        window.global::<PropertiesView>().get_location().as_str(),
        chosen.to_string_lossy()
    );
    assert_eq!(
        window.global::<PropertiesView>().get_name().as_str(),
        "chosen"
    );
    assert!(!window
        .global::<PropertiesView>()
        .get_location()
        .contains(other.to_string_lossy().as_ref()));
}

#[tokio::test]
async fn trailing_slash_properties_describe_the_link_itself() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    std::fs::create_dir(&target).unwrap();
    let link = root.path().join("link");
    symlink(&target, &link).unwrap();
    let (service, receiver) = service();
    let uri = format!("{}/", url::Url::from_file_path(&link).unwrap());
    service
        .show_item_properties(vec![uri], String::new())
        .await
        .unwrap();
    let Message::Properties { paths, result, .. } = receiver.try_recv().unwrap() else {
        panic!("Expected a properties response");
    };
    assert_eq!(paths, vec![link]);
    let properties = result.unwrap();
    assert_eq!(properties[0].kind, "Symbolic link");
    assert_eq!(properties[0].link_target.as_deref(), Some(target.as_path()));
}

#[test]
fn property_scan_counts_nested_files_without_following_links_or_duplicate_roots() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("folder");
    std::fs::create_dir(&folder).unwrap();
    let child = folder.join("child.txt");
    std::fs::write(&child, b"1234567").unwrap();
    std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o640)).unwrap();
    let outside = root.path().join("outside");
    std::fs::write(&outside, vec![0u8; 1000]).unwrap();
    let link = folder.join("external-link");
    symlink(&outside, &link).unwrap();
    let loop_link = folder.join("recursive-link");
    symlink(&folder, &loop_link).unwrap();
    let fifo = folder.join("pipe");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let items = describe_properties(&[folder.clone(), child.clone(), child.clone()]).unwrap();
    let summary = properties::scan(&items, &AtomicBool::new(false), 100, Duration::from_secs(1));
    assert!(summary.complete);
    assert!(!summary.unreadable);
    assert_eq!(summary.folders, 1);
    assert_eq!(summary.files, 4);
    assert_eq!(
        summary.bytes,
        7 + std::fs::symlink_metadata(link).unwrap().len()
            + std::fs::symlink_metadata(loop_link).unwrap().len()
    );
    assert_eq!(items[1].permissions, 0o640);
}

#[test]
fn property_scan_reports_partial_bounds_and_cancellation() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("first"), b"123").unwrap();
    std::fs::write(root.path().join("second"), b"123456").unwrap();
    let items = describe_properties(&[root.path().to_owned()]).unwrap();
    let summary = properties::scan(&items, &AtomicBool::new(false), 1, Duration::from_secs(1));
    assert!(!summary.complete);
    assert_eq!(summary.files, 0);
    let cancelled = properties::scan(&items, &AtomicBool::new(true), 100, Duration::from_secs(1));
    assert!(!cancelled.complete);
    assert_eq!(cancelled.folders, 0);
    let expired = properties::scan(&items, &AtomicBool::new(false), 100, Duration::ZERO);
    assert!(!expired.complete);
    assert_eq!(expired.files, 0);
}

#[tokio::test]
async fn properties_model_aggregates_mixed_selection_and_ignores_results_after_close() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first.txt");
    let second = root.path().join("second.txt");
    std::fs::write(&first, b"123").unwrap();
    std::fs::write(&second, b"1234567").unwrap();
    let (window, mut app, _) = tests::app(root.path().into());
    let mut items = describe_properties(&[first.clone(), second.clone()]).unwrap();
    items[1].permissions = 0o000;
    app.show_properties(vec![first, second], items);
    assert_eq!(app.dialog_kind, 10);
    let ui = window.global::<PropertiesView>();
    assert!(ui.get_multiple());
    assert_eq!(ui.get_name().as_str(), "2 selected items");
    assert_eq!(ui.get_location().as_str(), root.path().to_string_lossy());
    assert_eq!(ui.get_permissions().row_count(), 0);
    assert!(ui.get_permission_note().contains("vary"));
    let summary = properties::Summary {
        bytes: 10,
        allocated: 4096,
        files: 2,
        complete: true,
        ..Default::default()
    };
    let request = app.properties_request;
    app.message(Message::PropertiesSummary {
        request,
        summary: summary.clone(),
    });
    assert!(ui
        .get_details()
        .iter()
        .any(|row| row.label == "Size" && row.value.contains("10 bytes")));
    app.dismiss();
    app.message(Message::PropertiesSummary { request, summary });
    assert_eq!(app.dialog_kind, 0);
    assert!(app.properties_summary.is_none());
    assert!(app.properties_items.is_empty());
}

#[tokio::test]
async fn properties_link_permissions_describe_the_link_and_never_claim_target_access() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let link = root.path().join("broken");
    symlink("missing-target", &link).unwrap();
    let (window, mut app, _) = tests::app(root.path().into());
    app.show_properties(vec![link.clone()], describe_properties(&[link]).unwrap());
    let ui = window.global::<PropertiesView>();
    assert_eq!(ui.get_link_target().as_str(), "missing-target");
    assert_eq!(ui.get_permissions().row_count(), 0);
    assert!(ui.get_permission_note().contains("target"));
    assert_eq!(ui.get_description().as_str(), "Symbolic link");
}

#[tokio::test]
async fn local_properties_keeps_all_selected_items_beyond_external_request_limit() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..24 {
        std::fs::write(root.path().join(format!("item-{index:02}.txt")), b"123").unwrap();
    }
    let (window, mut app, receiver) = tests::app(root.path().into());
    app.tabs[0].entries = fs::read_directory(
        root.path(),
        false,
        SortBy::Name,
        false,
        &AtomicBool::new(false),
    )
    .unwrap();
    app.action("select-all");
    app.action("properties");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(message) = receiver.try_recv() {
            app.message(message);
        }
        if app.dialog_kind == 10 {
            break;
        }
        assert!(Instant::now() < deadline, "Local properties did not finish");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(app.properties_items.len(), 24);
    assert_eq!(app.dialog_paths.len(), 24);
    assert_eq!(
        window.global::<PropertiesView>().get_name(),
        "24 selected items"
    );
    app.dismiss();
}

#[tokio::test]
async fn replaced_properties_cancel_old_count_and_reject_its_late_result() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first.txt");
    let second = root.path().join("second.txt");
    std::fs::write(&first, b"1").unwrap();
    std::fs::write(&second, b"22").unwrap();
    let (window, mut app, _) = tests::app(root.path().into());
    app.show_properties(vec![first.clone()], describe_properties(&[first]).unwrap());
    let old_request = app.properties_request;
    let old_cancel = app.properties_scan.as_ref().unwrap().clone();
    app.show_properties(
        vec![second.clone()],
        describe_properties(&[second]).unwrap(),
    );
    assert!(old_cancel.load(Ordering::Relaxed));
    app.message(Message::PropertiesSummary {
        request: old_request,
        summary: properties::Summary {
            bytes: 9999,
            complete: true,
            ..Default::default()
        },
    });
    assert!(app.properties_summary.is_none());
    assert_eq!(window.global::<PropertiesView>().get_name(), "second.txt");
    assert!(window
        .global::<PropertiesView>()
        .get_details()
        .iter()
        .any(|row| {
            row.label == "Size" && row.value.contains("2 bytes") && !row.value.contains("9999")
        }));
    app.dismiss();
}

#[tokio::test]
async fn stale_property_metadata_cannot_replace_new_modal_or_reopen_after_dismissal() {
    let root = tempfile::tempdir().unwrap();
    let chosen = root.path().join("chosen.txt");
    std::fs::write(&chosen, b"data").unwrap();
    let (window, mut app, _) = tests::app(root.path().into());
    let metadata = describe_properties(std::slice::from_ref(&chosen)).unwrap();
    let old_request = app
        .properties_metadata_request
        .fetch_add(1, Ordering::SeqCst)
        + 1;
    app.dialog(1, "New folder", String::new(), vec![], "New folder".into());
    app.message(Message::Properties {
        request: old_request,
        paths: vec![chosen.clone()],
        result: Ok(metadata.clone()),
    });
    assert_eq!(app.dialog_kind, 1);
    assert_eq!(window.get_dialog_title(), "New folder");
    app.message(Message::Properties {
        request: old_request,
        paths: vec![chosen.clone()],
        result: Err("Old filesystem failure".into()),
    });
    assert_eq!(
        app.dialog_kind, 1,
        "A stale error must not replace a newer modal"
    );

    let pending_request = app
        .properties_metadata_request
        .fetch_add(1, Ordering::SeqCst)
        + 1;
    app.dismiss();
    app.message(Message::Properties {
        request: pending_request,
        paths: vec![chosen],
        result: Ok(metadata),
    });
    assert_eq!(app.dialog_kind, 0);
    assert!(app.properties_items.is_empty());
}

#[tokio::test]
async fn property_metadata_survives_refresh_but_not_navigation_or_a_newer_request() {
    let root = tempfile::tempdir().unwrap();
    let chosen = root.path().join("chosen.txt");
    std::fs::write(&chosen, b"data").unwrap();
    let (window, mut app, _) = tests::app(root.path().into());
    let metadata = describe_properties(std::slice::from_ref(&chosen)).unwrap();
    let request = app
        .properties_metadata_request
        .fetch_add(1, Ordering::SeqCst)
        + 1;
    app.load();
    app.message(Message::Properties {
        request,
        paths: vec![chosen.clone()],
        result: Ok(metadata.clone()),
    });
    assert_eq!(
        app.dialog_kind, 10,
        "An ordinary refresh must preserve the request"
    );
    assert_eq!(window.global::<PropertiesView>().get_name(), "chosen.txt");
    app.dismiss();

    let older = app
        .properties_metadata_request
        .fetch_add(1, Ordering::SeqCst)
        + 1;
    let newer = app
        .properties_metadata_request
        .fetch_add(1, Ordering::SeqCst)
        + 1;
    app.message(Message::Properties {
        request: older,
        paths: vec![chosen.clone()],
        result: Ok(metadata.clone()),
    });
    assert_eq!(app.dialog_kind, 0);
    app.navigate(root.path().to_owned());
    app.message(Message::Properties {
        request: newer,
        paths: vec![chosen],
        result: Ok(metadata),
    });
    assert_eq!(
        app.dialog_kind, 0,
        "Navigation must invalidate even the newer request"
    );
}

#[tokio::test]
async fn external_properties_share_ui_cancellation_epoch() {
    let root = tempfile::tempdir().unwrap();
    let chosen = root.path().join("chosen.txt");
    std::fs::write(&chosen, b"data").unwrap();
    let (_window, mut app, receiver) = tests::app(root.path().into());
    let service = FileManager {
        sender: app.sender.clone(),
        properties_metadata_request: app.properties_metadata_request.clone(),
    };
    service
        .show_item_properties(
            vec![url::Url::from_file_path(&chosen).unwrap().into()],
            String::new(),
        )
        .await
        .unwrap();

    app.key(
        &char::from(slint::platform::Key::Escape).to_string(),
        false,
        false,
        false,
    );
    app.message(receiver.try_recv().unwrap());
    assert_eq!(app.dialog_kind, 0);
    assert!(app.properties_items.is_empty());
}
