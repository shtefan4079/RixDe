use anyhow::{bail, Context, Result};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub icon: String,
    pub enabled: bool,
    pub available: bool,
    pub custom: bool,
    pub path: PathBuf,
}

#[derive(Clone)]
pub struct AutostartStore {
    local: PathBuf,
    inherited: Vec<PathBuf>,
}

fn keys(path: &Path) -> Result<BTreeMap<String, String>> {
    let text = fs::read_to_string(path)?;
    let mut active = false;
    let mut result = BTreeMap::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            active = line == "[Desktop Entry]";
        } else if active && !line.starts_with('#') {
            if let Some((key, value)) = line.split_once('=') {
                result.insert(key.trim().into(), value.trim().into());
            }
        }
    }
    Ok(result)
}
fn yes(values: &BTreeMap<String, String>, key: &str) -> bool {
    values
        .get(key)
        .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}
fn executable(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let exists = |path: &Path| {
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if name.contains('/') {
        exists(Path::new(name))
    } else {
        std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| exists(&d.join(name))))
    }
}
impl AutostartStore {
    pub fn new(rix_config: &Path) -> Self {
        let config_home = rix_config.parent().unwrap_or(rix_config);
        let dirs = std::env::var_os("XDG_CONFIG_DIRS")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/etc/xdg".into());
        Self::in_dirs(
            rix_config.join("autostart"),
            std::iter::once(config_home.join("autostart"))
                .chain(std::env::split_paths(&dirs).map(|d| d.join("autostart")))
                .collect(),
        )
    }
    pub fn in_dirs(local: PathBuf, inherited: Vec<PathBuf>) -> Self {
        Self { local, inherited }
    }
    pub fn list(&self) -> Result<Vec<Entry>> {
        let mut seen = HashSet::new();
        let mut result = Vec::new();
        for dir in std::iter::once(&self.local).chain(&self.inherited) {
            let entries = match fs::read_dir(dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            for file in entries.flatten() {
                let path = file.path();
                if path.extension().is_none_or(|ext| ext != "desktop") {
                    continue;
                }
                let id = file.file_name().to_string_lossy().into_owned();
                if !seen.insert(id.clone()) {
                    continue;
                }
                let values = match keys(&path) {
                    Ok(values) => values,
                    Err(_) => continue,
                };
                let contains_rix = |key: &str| {
                    values
                        .get(key)
                        .is_some_and(|v| v.split(';').any(|d| d == "RIX"))
                };
                if values.contains_key("OnlyShowIn") && !contains_rix("OnlyShowIn")
                    || contains_rix("NotShowIn")
                {
                    continue;
                }
                if values.get("Type").is_none_or(|t| t != "Application") {
                    continue;
                }
                let available = values
                    .get("TryExec")
                    .filter(|v| !v.is_empty())
                    .is_none_or(|v| executable(v))
                    && (values.get("Exec").is_some_and(|v| !v.is_empty())
                        || yes(&values, "DBusActivatable"));
                result.push(Entry {
                    name: values.get("Name").cloned().unwrap_or_else(|| id.clone()),
                    icon: values.get("Icon").cloned().unwrap_or_default(),
                    enabled: !yes(&values, "Hidden"),
                    available,
                    custom: yes(&values, "X-Rix-Added"),
                    id,
                    path,
                });
            }
        }
        result.sort_by_key(|entry| entry.name.to_lowercase());
        Ok(result)
    }
    fn write(&self, id: &str, mut values: BTreeMap<String, String>) -> Result<()> {
        if !id.ends_with(".desktop") || Path::new(id).components().count() != 1 {
            bail!("Invalid desktop entry ID");
        }
        values.remove("NotShowIn");
        values.insert("OnlyShowIn".into(), "RIX;".into());
        let mut content = String::from("[Desktop Entry]\n");
        for (k, v) in values {
            content.push_str(&format!("{k}={v}\n"));
        }
        fs::create_dir_all(&self.local)?;
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = self
            .local
            .join(format!(".{id}.{}.{serial}.tmp", std::process::id()));
        let result = (|| -> std::io::Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            fs::rename(&tmp, self.local.join(id))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result?;
        Ok(())
    }
    pub fn add(&self, app: &crate::applications::AppInfo) -> Result<()> {
        let mut values = keys(&app.desktop_file)?;
        values.insert("Hidden".into(), "false".into());
        values.insert("X-Rix-Added".into(), "true".into());
        self.write(&format!("{}.desktop", app.desktop_id), values)
    }
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let entry = self
            .list()?
            .into_iter()
            .find(|e| e.id == id)
            .context("Autostart entry no longer exists")?;
        let mut values = keys(&entry.path)?;
        values.insert("Hidden".into(), (!enabled).to_string());
        self.write(id, values)
    }
    pub fn remove(&self, id: &str) -> Result<()> {
        let entry = self
            .list()?
            .into_iter()
            .find(|e| e.id == id)
            .context("Autostart entry no longer exists")?;
        if entry.custom && entry.path.parent() == Some(self.local.as_path()) {

            if self.inherited.iter().any(|dir| dir.join(id).exists()) {
                return self.set_enabled(id, false);
            }
            fs::remove_file(entry.path)?;
            Ok(())
        } else {
            self.set_enabled(id, false)
        }
    }
    pub fn launch_command(entry: &Entry, wayland_socket: &str) -> Result<Command> {
        if !entry.enabled || !entry.available {
            bail!("Autostart entry is disabled or unavailable");
        }
        let mut command = Command::new(if crate::resources::enabled() {
            "rix-run"
        } else {
            "gio"
        });
        if crate::resources::enabled() {
            command.args(["--", "gio"]);
            crate::resources::ordinary_command(&mut command);
        }
        command
            .args([std::ffi::OsStr::new("launch"), entry.path.as_os_str()])
            .env("WAYLAND_DISPLAY", wayland_socket)
            .env_remove("DISPLAY")
            .env("XDG_SESSION_DESKTOP", "RIX")
            .env("XDG_SESSION_TYPE", "wayland")
            .stdin(std::process::Stdio::null());
        Ok(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overrides_are_local_and_respect_desktop_filters_and_hidden_precedence() {
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user");
        let system = temp.path().join("system");
        fs::create_dir_all(&user).unwrap();
        fs::create_dir_all(&system).unwrap();
        let desktop = "[Desktop Entry]\nType=Application\nName=Example\nExec=example %U\n";
        fs::write(system.join("app.desktop"), desktop).unwrap();
        fs::write(
            system.join("kde.desktop"),
            format!("{desktop}OnlyShowIn=KDE;\n"),
        )
        .unwrap();
        fs::write(
            system.join("absent.desktop"),
            format!("{desktop}TryExec=/missing/executable\n"),
        )
        .unwrap();
        fs::write(user.join("app.desktop"), format!("{desktop}Hidden=true\n")).unwrap();
        let store =
            AutostartStore::in_dirs(temp.path().join("rix"), vec![user.clone(), system.clone()]);
        let entries = store.list().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            !entries
                .iter()
                .find(|e| e.id == "app.desktop")
                .unwrap()
                .enabled
        );
        assert!(
            !entries
                .iter()
                .find(|e| e.id == "absent.desktop")
                .unwrap()
                .available
        );
        store.set_enabled("app.desktop", true).unwrap();
        let entry = store
            .list()
            .unwrap()
            .into_iter()
            .find(|e| e.id == "app.desktop")
            .unwrap();
        assert!(entry.enabled);
        assert!(fs::read_to_string(user.join("app.desktop"))
            .unwrap()
            .contains("Hidden=true"));
        assert_eq!(
            fs::read_to_string(system.join("app.desktop")).unwrap(),
            desktop
        );
        store.remove("app.desktop").unwrap();
        assert!(
            !store
                .list()
                .unwrap()
                .iter()
                .find(|e| e.id == "app.desktop")
                .unwrap()
                .enabled
        );
        assert!(store.write("../escape.desktop", BTreeMap::new()).is_err());
    }
}
