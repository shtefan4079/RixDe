mod reexec;

use anyhow::Context;
use gio::prelude::*;
use std::{
    collections::HashSet,
    path::Path,
    sync::{Mutex, OnceLock},
    time::Instant,
};

pub(crate) fn launch_desktop(path: &Path) -> anyhow::Result<()> {
    let app = gio::DesktopAppInfo::from_filename(path)
        .with_context(|| format!("Could not load desktop entry {}", path.display()))?;
    rix_services::applications::native_compat::prepare_desktop(&app)?;
    let context = gio::glib::MainContext::new();
    context.with_thread_default(|| -> anyhow::Result<()> {
        let launch_context = gio::AppLaunchContext::new();
        context.block_on(gio::glib::future_with_timeout(
            std::time::Duration::from_secs(30),
            app.launch_uris_future(&[], Some(&launch_context)),
        ))??;
        Ok(())
    })??;
    Ok(())
}

#[derive(Default)]
struct LaunchTracker {
    known_windows: HashSet<u64>,
    pending: Vec<(String, Instant, HashSet<u64>)>,
}
impl LaunchTracker {
    fn requested(&mut self, id: String) {
        self.pending
            .retain(|(_, since, _)| since.elapsed().as_secs() < 300);
        if self.pending.len() >= 64 {
            self.pending.remove(0);
        }
        self.pending
            .push((id, Instant::now(), self.known_windows.clone()));
    }
    fn observed(
        &mut self,
        windows: &[rix_ipc::WindowInfo],
        apps: &[crate::apps::AppInfo],
    ) -> Vec<(String, u64, u128)> {
        let mut completed = Vec::new();
        self.pending.retain(|(id, since, previous)| {
            let matching = windows.iter().find(|window| {
                !previous.contains(&window.id)
                    && crate::apps::find_app(apps, &window.app_id)
                        .map(|app| app.desktop_id.as_str())
                        .unwrap_or(window.app_id.trim_end_matches(".desktop"))
                        .eq_ignore_ascii_case(id)
            });
            if let Some(window) = matching {
                completed.push((id.clone(), window.id, since.elapsed().as_millis()));
                return false;
            }
            since.elapsed().as_secs() < 300
        });
        self.known_windows = windows.iter().map(|window| window.id).collect();
        completed
    }
}
static TRACKER: OnceLock<Mutex<LaunchTracker>> = OnceLock::new();
pub(crate) fn observe_windows(windows: &[rix_ipc::WindowInfo], apps: &[crate::apps::AppInfo]) {
    let completed = TRACKER
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .observed(windows, apps);
    for (id, window_id, elapsed_ms) in completed {
        tracing::info!(%id, window_id, elapsed_ms, "Application window observed after launch request");
    }
}

pub fn spawn_app(app: &crate::apps::AppInfo, language: &str) {
    spawn_entry(app, language, None);
}

pub fn spawn_desktop_action(app: &crate::apps::AppInfo, language: &str, action: &str) {
    spawn_entry(app, language, Some(action));
}

pub fn launch_from_menu(app: &crate::apps::AppInfo, apps: &[crate::apps::AppInfo], language: &str) {
    let running = crate::tasks::existing_window(app, apps).is_some();
    let action = menu_window_action(&app.desktop_file, running);
    spawn_entry(app, language, action.as_deref());
}

fn menu_window_action(path: &Path, running: bool) -> Option<String> {
    if !running {
        return None;
    }
    rix_services::applications::desktop_actions(path)
        .into_iter()
        .find(|action| {
            action
                .id
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .eq_ignore_ascii_case("newwindow")
        })
        .map(|action| action.id)
}

