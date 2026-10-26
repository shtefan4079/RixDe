use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
mod catalog;
pub mod file_opening;
pub mod native_compat;
pub use catalog::ApplicationCatalog;

#[derive(Clone, Debug, PartialEq)]
pub struct DesktopAction {
    pub id: String,
    pub name: String,
}

pub fn desktop_actions(path: &Path) -> Vec<DesktopAction> {
    let Some(info) = gio::DesktopAppInfo::from_filename(path) else {
        return Vec::new();
    };
    info.list_actions()
        .iter()
        .map(|id| DesktopAction {
            id: id.to_string(),
            name: info.action_name(id).to_string(),
        })
        .collect()
}

pub fn launch_desktop_action(path: &Path, action: &str) -> anyhow::Result<()> {
    use gio::glib::variant::ToVariant;
    use gio::prelude::*;
    let info = gio::DesktopAppInfo::from_filename(path)
        .ok_or_else(|| anyhow::anyhow!("Desktop entry is unavailable"))?;
    native_compat::prepare_desktop(&info)?;
    anyhow::ensure!(
        info.list_actions().iter().any(|id| id == action),
        "Unknown desktop action"
    );
    let launch_context = gio::AppLaunchContext::new();
    launch_context.unsetenv("DISPLAY");

    let app_id = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".desktop"))
        .filter(|id| !id.starts_with(':') && gio::dbus_is_name(id));
    if !info.boolean("DBusActivatable") || app_id.is_none() {
        info.launch_action(action, Some(&launch_context));
        return Ok(());
    }
    let app_id = app_id.unwrap();
    let context = gio::glib::MainContext::new();
    context.with_thread_default(|| -> anyhow::Result<()> {
        let activation = context.block_on(gio::glib::future_with_timeout(
            std::time::Duration::from_secs(30),
            async {
                let bus = gio::bus_get_future(gio::BusType::Session).await?;
                let mut platform_data =
                    std::collections::HashMap::<String, gio::glib::Variant>::new();
                if info.boolean("StartupNotify") {
                    if let Some(token) = launch_context.startup_notify_id(Some(&info), &[]) {
                        platform_data.insert("desktop-startup-id".into(), token.to_variant());
                        platform_data.insert("activation-token".into(), token.to_variant());
                    }
                }
                let parameters =
                    (action, Vec::<gio::glib::Variant>::new(), platform_data).to_variant();
                let object_path = format!("/{}", app_id.replace('.', "/").replace('-', "_"));
                bus.call_future(
                    Some(app_id),
                    &object_path,
                    "org.freedesktop.Application",
                    "ActivateAction",
                    Some(&parameters),
                    None,
                    gio::DBusCallFlags::NONE,
                    30_000,
                )
                .await
            },
        ))?;
        match activation {
            Ok(_) => Ok(()),
            Err(error) => {
                eprintln!("Desktop action {app_id}/{action}: D-Bus activation failed: {error}");

                if error.matches(gio::IOErrorEnum::TimedOut)
                    || error.matches(gio::DBusError::Timeout)
                    || error.matches(gio::DBusError::NoReply)
                {
                    return Err(error.into());
                }
                launch_desktop_action_exec(path, action, &launch_context).map_err(|fallback| {
                    anyhow::anyhow!(
                        "D-Bus activation failed: {error}; Exec fallback failed: {fallback}"
                    )
                })
            }
        }
    })?
}

fn launch_desktop_action_exec(
    path: &Path,
    action: &str,
    launch_context: &gio::AppLaunchContext,
) -> anyhow::Result<()> {
    use gio::prelude::*;
    let keyfile = gio::glib::KeyFile::new();
    keyfile.load_from_file(path, gio::glib::KeyFileFlags::KEEP_TRANSLATIONS)?;
    let exec = keyfile.string(&format!("Desktop Action {action}"), "Exec")?;
    let quoted_path = gio::glib::shell_quote(path)
        .into_string()
        .map_err(|_| anyhow::anyhow!("Desktop entry path is not valid UTF-8"))?;
    let mut expanded = String::new();
    let mut chars = exec.chars();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            match chars.next() {
                Some('k') => expanded.push_str(&quoted_path.replace('%', "%%")),
                Some(code) => {
                    expanded.push('%');
                    expanded.push(code);
                }
                None => expanded.push('%'),
            }
        } else {
            expanded.push(ch);
        }
    }

    keyfile.set_string(&format!("Desktop Action {action}"), "Exec", &expanded);
    if keyfile
        .string("Desktop Entry", "Exec")
        .map_or(true, |exec| exec.trim().is_empty())
    {

        let arguments = gio::glib::shell_parse_argv(&exec)?;
        let executable = arguments
            .first()
            .and_then(|argument| argument.to_str())
            .ok_or_else(|| anyhow::anyhow!("Desktop action executable is unavailable"))?
            .replace("%%", "%");
        keyfile.set_string(
            "Desktop Entry",
            "Exec",
            &gio::glib::shell_quote(executable).to_string_lossy(),
        );
    }
    keyfile.set_boolean("Desktop Entry", "DBusActivatable", false);
    let fallback = gio::DesktopAppInfo::from_keyfile(&keyfile)
        .ok_or_else(|| anyhow::anyhow!("Desktop action Exec is unavailable"))?;
    launch_context.setenv(
        "GIO_LAUNCHED_DESKTOP_FILE",
        path.to_str()
            .ok_or_else(|| anyhow::anyhow!("Desktop entry path is not valid UTF-8"))?,
    );

    fallback.launch_action(action, Some(launch_context));
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppInfo {
    pub desktop_file: PathBuf,
    pub name: String,
    pub desktop_id: String,
    pub exec: String,
    pub icon_name: String,
    pub icon_path: Option<PathBuf>,
    pub categories: Vec<String>,
    pub startup_wm_class: String,
}

