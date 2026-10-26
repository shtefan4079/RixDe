use rix_portal::{
    backend::{FileChooser, Options, BUS_NAME, OBJECT_PATH},
    picker::{
        path_to_uri, Choice, Filter, PickerMode, PickerRequest, PickerResponse, PROTOCOL_VERSION,
    },
};
use std::{
    io::{BufRead, BufReader},
    os::unix::{ffi::OsStringExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
use zbus::{
    zvariant::{OwnedObjectPath, OwnedValue, Value},
    Connection, Proxy,
};

struct Fixture {
    directory: tempfile::TempDir,
    bus: Child,
    server: Connection,
    frontend: Connection,
    attacker: Connection,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.bus.kill();
        let _ = self.bus.wait();
    }
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let picker = directory.path().join("picker");
        std::fs::write(
            &picker,
            r#"#!/usr/bin/python3 -I
import json, os, pathlib, sys, time
assert sys.argv[1:] == ['--portal-picker']
root = pathlib.Path(__file__).parent
request = json.load(sys.stdin)
(root / 'request.json').write_text(json.dumps(request))
(root / 'pid').write_text(str(os.getpid()))
if request['title'] == 'wait':
    while True: time.sleep(0.1)
if request['title'] == 'oversized':
    sys.stdout.write('x' * (1024 * 1024 + 1))
    sys.stdout.flush()
    while True: time.sleep(0.1)
sys.stdout.write((root / 'response.json').read_text())
"#,
        )
        .unwrap();
        std::fs::set_permissions(&picker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut bus = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut address = String::new();
        BufReader::new(bus.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let server = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .name(BUS_NAME)
            .unwrap()
            .serve_at(OBJECT_PATH, FileChooser::new(picker))
            .unwrap()
            .build()
            .await
            .unwrap();
        let frontend = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .name("org.freedesktop.portal.Desktop")
            .unwrap()
            .build()
            .await
            .unwrap();
        let attacker = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .build()
            .await
            .unwrap();
        Self {
            directory,
            bus,
            server,
            frontend,
            attacker,
        }
    }
    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }
    fn response(&self, response: &PickerResponse) {
        std::fs::write(
            self.path("response.json"),
            serde_json::to_vec(response).unwrap(),
        )
        .unwrap();
    }
    async fn proxy<'a>(&self, connection: &'a Connection) -> Proxy<'a> {
        Proxy::new(
            connection,
            BUS_NAME,
            OBJECT_PATH,
            "org.freedesktop.impl.portal.FileChooser",
        )
        .await
        .unwrap()
    }
    async fn child_pid(&self) -> i32 {
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(self.path("pid")) {
                    break pid.parse().unwrap();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }
    fn assert_child_stopped(&self, pid: i32) {
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "Picker process is still running"
        );
    }
}

fn handle(name: &str) -> OwnedObjectPath {
    format!("/org/freedesktop/portal/desktop/request/fixture/{name}")
        .try_into()
        .unwrap()
}
fn value<T: Into<Value<'static>>>(value: T) -> OwnedValue {
    value.into().try_to_owned().unwrap()
}
fn path_option(path: &Path) -> OwnedValue {
    use std::os::unix::ffi::OsStrExt;
    let mut bytes = path.as_os_str().as_bytes().to_vec();
    bytes.push(0);
    value(bytes)
}
fn accepted(paths: &[PathBuf]) -> PickerResponse {
    PickerResponse {
        response: 0,
        uris: paths.iter().map(|p| path_to_uri(p).unwrap()).collect(),
        current_filter: None,
        choices: vec![],
    }
}