fn spawn_entry(app: &crate::apps::AppInfo, language: &str, action: Option<&str>) {
    let id = app.desktop_id.clone();
    let path = app.desktop_file.clone();
    let requested = Instant::now();
    TRACKER
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .requested(id.clone());
    tracing::info!(%id, desktop_file = %path.display(), has_wayland_display = std::env::var_os("WAYLAND_DISPLAY").is_some(), has_session_bus = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some(), "Application launch requested");
    let mut command = match reexec::command() {
        Ok(command) => command,
        Err(error) => {
            tracing::warn!(%id, %error, "Could not locate desktop-entry launcher");
            return;
        }
    };
    command
        .arg("--launch-desktop")
        .arg(path)
        .env_remove("DISPLAY")
        .stdin(std::process::Stdio::null());
    if let Some(action) = action {
        command.arg("--desktop-action").arg(action);
    }
    spawn_desktop_command(command, id, language, requested);
}

pub fn open_app(app: &crate::apps::AppInfo, apps: &[crate::apps::AppInfo], language: &str) {
    if let Some(id) = crate::tasks::existing_window(app, apps) {
        tracing::info!(desktop_id = %app.desktop_id, window_id = id, "Opening existing application window");
        crate::events::fire_and_forget_ipc(rix_ipc::IpcRequest::FocusWindow(id));
    } else {
        spawn_app(app, language);
    }
}

fn spawn_desktop_command(
    mut command: std::process::Command,
    id: String,
    language: &str,
    requested: Instant,
) {
    rix_services::resources::ordinary_command(&mut command);
    rix_services::locale::apply_to_command(&mut command, language);

    let mut child = match tokio::process::Command::from(command).spawn() {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(%id, %error, elapsed_ms = requested.elapsed().as_millis(), "Could not start desktop-entry launcher");
            return;
        }
    };
    tracing::info!(%id, pid = child.id(), spawn_ms = requested.elapsed().as_millis(), "Desktop-entry launcher started");
    tokio::spawn(async move {
        let status = loop {
            tokio::select! {
                status = child.wait() => break status,
                _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => tracing::warn!(%id, elapsed_ms = requested.elapsed().as_millis(), "Desktop-entry launcher is still waiting for activation"),
            }
        };
        match status {
            Ok(status) if status.success() => {
                tracing::info!(%id, elapsed_ms = requested.elapsed().as_millis(), "Desktop entry accepted the launch request")
            }
            Ok(status) => {
                tracing::warn!(%id, %status, elapsed_ms = requested.elapsed().as_millis(), "Application launch failed")
            }
            Err(error) => {
                tracing::warn!(%id, %error, elapsed_ms = requested.elapsed().as_millis(), "Could not complete desktop-entry launch")
            }
        }
    });
}