pub fn scan_applications() -> Vec<AppInfo> {
    scan_applications_in(&application_dirs())
}

fn application_dirs() -> Vec<PathBuf> {
    let user_data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| dirs_home().join(".local/share"));
    let data_dirs = std::env::var_os("XDG_DATA_DIRS")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    application_dirs_for(&user_data, std::env::split_paths(&data_dirs))
}
fn application_dirs_for(
    user_data: &Path,
    data_dirs: impl Iterator<Item = PathBuf>,
) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    std::iter::once(user_data.join("applications"))
        .chain(
            data_dirs
                .filter(|dir| dir.is_absolute())
                .map(|dir| dir.join("applications")),
        )

        .chain([
            user_data.join("flatpak/exports/share/applications"),
            PathBuf::from("/var/lib/flatpak/exports/share/applications"),
        ])
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

fn scan_applications_in(search_dirs: &[PathBuf]) -> Vec<AppInfo> {
    let mut apps = Vec::new();
    let mut seen_ids = HashSet::new();

    for dir in search_dirs {
        let mut files = Vec::new();
        desktop_files(dir, &mut files, 0);
        files.sort();
        for path in files {
            let Some(id) = desktop_id(dir, &path) else {
                continue;
            };

            if seen_ids.insert(id.clone()) {
                if let Some(mut app) = parse_desktop_file(&path) {
                    app.desktop_id = id;
                    apps.push(app);
                }
            }
        }
    }

    apps.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.desktop_id.cmp(&b.desktop_id))
    });
    apps
}
fn desktop_id(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    Some(
        relative
            .with_extension("")
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("-"),
    )
}
fn desktop_files(dir: &Path, files: &mut Vec<PathBuf>, depth: usize) {
    if depth > 16 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "desktop")
        {
            files.push(path);
        } else if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            desktop_files(&path, files, depth + 1);
        }
    }
}

fn dirs_home() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home)
    } else {
        PathBuf::from("/tmp")
    }
}

fn command_exists(command: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let executable = |path: &Path| {
        path.metadata()
            .is_ok_and(|info| info.is_file() && info.permissions().mode() & 0o111 != 0)
    };
    if command.contains('/') {
        return executable(Path::new(command));
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| executable(&dir.join(command)))
    })
}

fn parse_desktop_file(path: &Path) -> Option<AppInfo> {
    let content = fs::read_to_string(path).ok()?;
    let mut in_desktop_entry = false;
    let mut name = None;
    let mut exec = None;
    let mut icon = None;
    let mut no_display = false;
    let mut hidden = false;
    let mut categories = Vec::new();
    let mut try_exec = None;
    let mut is_app = false;
    let mut dbus_activatable = false;
    let mut startup_wm_class = String::new();

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_desktop_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_desktop_entry {
            continue;
        }

        if let Some((k, v)) = line.split_once('=') {
            let key = k.trim();
            let val = v.trim();
            match key {
                "DBusActivatable" => dbus_activatable = val.eq_ignore_ascii_case("true"),
                "StartupWMClass" => startup_wm_class = val.to_owned(),
                "Type" => {
                    if val == "Application" {
                        is_app = true;
                    }
                }
                "Name" => {
                    if name.is_none() {
                        name = Some(val.to_string());
                    }
                }
                "Exec" => {
                    if exec.is_none() {

                        exec = Some(val.to_string());
                    }
                }
                "Icon" => {
                    if icon.is_none() {
                        icon = Some(val.to_string());
                    }
                }
                "Categories" => {
                    categories = val
                        .split(';')
                        .filter(|category| !category.is_empty())
                        .map(ToOwned::to_owned)
                        .collect();
                }
                "NoDisplay" => {
                    if val.eq_ignore_ascii_case("true") {
                        no_display = true;
                    }
                }
                "Hidden" => hidden = val.eq_ignore_ascii_case("true"),
                "TryExec" => try_exec = Some(val.to_string()),
                _ => {}
            }
        }
    }

    if is_app && !no_display && !hidden && try_exec.as_deref().is_none_or(command_exists) {
        if let (Some(name), Some(exec)) =
            (name, exec.or_else(|| dbus_activatable.then(String::new)))
        {
            if exec.is_empty() && !dbus_activatable {
                return None;
            }
            let icon_name = icon.unwrap_or_default();

            return Some(AppInfo {
                desktop_file: path.to_path_buf(),
                name,
                desktop_id: path.file_stem()?.to_string_lossy().to_string(),
                exec,
                icon_name,
                icon_path: None,
                categories,
                startup_wm_class,
            });
        }
    }

    None
}

