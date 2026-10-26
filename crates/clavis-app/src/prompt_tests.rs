use super::*;
use std::{
    io::{BufRead, BufReader, Read},
    process::{Child, Command, Stdio},
};

struct PrivateBus {
    child: Child,
    _directory: tempfile::TempDir,
    address: String,
}
impl PrivateBus {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--nopidfile", "--print-address=1"])
            .arg(format!(
                "--address=unix:path={}",
                directory.path().join("bus").display()
            ))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        Self {
            child,
            _directory: directory,
            address: address.trim().to_owned(),
        }
    }
}
impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
async fn on_ui<T: Send + 'static>(
    weak: &slint::Weak<PasswordWindow>,
    action: impl FnOnce(&PasswordWindow) -> T + Send + 'static,
) -> T {
    let (send, receive) = tokio::sync::oneshot::channel();
    weak.upgrade_in_event_loop(move |ui| {
        let _ = send.send(action(&ui));
    })
    .unwrap();
    tokio::time::timeout(Duration::from_secs(2), receive)
        .await
        .unwrap()
        .unwrap()
}
async fn shown(weak: &slint::Weak<PasswordWindow>, label: &'static str) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if on_ui(weak, move |ui| {
                ui.window().is_visible() && ui.get_heading().contains(label)
            })
            .await
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
async fn call_prompt_at(
    connection: zbus::Connection,
    path: &'static str,
    label: &'static str,
    role: &'static str,
) -> zbus::Result<(zbus::zvariant::OwnedFd, bool)> {
    let message = connection
        .call_method(
            Some("org.freedesktop.secrets.CliPrompter"),
            path,
            Some("org.clavis.Prompter"),
            "Prompt",
            &(role, label, "Test request"),
        )
        .await?;
    message.body().deserialize()
}
async fn call_prompt(
    connection: zbus::Connection,
    label: &'static str,
    role: &'static str,
) -> zbus::Result<(zbus::zvariant::OwnedFd, bool)> {
    call_prompt_at(connection, "/org/clavis/Prompter", label, role).await
}
async fn cancel(connection: &zbus::Connection) {
    connection
        .call_method(
            Some("org.freedesktop.secrets.CliPrompter"),
            "/org/clavis/Prompter",
            Some("org.clavis.Prompter"),
            "Cancel",
            &(),
        )
        .await
        .unwrap();
}
async fn response(
    task: tokio::task::JoinHandle<zbus::Result<(zbus::zvariant::OwnedFd, bool)>>,
) -> (zbus::zvariant::OwnedFd, bool) {
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

#[test]
fn private_bus_prompts_require_owner_confirm_password_and_dismiss_stale_requests() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let ui = PasswordWindow::new().unwrap();
    let broker = Broker::with_trusted(&ui, vec![std::env::current_exe().unwrap()]);
    let untrusted = Broker::with_trusted(&ui, Vec::new());
    let _tick = install_ui_callbacks(&ui, &broker);
    let bus = PrivateBus::new();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let service = runtime.block_on(async {
        zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name("org.freedesktop.secrets.CliPrompter")
            .unwrap()
            .serve_at("/org/clavis/Prompter", ClavisPrompter(broker.clone()))
            .unwrap()
            .serve_at("/org/clavis/UntrustedPrompter", ClavisPrompter(untrusted))
            .unwrap()
            .build()
            .await
            .unwrap()
    });
    let _watch = watch_owner(&runtime, broker.clone(), service.clone());
    let address = bus.address.clone();
    let weak = ui.as_weak();
    let job = runtime.spawn(async move {
        let owner = zbus::connection::Builder::address(address.as_str()).unwrap().name("org.freedesktop.secrets").unwrap().build().await.unwrap();
        let outsider = zbus::connection::Builder::address(address.as_str()).unwrap().build().await.unwrap();
        assert!(matches!(call_prompt(outsider, "Forbidden", "unlock").await, Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == "org.freedesktop.DBus.Error.AccessDenied"));
        assert!(!on_ui(&weak, |ui| ui.window().is_visible()).await);

        assert!(matches!(
            call_prompt_at(owner.clone(), "/org/clavis/UntrustedPrompter", "Impostor", "unlock").await,
            Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == "org.freedesktop.DBus.Error.AccessDenied"
        ));
        assert!(!on_ui(&weak, |ui| ui.window().is_visible()).await);

        let request = tokio::spawn(call_prompt(owner.clone(), "Confirm", "create"));
        shown(&weak, "Confirm").await;
        assert!(on_ui(&weak, |ui| {
            assert!(ui.get_confirm_required()); ui.set_password("dummy-master".into());
            ui.set_confirmation("wrong-dummy".into()); ui.invoke_submit(); ui.window().is_visible()
        }).await);
        assert!(!request.is_finished());
        on_ui(&weak, |ui| { ui.set_confirmation("dummy-master".into()); ui.invoke_submit(); }).await;
        let (fd, accepted) = response(request).await; assert!(accepted);
        let mut stream = std::os::unix::net::UnixStream::from(OwnedFd::from(fd));
        stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut value = Zeroizing::new(Vec::new()); stream.read_to_end(&mut value).unwrap();
        assert!(value.as_slice() == b"dummy-master");
        assert!(on_ui(&weak, |ui| !ui.window().is_visible() && ui.get_password().is_empty() && ui.get_confirmation().is_empty()).await);

        let first = tokio::spawn(call_prompt(owner.clone(), "First", "unlock")); shown(&weak, "First").await;
        let queued = tokio::spawn(call_prompt(owner.clone(), "Queued", "unlock"));
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel(&owner).await;
        assert!(!response(first).await.1); assert!(!response(queued).await.1);
        assert!(on_ui(&weak, |ui| !ui.window().is_visible() && ui.get_password().is_empty()).await);

        let old = tokio::spawn(call_prompt(owner.clone(), "Old owner", "unlock")); shown(&weak, "Old owner").await;
        owner.release_name("org.freedesktop.secrets").await.unwrap();
        assert!(!response(old).await.1);
        assert!(call_prompt(owner.clone(), "Revoked", "unlock").await.is_err());
        let replacement = zbus::connection::Builder::address(address.as_str()).unwrap().name("org.freedesktop.secrets").unwrap().build().await.unwrap();
        let new = tokio::spawn(call_prompt(replacement.clone(), "New owner", "unlock")); shown(&weak, "New owner").await;

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(on_ui(&weak, |ui| ui.window().is_visible() && ui.get_heading().contains("New owner")).await);
        on_ui(&weak, |ui| ui.invoke_cancel()).await;
        assert!(!response(new).await.1);
        assert!(on_ui(&weak, |ui| !ui.window().is_visible() && ui.get_password().is_empty() && ui.get_confirmation().is_empty()).await);
        slint::quit_event_loop().unwrap();
    });
    let watchdog = slint::Timer::default();
    watchdog.start(
        slint::TimerMode::SingleShot,
        Duration::from_secs(10),
        || {
            let _ = slint::quit_event_loop();
        },
    );
    slint::run_event_loop_until_quit().unwrap();
    runtime.block_on(job).unwrap();
    drop(service);
    drop(runtime);
    drop(bus);
}