pub fn spawn_process(mut command: std::process::Command) -> std::io::Result<()> {
    if rix_services::resources::enabled() {
        let helper = std::env::current_exe()?.with_file_name("rix-run");
        let mut wrapped = std::process::Command::new(helper);
        wrapped
            .arg("--")
            .arg(command.get_program())
            .args(command.get_args());
        for (key, value) in command.get_envs() {
            match value {
                Some(value) => {
                    wrapped.env(key, value);
                }
                None => {
                    wrapped.env_remove(key);
                }
            }
        }
        if let Some(directory) = command.get_current_dir() {
            wrapped.current_dir(directory);
        }
        wrapped.stdin(std::process::Stdio::null());
        command = wrapped;
    }
    rix_services::resources::ordinary_command(&mut command);
    let mut child = tokio::process::Command::from(command).spawn()?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_menu_launch_uses_only_the_advertised_normal_new_window_action() {
        let root = tempfile::tempdir().unwrap();
        let desktop = root.path().join("fixture.desktop");
        std::fs::write(&desktop, "[Desktop Entry]\nType=Application\nName=Fixture\nExec=/usr/bin/true\nActions=new-private-window;new-window;\n[Desktop Action new-private-window]\nName=Private Window\nExec=/usr/bin/true\n[Desktop Action new-window]\nName=New Window\nExec=/usr/bin/true\n").unwrap();
        assert_eq!(menu_window_action(&desktop, false), None);
        assert_eq!(
            menu_window_action(&desktop, true).as_deref(),
            Some("new-window")
        );
        std::fs::write(&desktop, "[Desktop Entry]\nType=Application\nName=Fixture\nExec=/usr/bin/true\nActions=new-private-window;\n[Desktop Action new-private-window]\nName=Private Window\nExec=/usr/bin/true\n").unwrap();
        assert_eq!(
            menu_window_action(&desktop, true),
            None,
            "default activation remains available without a New Window action"
        );
    }
    #[test]
    fn dbus_launch_helper_process() {
        if let Some(path) = std::env::var_os("RIX_TEST_DESKTOP_HELPER") {
            if let Ok(action) = std::env::var("RIX_TEST_DESKTOP_ACTION") {
                rix_services::applications::launch_desktop_action(Path::new(&path), &action)
                    .unwrap();
            } else {
                launch_desktop(Path::new(&path)).unwrap();
            }
        }
    }

    #[test]
    fn cold_dbus_service_receives_activate_before_helper_exits() {
        cold_dbus_launch(None);
    }

    #[test]
    fn cold_dbus_service_receives_action_before_helper_exits() {
        cold_dbus_launch(Some("New"));
    }

    fn cold_dbus_launch(action: Option<&str>) {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("data");
        std::fs::create_dir_all(data.join("dbus-1/services")).unwrap();
        let source = directory.path().join("fixture.c");
        let binary = directory.path().join("fixture");
        let marker = directory.path().join("activated");
        std::fs::write(
            &source,
            r#"
#include <gio/gio.h>
static void activate(GApplication *app, gpointer unused) {
  g_file_set_contents(g_getenv("RIX_TEST_ACTIVATED"), "activated", -1, NULL);
  g_application_quit(app);
}
static void activate_action(GSimpleAction *action, GVariant *parameter, gpointer app) {
  g_file_set_contents(g_getenv("RIX_TEST_ACTIVATED"), "action:New", -1, NULL);
  g_application_quit(G_APPLICATION(app));
}
int main(int argc, char **argv) {
  g_usleep(400000);
  GApplication *app = g_application_new("org.rixde.LaunchFixture", G_APPLICATION_IS_SERVICE);
  g_signal_connect(app, "activate", G_CALLBACK(activate), NULL);
  GSimpleAction *action = g_simple_action_new("New", NULL);
  g_signal_connect(action, "activate", G_CALLBACK(activate_action), app);
  g_action_map_add_action(G_ACTION_MAP(app), G_ACTION(action));
  g_object_unref(action);
  g_application_hold(app);
  return g_application_run(app, argc, argv);
}
"#,
        )
        .unwrap();
        let flags = std::process::Command::new("pkg-config")
            .args(["--cflags", "--libs", "gio-2.0"])
            .output()
            .unwrap();
        assert!(flags.status.success());
        assert!(std::process::Command::new("cc")
            .arg(&source)
            .arg("-o")
            .arg(&binary)
            .args(String::from_utf8(flags.stdout).unwrap().split_whitespace())
            .status()
            .unwrap()
            .success());
        std::fs::write(
            data.join("dbus-1/services/org.rixde.LaunchFixture.service"),
            format!(
                "[D-BUS Service]\nName=org.rixde.LaunchFixture\nExec={}\n",
                binary.display()
            ),
        )
        .unwrap();
        let desktop = directory.path().join("org.rixde.LaunchFixture.desktop");
        std::fs::write(&desktop, format!("[Desktop Entry]\nType=Application\nName=Cold service fixture\nExec={}\nDBusActivatable=true\nActions=New;\n[Desktop Action New]\nName=New window\nExec=/usr/bin/false\n", binary.display())).unwrap();
        let start = Instant::now();
        let mut command = std::process::Command::new("dbus-run-session");
        command
            .arg("--")
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "launch::tests::dbus_launch_helper_process",
                "--nocapture",
            ])
            .env("RIX_TEST_DESKTOP_HELPER", desktop)
            .env("RIX_TEST_ACTIVATED", &marker)
            .env("XDG_DATA_HOME", &data)
            .env("XDG_DATA_DIRS", &data)
            .env("GIO_USE_VFS", "local")
            .env_remove("RIX_TEST_DESKTOP_ACTION");
        if let Some(action) = action {
            command.env("RIX_TEST_DESKTOP_ACTION", action);
        }
        let result = command.output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(start.elapsed() >= std::time::Duration::from_millis(400));
        assert_eq!(
            std::fs::read_to_string(marker).unwrap(),
            if action.is_some() {
                "action:New"
            } else {
                "activated"
            }
        );
    }

    #[test]
    fn missing_dbus_action_service_preserves_exec_fields_and_literal_arguments() {
        action_exec_fallback(false);
    }

    #[test]
    fn rejected_dbus_action_reports_remote_error_and_runs_exec_fallback() {
        action_exec_fallback(true);
    }

    fn action_exec_fallback(rejected: bool) {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("files with 'quote%k");
        let data = root.join("data");
        let bin = root.join("bin");
        let working_directory = root.join("working directory");
        for path in [&data, &bin, &working_directory] {
            std::fs::create_dir_all(path).unwrap();
        }
        let capture = bin.join("capture");
        std::fs::write(
            &capture,
            r#"#!/usr/bin/python3
import json, os, sys
marker = os.environ["RIX_TEST_ACTION_MARKER"]
with open(marker + ".tmp", "w") as output:
    json.dump({"args": sys.argv[1:], "cwd": os.getcwd(),
               "desktop": os.environ.get("GIO_LAUNCHED_DESKTOP_FILE"),
               "terminal": os.environ.get("RIX_TEST_TERMINAL")}, output)
os.replace(marker + ".tmp", marker)
"#,
        )
        .unwrap();
        std::fs::set_permissions(&capture, std::fs::Permissions::from_mode(0o755)).unwrap();
        for name in ["xdg-terminal-exec", "xterm"] {
            let terminal = bin.join(name);
            std::fs::write(
                &terminal,
                r#"#!/usr/bin/python3
import os, sys
args = sys.argv[1:]
if args[0] == "-e":
    args.pop(0)
os.environ["RIX_TEST_TERMINAL"] = "yes"
os.execv(args[0], args)
"#,
            )
            .unwrap();
            std::fs::set_permissions(terminal, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let desktop = root.join("org.rixde.MissingActionFixture.desktop");
        if rejected {
            let source = directory.path().join("reject.c");
            let binary = directory.path().join("reject");
            std::fs::write(
                &source,
                r#"
#include <gio/gio.h>
static void method(GDBusConnection *connection, const gchar *sender,
  const gchar *path, const gchar *interface, const gchar *name,
  GVariant *parameters, GDBusMethodInvocation *invocation, gpointer data) {
  g_dbus_method_invocation_return_dbus_error(invocation,
    "org.rixde.ActionDenied", "Fixture action rejected");
}
static void acquired(GDBusConnection *connection, const gchar *name, gpointer data) {
  const gchar *xml = "<node><interface name='org.freedesktop.Application'>"
    "<method name='ActivateAction'><arg type='s' direction='in'/>"
    "<arg type='av' direction='in'/><arg type='a{sv}' direction='in'/>"
    "</method></interface></node>";
  GDBusNodeInfo *info = g_dbus_node_info_new_for_xml(xml, NULL);
  static const GDBusInterfaceVTable vtable = { method, NULL, NULL };
  g_dbus_connection_register_object(connection, "/org/rixde/MissingActionFixture",
    info->interfaces[0], &vtable, NULL, NULL, NULL);
}
int main(void) {
  g_bus_own_name(G_BUS_TYPE_SESSION, "org.rixde.MissingActionFixture",
    G_BUS_NAME_OWNER_FLAGS_NONE, acquired, NULL, NULL, NULL, NULL);
  g_main_loop_run(g_main_loop_new(NULL, FALSE));
  return 0;
}
"#,
            )
            .unwrap();
            let flags = std::process::Command::new("pkg-config")
                .args(["--cflags", "--libs", "gio-2.0"])
                .output()
                .unwrap();
            assert!(flags.status.success());
            assert!(std::process::Command::new("cc")
                .arg(&source)
                .arg("-o")
                .arg(&binary)
                .args(String::from_utf8(flags.stdout).unwrap().split_whitespace())
                .status()
                .unwrap()
                .success());
            let services = data.join("dbus-1/services");
            std::fs::create_dir_all(&services).unwrap();
            std::fs::write(
                services.join("org.rixde.MissingActionFixture.service"),
                format!(
                    "[D-BUS Service]\nName=org.rixde.MissingActionFixture\nExec={}\n",
                    binary.display()
                ),
            )
            .unwrap();
        }
        for (terminal, dbus_only) in [(false, false), (true, false), (false, true)] {
            let marker = root.join(format!("result-{terminal}-{dbus_only}.json"));
            let keyfile = gio::glib::KeyFile::new();
            keyfile.set_string("Desktop Entry", "Type", "Application");
            keyfile.set_string("Desktop Entry", "Name", "Action fixture");
            if !dbus_only {
                keyfile.set_string("Desktop Entry", "Exec", "/usr/bin/false");
            }
            keyfile.set_string("Desktop Entry", "Actions", "New;");
            keyfile.set_string("Desktop Entry", "Path", working_directory.to_str().unwrap());
            keyfile.set_boolean("Desktop Entry", "DBusActivatable", true);
            keyfile.set_boolean("Desktop Entry", "Terminal", terminal);
            keyfile.set_string("Desktop Action New", "Name", "New window");
            keyfile.set_string(
                "Desktop Action New",
                "Exec",
                &format!(
                    "{} %k %%k \"two words\" '$HOME' '$(touch injected)' %c",
                    gio::glib::shell_quote(&capture)
                        .to_string_lossy()
                        .replace('%', "%%")
                ),
            );
            std::fs::write(&desktop, keyfile.to_data()).unwrap();
            let result = std::process::Command::new("/usr/bin/dbus-run-session")
                .arg("--dbus-daemon=/usr/bin/dbus-daemon")
                .arg("--")
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "launch::tests::dbus_launch_helper_process",
                    "--nocapture",
                ])
                .env("RIX_TEST_DESKTOP_HELPER", &desktop)
                .env("RIX_TEST_DESKTOP_ACTION", "New")
                .env("RIX_TEST_ACTION_MARKER", &marker)
                .env("PATH", &bin)
                .env("XDG_DATA_HOME", &data)
                .env("XDG_DATA_DIRS", &data)
                .env("GIO_USE_VFS", "local")
                .env_remove("RIX_TEST_TERMINAL")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(
                String::from_utf8_lossy(&result.stderr).contains(if rejected {
                    "org.rixde.ActionDenied"
                } else {
                    "ServiceUnknown"
                })
            );
            let deadline = Instant::now() + std::time::Duration::from_secs(2);
            while !marker.exists() && Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let output: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
            assert_eq!(
                output["args"],
                serde_json::json!([
                    desktop.to_str().unwrap(),
                    "%k",
                    "two words",
                    "$HOME",
                    "$(touch injected)",
                    "Action fixture"
                ])
            );
            assert_eq!(output["cwd"], working_directory.to_str().unwrap());
            assert_eq!(output["desktop"], desktop.to_str().unwrap());
            assert_eq!(
                output["terminal"],
                if terminal {
                    serde_json::json!("yes")
                } else {
                    serde_json::Value::Null
                }
            );
            assert!(!working_directory.join("injected").exists());
        }
    }
    #[test]
    fn launch_latency_tracks_only_new_windows_of_the_requested_application() {
        let make_window = |id, app: &str| rix_ipc::WindowInfo {
            id,
            title: String::new(),
            app_id: app.into(),
            outputs: vec![],
            foreground_outputs: None,
            is_active: false,
            is_minimized: false,
            is_maximized: false,
            is_fullscreen: false,
        };
        let mut tracker = LaunchTracker::default();
        tracker.observed(&[make_window(1, "editor.desktop")], &[]);
        tracker.requested("editor".into());
        assert!(tracker
            .observed(
                &[make_window(1, "editor.desktop"), make_window(2, "browser")],
                &[]
            )
            .is_empty());
        let completed = tracker.observed(
            &[make_window(1, "editor.desktop"), make_window(3, "editor")],
            &[],
        );
        assert_eq!((completed[0].0.as_str(), completed[0].1), ("editor", 3));
        assert!(tracker.pending.is_empty());
    }
    #[tokio::test]
    async fn gio_launch_preserves_quoted_arguments_on_first_private_bus_launch() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("record-launch");
        let marker = directory.path().join("received");
        let desktop = directory.path().join("fixture.desktop");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s' \"$1\" > \"$RIX_LAUNCH_MARKER\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(&desktop, format!("[Desktop Entry]\nType=Application\nName=Launch fixture\nExec={} \"hello with spaces\" %U\nTerminal=false\n", script.display())).unwrap();
        let mut child = tokio::process::Command::new("dbus-run-session")
            .args(["--", "gio", "launch"])
            .arg(&desktop)
            .env("RIX_LAUNCH_MARKER", &marker)
            .env("GIO_USE_VFS", "local")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(3), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + std::time::Duration::from_secs(3);
        while !marker.exists() {
            assert!(
                Instant::now() < deadline,
                "GIO accepted but did not execute the fixture"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            std::fs::read_to_string(marker).unwrap(),
            "hello with spaces"
        );
    }

    #[test]
    fn desktop_launch_starts_while_all_runtime_workers_are_busy() {
        use std::{
            os::unix::fs::PermissionsExt,
            sync::{mpsc, Arc, Condvar},
            time::Duration,
        };
        const FIXTURE_ENV: &str = "RIX_LAUNCH_BUSY_WORKERS_FIXTURE";
        if std::env::var_os(FIXTURE_ENV).is_none() {

            let mut child = std::process::Command::new("dbus-run-session")
                .arg("--")
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "launch::tests::desktop_launch_starts_while_all_runtime_workers_are_busy",
                    "--nocapture",
                ])
                .env(FIXTURE_ENV, "1")
                .env("GIO_USE_VFS", "local")
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success());
                    return;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("Busy-runtime launch fixture timed out");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("record-launch");
        let marker = root.path().join("received");
        let desktop = root.path().join("fixture.desktop");
        std::fs::write(&script, "#!/bin/sh\nprintf '%s' \"$1\" > \"$2\"\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(&desktop, format!("[Desktop Entry]\nType=Application\nName=Busy-runtime fixture\nExec={} \"hello with spaces\" {}\nTerminal=false\n", script.display(), marker.display())).unwrap();
        let app = crate::apps::AppInfo {
            desktop_file: desktop,
            desktop_id: "busy-runtime-fixture".into(),
            name: "Busy-runtime fixture".into(),
            exec: String::new(),
            icon_name: String::new(),
            icon_path: None,
            categories: Vec::new(),
            startup_wm_class: String::new(),
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        struct Release(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0 .0.lock().unwrap() = true;
                self.0 .1.notify_all();
            }
        }
        let release = Release(Arc::new((Mutex::new(false), Condvar::new())));
        let (entered, ready) = mpsc::sync_channel(2);
        for _ in 0..2 {
            let blocked = release.0.clone();
            let entered = entered.clone();
            runtime.spawn(async move {
                let mut done = blocked.0.lock().unwrap();
                entered.send(()).unwrap();
                while !*done {
                    done = blocked.1.wait(done).unwrap();
                }
            });
        }
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        let _entered = runtime.enter();

        let mut command = std::process::Command::new("gio");
        command.arg("launch").arg(&app.desktop_file);
        spawn_desktop_command(command, app.desktop_id.clone(), "system", Instant::now());
        let deadline = Instant::now() + Duration::from_secs(2);
        while !matches!(
            std::fs::read_to_string(&marker).as_deref(),
            Ok("hello with spaces")
        ) {
            assert!(
                Instant::now() < deadline,
                "Launch was queued behind runtime workers"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(!*release.0 .0.lock().unwrap());
    }
}