pub fn find_app<'a>(apps: &'a [AppInfo], key: &str) -> Option<&'a AppInfo> {
    if key.is_empty() {
        return None;
    }

    let exact = apps
        .iter()
        .find(|app| app.desktop_id.eq_ignore_ascii_case(key));
    let key = key.strip_suffix(".desktop").unwrap_or(key);
    if key.is_empty() {
        return exact;
    }
    exact
        .or_else(|| {
            apps.iter()
                .find(|app| app.desktop_id.eq_ignore_ascii_case(key))
        })
        .or_else(|| apps.iter().find(|app| app.exec == key))
        .or_else(|| {
            apps.iter().find(|app| {
                !app.startup_wm_class.is_empty() && app.startup_wm_class.eq_ignore_ascii_case(key)
            })
        })
        .or_else(|| {
            apps.iter().find(|app| {
                let command = app
                    .exec
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_matches('"');
                Path::new(command)
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(key))
            })
        })
}

#[cfg(test)]
mod desktop_tests {
    use super::*;

    #[test]
    fn dbus_activatable_entries_without_exec_have_unambiguous_desktop_identity() {
        let root = tempfile::tempdir().unwrap();
        for id in ["com.example.First", "com.example.Second"] {
            fs::write(
                root.path().join(format!("{id}.desktop")),
                format!("[Desktop Entry]\nType=Application\nName={id}\nDBusActivatable=true\n"),
            )
            .unwrap();
        }
        let apps = scan_applications_in(&[root.path().to_path_buf()]);
        assert_eq!(apps.len(), 2);
        assert!(find_app(&apps, "").is_none());
        for app in &apps {
            assert!(app.exec.is_empty());
            assert_eq!(
                find_app(&apps, &app.desktop_id).unwrap().desktop_file,
                app.desktop_file
            );
        }
    }