#[tokio::test]
async fn standard_open_save_and_savefiles_preserve_paths_and_controls() {
    let fixture = Fixture::new().await;
    let proxy = fixture.proxy(&fixture.frontend).await;
    assert_eq!(proxy.get_property::<u32>("version").await.unwrap(), 4);
    let file = fixture.directory.path().join(std::ffi::OsString::from_vec(
        b"document \xff #?.txt".to_vec(),
    ));
    std::fs::write(&file, b"fixture").unwrap();
    let filter = Filter {
        name: "Text".into(),
        rules: vec![(0, "*.txt".into()), (1, "text/*".into())],
    };
    let choice = Choice {
        id: "encoding".into(),
        label: "Encoding".into(),
        options: vec![("utf8".into(), "Unicode".into())],
        selected: "utf8".into(),
    };
    let mut response = accepted(std::slice::from_ref(&file));
    response.current_filter = Some(filter.clone());
    response.choices = vec![
        ("encoding".into(), "utf8".into()),
        ("readonly".into(), "true".into()),
    ];
    fixture.response(&response);
    let options = Options::from([
        ("multiple".into(), value(true)),
        ("modal".into(), value(false)),
        (
            "current_folder".into(),
            path_option(fixture.directory.path()),
        ),
        (
            "filters".into(),
            value(vec![(filter.name.clone(), filter.rules.clone())]),
        ),
        (
            "current_filter".into(),
            value((filter.name.clone(), filter.rules.clone())),
        ),
        (
            "choices".into(),
            value(vec![
                (
                    choice.id.clone(),
                    choice.label.clone(),
                    choice.options.clone(),
                    choice.selected.clone(),
                ),
                ("readonly".into(), "Read only".into(), vec![], "true".into()),
            ]),
        ),
    ]);
    let (code, results): (u32, Options) = proxy
        .call(
            "OpenFile",
            &(
                handle("open"),
                "org.example.Editor",
                "wayland:fixture-parent",
                "Choose file",
                options,
            ),
        )
        .await
        .unwrap();
    assert_eq!(code, 0);
    assert_eq!(
        Vec::<String>::try_from(results["uris"].try_clone().unwrap()).unwrap(),
        response.uris
    );
    let request: PickerRequest =
        serde_json::from_slice(&std::fs::read(fixture.path("request.json")).unwrap()).unwrap();
    assert_eq!(request.mode, PickerMode::Open);
    assert!(!request.modal);
    assert!(request.multiple);
    assert_eq!(
        request.current_folder.unwrap(),
        fixture.directory.path().as_os_str().as_encoded_bytes()
    );
    assert_eq!(request.choices[0], choice);
    let save = fixture.path("new document.pdf");
    fixture.response(&accepted(std::slice::from_ref(&save)));
    let (code, _): (u32, Options) = proxy
        .call(
            "SaveFile",
            &(
                handle("save"),
                "org.example.Editor",
                "",
                "Save document",
                Options::from([
                    ("current_name".into(), value("new document.pdf".to_owned())),
                    (
                        "current_folder".into(),
                        path_option(fixture.directory.path()),
                    ),
                ]),
            ),
        )
        .await
        .unwrap();
    assert_eq!(code, 0);
    assert!(
        !save.exists(),
        "The chooser must not create or overwrite the saved file"
    );
    let request: PickerRequest =
        serde_json::from_slice(&std::fs::read(fixture.path("request.json")).unwrap()).unwrap();
    assert_eq!(request.mode, PickerMode::Save);
    assert_eq!(request.current_name, "new document.pdf");
    let names = vec![b"one.txt\0".to_vec(), b"two\xff.txt\0".to_vec()];
    let paths = names
        .iter()
        .map(|name| {
            fixture.directory.path().join(std::ffi::OsString::from_vec(
                name[..name.len() - 1].to_vec(),
            ))
        })
        .collect::<Vec<_>>();
    fixture.response(&accepted(&paths));
    let (code, results): (u32, Options) = proxy
        .call(
            "SaveFiles",
            &(
                handle("savefiles"),
                "org.example.Editor",
                "",
                "Save files",
                Options::from([
                    ("files".into(), value(names)),
                    (
                        "current_folder".into(),
                        path_option(fixture.directory.path()),
                    ),
                ]),
            ),
        )
        .await
        .unwrap();
    assert_eq!(code, 0);
    assert_eq!(
        Vec::<String>::try_from(results["uris"].try_clone().unwrap())
            .unwrap()
            .len(),
        2
    );
    let request: PickerRequest =
        serde_json::from_slice(&std::fs::read(fixture.path("request.json")).unwrap()).unwrap();
    assert_eq!(request.mode, PickerMode::SaveFiles);
    assert!(request.directory);
    assert_eq!(request.files[1], b"two\xff.txt");
    assert!(paths.iter().all(|p| !p.exists()));
}

#[tokio::test]
async fn untrusted_sender_and_malformed_options_never_start_picker() {
    let fixture = Fixture::new().await;
    let proxy = fixture.proxy(&fixture.attacker).await;
    let result: zbus::Result<(u32, Options)> = proxy
        .call(
            "OpenFile",
            &(
                handle("untrusted"),
                "org.example.Victim",
                "",
                "Choose",
                Options::new(),
            ),
        )
        .await;
    assert!(
        matches!(result, Err(zbus::Error::MethodError(name, _, _)) if name.as_str().ends_with("AccessDenied"))
    );
    let proxy = fixture.proxy(&fixture.frontend).await;
    for options in [
        Options::from([("multiple".into(), value("yes".to_owned()))]),
        Options::from([(
            "current_folder".into(),
            value(b"/tmp/not-terminated".to_vec()),
        )]),
        Options::from([("current_name".into(), value("../outside".to_owned()))]),
    ] {
        let result: zbus::Result<(u32, Options)> = proxy
            .call(
                "OpenFile",
                &(
                    handle("invalid"),
                    "org.example.Editor",
                    "",
                    "Choose",
                    options,
                ),
            )
            .await;
        assert!(
            matches!(result, Err(zbus::Error::MethodError(name, _, _)) if name.as_str().ends_with("InvalidArgs"))
        );
    }
    assert!(!fixture.path("pid").exists());
}

