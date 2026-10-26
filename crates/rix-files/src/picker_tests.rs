use super::*;
use rix_portal::picker::{Filter, PickerMode, PickerRequest};
fn request(folder: &Path, mode: PickerMode) -> PickerRequest {
    PickerRequest {
        version: 1,
        mode,
        app_id: "org.rix.test.Picker".into(),
        title: "Fixture".into(),
        accept_label: String::new(),
        parent_window: String::new(),
        modal: true,
        multiple: false,
        directory: false,
        current_folder: Some(folder.as_os_str().as_bytes().to_vec()),
        current_file: None,
        current_name: "new.txt".into(),
        files: vec![],
        filters: vec![],
        current_filter: None,
        choices: vec![],
    }
}
fn setup(folder: &Path, request: PickerRequest) -> (FilesWindow, App) {
    i_slint_backend_testing::init_no_event_loop();
    let window = FilesWindow::new().unwrap();
    let (sender, _) = mpsc::channel();
    let tab = Tab::new(1, folder.to_owned(), false, SortBy::Name);
    let mut app = App::new(
        &window,
        vec![tab],
        Preferences::default(),
        ThemeSettings::default(),
        sender,
        Arc::new(AtomicBool::new(false)),
    );
    app.tabs[0].loading = false;
    let picker = Picker::new(request);
    picker.install(&window);
    app.picker = Some(picker);
    (window, app)
}
#[test]
fn save_overwrite_requires_confirmation_and_never_writes_to_the_file() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("existing.txt");
    std::fs::write(&path, "preserve").unwrap();
    let mut req = request(root.path(), PickerMode::Save);
    req.current_name = "existing.txt".into();
    let (_window, mut app) = setup(root.path(), req);
    app.accept_picker(false);
    assert_eq!(app.dialog_kind, 8);
    assert!(app.picker.as_ref().unwrap().response.is_none());
    app.confirm(String::new());
    assert_eq!(
        app.picker
            .as_ref()
            .unwrap()
            .response
            .as_ref()
            .unwrap()
            .response,
        0
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "preserve");
}
#[test]
fn saving_unchanged_non_utf8_name_preserves_original_bytes() {
    let root = tempfile::tempdir().unwrap();
    let path = root
        .path()
        .join(OsString::from_vec(b"name-\xff.txt".to_vec()));
    let mut req = request(root.path(), PickerMode::Save);
    req.current_file = Some(path.as_os_str().as_bytes().to_vec());
    let (_window, mut app) = setup(root.path(), req);
    app.accept_picker(false);
    let response = app.picker.as_ref().unwrap().response.as_ref().unwrap();
    assert_eq!(
        rix_portal::picker::uri_to_path(&response.uris[0]).unwrap(),
        path
    );
}
#[test]
fn open_directory_defaults_to_current_folder_and_filters_keep_navigation() {
    let root = tempfile::tempdir().unwrap();
    let mut req = request(root.path(), PickerMode::Open);
    req.directory = true;
    let (_window, mut app) = setup(root.path(), req.clone());
    app.accept_picker(false);
    let response = app.picker.as_ref().unwrap().response.as_ref().unwrap();
    assert_eq!(
        rix_portal::picker::uri_to_path(&response.uris[0]).unwrap(),
        root.path()
    );
    let file = root.path().join("report.pdf");
    std::fs::write(&file, b"%PDF-1.4").unwrap();
    let entry = Entry {
        path: file,
        name: "report.pdf".into(),
        is_dir: false,
        is_symlink: false,
        size: 8,
        modified: None,
        kind: "Document".into(),
        icon_name: String::new(),
        icon_names: Vec::new(),
    };
    assert!(!Matcher::new(&req, None).matches(&entry));
    req.directory = false;
    let filter = Filter {
        name: "PDF".into(),
        rules: vec![(0, "*.pdf".into())],
    };
    assert!(Matcher::new(&req, Some(&filter)).matches(&entry));
    let filter = Filter {
        name: "Text".into(),
        rules: vec![(0, "*.txt".into())],
    };
    assert!(!Matcher::new(&req, Some(&filter)).matches(&entry));
}

#[test]
fn multiple_mime_rules_detect_once_and_directory_glob_matches_do_not_read_files() {
    let entry = Entry {
        path: "/does/not/exist.pdf".into(),
        name: "report.pdf".into(),
        is_dir: false,
        is_symlink: false,
        size: 0,
        modified: None,
        kind: String::new(),
        icon_name: String::new(),
        icon_names: Vec::new(),
    };
    let req = request(Path::new("/"), PickerMode::Open);
    let filter = Filter {
        name: "Media".into(),
        rules: vec![
            (1, "image/*".into()),
            (1, "text/plain".into()),
            (1, "application/pdf".into()),
        ],
    };
    let matcher = Matcher::new(&req, Some(&filter));
    let calls = std::cell::Cell::new(0);
    assert!(matcher.matches_using(&entry, || {
        calls.set(calls.get() + 1);
        Some("application/pdf".into())
    }));
    assert_eq!(calls.get(), 1);
    let filter = Filter {
        name: "PDF".into(),
        rules: vec![(0, "*.pdf".into()), (1, "application/pdf".into())],
    };
    assert!(Matcher::new(&req, Some(&filter))
        .matches_using(&entry, || panic!("glob matches without MIME I/O")));
    let mut directory = entry.clone();
    directory.is_dir = true;
    assert!(matcher.matches_using(&directory, || panic!("navigation folders never need MIME")));
}

#[test]
fn cancelled_picker_snapshot_stops_before_file_metadata_or_mime_reads() {
    let req = request(Path::new("/"), PickerMode::Open);
    let filter = Filter {
        name: "PDF".into(),
        rules: vec![(1, "application/pdf".into())],
    };
    let matcher = Matcher::new(&req, Some(&filter));
    let mut entries = vec![Entry {
        path: "/does/not/exist.pdf".into(),
        name: "report.pdf".into(),
        is_dir: false,
        is_symlink: false,
        size: 0,
        modified: None,
        kind: String::new(),
        icon_name: String::new(),
        icon_names: Vec::new(),
    }];
    let cancel = AtomicBool::new(true);
    assert!(!matcher.filter(&mut entries, &cancel));
    assert!(entries.is_empty());
}