    #[test]
    fn declared_desktop_actions_are_ordered_and_validate_identity() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("browser.desktop");
        fs::write(&path,"[Desktop Entry]\nType=Application\nName=Browser\nExec=true\nActions=Tab;Private;\n[Desktop Action Ignored]\nName=Not declared\nExec=false\n[Desktop Action Tab]\nName=New tab\nExec=true\n[Desktop Action Private]\nName=Private window\nExec=true\n").unwrap();
        assert_eq!(
            desktop_actions(&path),
            vec![
                DesktopAction {
                    id: "Tab".into(),
                    name: "New tab".into()
                },
                DesktopAction {
                    id: "Private".into(),
                    name: "Private window".into()
                }
            ]
        );
        assert!(launch_desktop_action(&path, "Ignored").is_err());
        assert!(launch_desktop_action(&path, "true; false").is_err());
    }

    #[test]
    fn exact_desktop_id_precedes_another_entries_command_alias() {
        let temp = tempfile::tempdir().unwrap();
        for (id, name) in [("alias", "A shortcut"), ("editor", "Editor")] {
            fs::write(
                temp.path().join(format!("{id}.desktop")),
                format!("[Desktop Entry]\nType=Application\nName={name}\nExec=editor\n"),
            )
            .unwrap();
        }
        let apps = scan_applications_in(&[temp.path().to_path_buf()]);
        assert_eq!(find_app(&apps, "editor").unwrap().desktop_id, "editor");
    }

    #[test]
    fn desktop_suffix_inside_telegram_identity_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("org.telegram.desktop.desktop");
        fs::write(
            &path,
            "[Desktop Entry]\nType=Application\nName=Telegram\nExec=Telegram -- %U\nStartupWMClass=TelegramDesktop\nDBusActivatable=true\n",
        )
        .unwrap();
        let apps = scan_applications_in(&[temp.path().to_path_buf()]);
        assert_eq!(apps[0].desktop_id, "org.telegram.desktop");
        for key in [
            "org.telegram.desktop",
            "org.telegram.desktop.desktop",
            "TelegramDesktop",
            "Telegram",
        ] {
            assert_eq!(
                find_app(&apps, key).unwrap().desktop_file,
                path,
                "Telegram should resolve from {key}"
            );
        }
        assert!(find_app(&apps, "org.telegram.desktop.desktop.desktop").is_none());
    }

    #[test]
    fn canonical_id_precedes_another_entries_desktop_filename() {
        let temp = tempfile::tempdir().unwrap();
        for id in ["org.example", "org.example.desktop"] {
            fs::write(
                temp.path().join(format!("{id}.desktop")),
                format!("[Desktop Entry]\nType=Application\nName={id}\nExec=true\n"),
            )
            .unwrap();
        }
        let apps = scan_applications_in(&[temp.path().to_path_buf()]);
        assert_eq!(
            find_app(&apps, "org.example.desktop").unwrap().desktop_id,
            "org.example.desktop"
        );
    }

    #[test]
    fn identity_matches_desktop_extension_and_startup_class() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("com.example.Editor.desktop");
        fs::write(&path,"[Desktop Entry]\nType=Application\nName=Editor\nExec=/opt/editor/editor %U\nStartupWMClass=EditorWindow\n").unwrap();
        let apps = vec![parse_desktop_file(&path).unwrap()];
        for id in ["com.example.Editor.desktop", "EditorWindow", "editor"] {
            assert_eq!(
                find_app(&apps, id).unwrap().desktop_id,
                "com.example.Editor"
            );
        }
        assert!(find_app(&apps, "Untitled document").is_none());
    }
    #[test]
    fn user_desktop_entry_overrides_system_and_preserves_quoted_exec() {
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user");
        let system = temp.path().join("system");
        fs::create_dir_all(&user).unwrap();
        fs::create_dir_all(&system).unwrap();
        fs::write(
            system.join("demo.desktop"),
            "[Desktop Entry]\nType=Application\nName=System Demo\nExec=system-demo\n",
        )
        .unwrap();
        fs::write(user.join("demo.desktop"), "[Desktop Entry]\nType=Application\nName=User Demo\nExec=\"/opt/My App/demo\" --title \"Hello World\" %U\n").unwrap();
        let apps = scan_applications_in(&[user.clone(), system.clone()]);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "User Demo");
        assert_eq!(apps[0].desktop_id, "demo");
        assert_eq!(
            apps[0].exec,
            "\"/opt/My App/demo\" --title \"Hello World\" %U"
        );

        fs::write(
            user.join("demo.desktop"),
            "[Desktop Entry]\nType=Application\nHidden=true\n",
        )
        .unwrap();
        assert!(scan_applications_in(&[user, system]).is_empty());
    }
    #[test]
    fn nested_entries_use_xdg_desktop_ids_and_hidden_override_masks_them() {
        let root = tempfile::tempdir().unwrap();
        let system = root.path().join("system");
        let user = root.path().join("user");
        fs::create_dir_all(system.join("vendor")).unwrap();
        fs::create_dir_all(&user).unwrap();
        fs::write(
            system.join("vendor/editor.desktop"),
            "[Desktop Entry]\nType=Application\nName=Nested editor\nExec=true\n",
        )
        .unwrap();
        let apps = scan_applications_in(&[user.clone(), system.clone()]);
        assert_eq!(apps[0].desktop_id, "vendor-editor");
        fs::write(
            user.join("vendor-editor.desktop"),
            "[Desktop Entry]\nHidden=true\n",
        )
        .unwrap();
        assert!(scan_applications_in(&[user, system]).is_empty());
    }
    #[test]
    fn flatpak_exports_are_discovered_without_shell_profile_and_paths_are_deduplicated() {
        let dirs = application_dirs_for(
            Path::new("/home/test/.local/share"),
            [
                PathBuf::from("/usr/share"),
                PathBuf::from("relative"),
                PathBuf::from("/usr/share"),
            ]
            .into_iter(),
        );
        assert_eq!(
            dirs[0],
            PathBuf::from("/home/test/.local/share/applications")
        );
        assert!(dirs.contains(&PathBuf::from(
            "/home/test/.local/share/flatpak/exports/share/applications"
        )));
        assert!(dirs.contains(&PathBuf::from(
            "/var/lib/flatpak/exports/share/applications"
        )));
        assert_eq!(
            dirs.iter()
                .filter(|path| path.as_path() == Path::new("/usr/share/applications"))
                .count(),
            1
        );
        assert!(dirs.iter().all(|path| path.is_absolute()));
    }
    #[test]
    fn try_exec_requires_executable_permission() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("fixture");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        assert!(!command_exists(executable.to_str().unwrap()));
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(command_exists(executable.to_str().unwrap()));
    }
}