async fn start_wait(
    fixture: &Fixture,
    path: OwnedObjectPath,
) -> tokio::task::JoinHandle<(u32, Options)> {
    let frontend = fixture.frontend.clone();
    tokio::spawn(async move {
        let proxy = Proxy::new(
            &frontend,
            BUS_NAME,
            OBJECT_PATH,
            "org.freedesktop.impl.portal.FileChooser",
        )
        .await
        .unwrap();
        proxy
            .call(
                "OpenFile",
                &(path, "org.example.Editor", "", "wait", Options::new()),
            )
            .await
            .unwrap()
    })
}

#[tokio::test]
async fn close_only_from_owner_cancels_and_reaps_its_picker() {
    let fixture = Fixture::new().await;
    let path = handle("close");
    let operation = start_wait(&fixture, path.clone()).await;
    let pid = fixture.child_pid().await;
    let attacker = Proxy::new(
        &fixture.attacker,
        BUS_NAME,
        path.clone(),
        "org.freedesktop.impl.portal.Request",
    )
    .await
    .unwrap();
    assert!(attacker.call::<_, _, ()>("Close", &()).await.is_err());
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
    let duplicate = fixture
        .proxy(&fixture.frontend)
        .await
        .call::<_, _, (u32, Options)>(
            "OpenFile",
            &(
                path.clone(),
                "org.example.Editor",
                "",
                "wait",
                Options::new(),
            ),
        )
        .await;
    assert!(
        matches!(duplicate, Err(zbus::Error::MethodError(name, _, _)) if name.as_str().ends_with("InvalidArgs"))
    );
    let frontend = Proxy::new(
        &fixture.frontend,
        BUS_NAME,
        path.clone(),
        "org.freedesktop.impl.portal.Request",
    )
    .await
    .unwrap();
    tokio::time::timeout(
        Duration::from_secs(4),
        frontend.call::<_, _, ()>("Close", &()),
    )
    .await
    .unwrap()
    .unwrap();
    fixture.assert_child_stopped(pid);
    let (response, results) = tokio::time::timeout(Duration::from_secs(4), operation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response, 1);
    assert!(results.is_empty());
    let introspect = Proxy::new(
        &fixture.frontend,
        BUS_NAME,
        path,
        "org.freedesktop.DBus.Introspectable",
    )
    .await
    .unwrap()
    .call::<_, _, String>("Introspect", &())
    .await;
    assert!(
        matches!(introspect, Err(zbus::Error::MethodError(name, _, _)) if name.as_str().ends_with("UnknownObject"))
    );
    let _ = &fixture.server;
}

#[tokio::test]
async fn disappearing_frontend_cancels_instead_of_leaving_an_orphan() {
    let fixture = Fixture::new().await;
    let operation = start_wait(&fixture, handle("disappear")).await;
    let pid = fixture.child_pid().await;
    fixture
        .frontend
        .release_name("org.freedesktop.portal.Desktop")
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(4), operation)
            .await
            .unwrap()
            .unwrap()
            .0,
        1
    );
    fixture.assert_child_stopped(pid);
}

#[tokio::test]
async fn oversized_output_is_stopped_and_does_not_return_partial_selection() {
    let fixture = Fixture::new().await;
    let proxy = fixture.proxy(&fixture.frontend).await;
    let (response, results): (u32, Options) = tokio::time::timeout(
        Duration::from_secs(4),
        proxy.call(
            "OpenFile",
            &(
                handle("oversized"),
                "org.example.Editor",
                "",
                "oversized",
                Options::new(),
            ),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response, 2);
    assert!(results.is_empty());
    fixture.assert_child_stopped(fixture.child_pid().await);
}

#[test]
fn response_validation_rejects_remote_files_wrong_counts_and_unknown_choices() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("one.txt");
    std::fs::write(&file, b"one").unwrap();
    let request = PickerRequest {
        version: PROTOCOL_VERSION,
        mode: PickerMode::Open,
        app_id: "org.example.Editor".into(),
        title: "Open".into(),
        accept_label: "".into(),
        parent_window: "".into(),
        modal: true,
        multiple: false,
        directory: false,
        current_folder: None,
        current_file: None,
        current_name: "".into(),
        files: vec![],
        filters: vec![],
        current_filter: None,
        choices: vec![],
    };
    assert!(request
        .validate_response(&accepted(std::slice::from_ref(&file)))
        .is_ok());
    let mut response = accepted(std::slice::from_ref(&file));
    response.uris[0] = "file://remote-host/tmp/file".into();
    assert!(request.validate_response(&response).is_err());
    response = accepted(&[file.clone(), file.clone()]);
    assert!(request.validate_response(&response).is_err());
    response = accepted(&[file]);
    response.choices.push(("unknown".into(), "true".into()));
    assert!(request.validate_response(&response).is_err());
    assert_eq!(
        request
            .validate_response(&PickerResponse {
                response: 1,
                uris: vec!["https://ignore.invalid".into()],
                current_filter: None,
                choices: vec![]
            })
            .unwrap(),
        PickerResponse::cancelled()
    );
}
