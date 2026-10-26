use std::{ffi::OsString, os::unix::ffi::OsStringExt, path::PathBuf};

#[test]
#[ignore = "requires an isolated Wayland compositor with ext-data-control"]
fn file_clipboard_roundtrips_through_separate_native_connections() {
    assert_eq!(
        std::env::var("RIX_FILES_ISOLATED_WAYLAND").as_deref(),
        Ok("1"),
        "Only run this test in a disposable compositor session"
    );
    let paths = vec![
        PathBuf::from("/tmp/rix clipboard $() #?%\nfile"),
        PathBuf::from(OsString::from_vec(b"/tmp/rix-clipboard-raw-\xff".to_vec())),
    ];
    for cut in [false, true] {
        rix_files::clipboard::publish(&paths, cut).unwrap();
        assert_eq!(rix_files::clipboard::read().unwrap(), (paths.clone(), cut));
    }

    use wl_clipboard_rs::copy::{MimeType, Options, Source};
    let (_, uris) = rix_files::clipboard::encode(&paths, false).unwrap();
    let mut options = Options::new();
    options.foreground(true);
    let prepared = options
        .prepare_copy(
            Source::Bytes(uris.into_boxed_slice()),
            MimeType::Specific("text/uri-list".into()),
        )
        .unwrap();
    let server = std::thread::spawn(move || prepared.serve());
    assert_eq!(
        rix_files::clipboard::read().unwrap(),
        (paths.clone(), false)
    );

    rix_files::clipboard::publish(&paths, false).unwrap();
    server.join().unwrap().unwrap();
}