#[test]
fn only_protected_trusted_executables_may_request_passwords() {
    use std::os::unix::fs::PermissionsExt;
    let directory = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned();
    let fixture = directory.join(format!("clavis-daemon-fixture-{}", std::process::id()));
    for (mode, expected) in [
        (0o755, true),
        (0o775, false),
        (0o777, false),
        (0o4755, false),
    ] {
        std::fs::write(&fixture, b"fixture").unwrap();
        std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(protected_executable(&fixture), expected, "mode {mode:o}");
        std::fs::remove_file(&fixture).unwrap();
    }
    std::fs::write(&fixture, b"fixture").unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o755)).unwrap();
    let link = directory.join(format!("clavis-daemon-link-{}", std::process::id()));
    std::os::unix::fs::symlink(&fixture, &link).unwrap();
    assert!(!protected_executable(&link));
    std::fs::remove_file(&link).unwrap();
    std::fs::remove_file(&fixture).unwrap();

    let temporary = tempfile::tempdir().unwrap();
    let binary = temporary.path().join("clavis-daemon");
    std::fs::write(&binary, b"fixture").unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!protected_executable(&binary));
    assert!(!protected_executable(Path::new("relative-daemon")));
}

#[test]
fn development_paths_are_explicitly_opt_in() {
    let directory = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned();
    let checked_out = directory.join("clavis-daemon");

    let release = trusted_daemon_paths(false);
    assert!(release.contains(&PathBuf::from("/usr/libexec/rixde/clavis-daemon")));
    assert!(release.contains(&PathBuf::from("/usr/local/libexec/rixde/clavis-daemon")));
    assert!(!release.contains(&checked_out));
    assert!(!release
        .iter()
        .any(|path| path.starts_with(directory.parent().unwrap())));

    let development = trusted_daemon_paths(true);
    assert!(development.contains(&checked_out));
    assert!(development.contains(&PathBuf::from("/usr/libexec/rixde/clavis-daemon")));

    assert!(!development_paths(false, None));
    assert!(!development_paths(false, Some(std::ffi::OsStr::new("0"))));
    assert!(development_paths(false, Some(std::ffi::OsStr::new("1"))));
    assert!(development_paths(true, None));
}
