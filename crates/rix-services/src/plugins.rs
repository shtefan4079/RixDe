use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    Wallpaper,
    DesktopControl,
    ScreenCapture,
    RemoteInput,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub api: u32,
    pub id: String,
    pub name: String,
    pub description: String,
    pub exec: Vec<String>,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub settings: Vec<String>,
    #[serde(default)]
    pub session_bus: bool,
}
#[derive(Clone)]
pub struct Plugin {
    pub manifest: Manifest,
    pub directory: PathBuf,
    pub unavailable: Vec<String>,
}
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 120
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && !matches!(id, "." | "..")
}
fn command_path(directory: &Path, executable: &str) -> Result<PathBuf> {
    anyhow::ensure!(!executable.is_empty(), "Empty executable");
    if executable.starts_with("./") {
        let path = directory.join(executable).canonicalize()?;
        anyhow::ensure!(
            path.starts_with(directory.canonicalize()?),
            "Plugin executable escapes its directory"
        );
        Ok(path)
    } else {
        Ok(PathBuf::from(executable))
    }
}
pub fn executable_available(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if path.components().count() > 1 || path.is_absolute() {
        executable(path)
    } else {
        std::env::var_os("PATH")
            .is_some_and(|dirs| std::env::split_paths(&dirs).any(|d| executable(&d.join(path))))
    }
}
impl Plugin {
    pub fn load(path: &Path) -> Result<Self> {
        anyhow::ensure!(path.metadata()?.len() <= 65536, "Plugin manifest too large");
        let manifest: Manifest = serde_json::from_slice(&std::fs::read(path)?)?;
        anyhow::ensure!(manifest.api == 1, "Unsupported plugin API {}", manifest.api);
        anyhow::ensure!(valid_id(&manifest.id), "Invalid plugin ID");
        anyhow::ensure!(
            !manifest.exec.is_empty() && manifest.exec.len() <= 64,
            "Invalid command arguments"
        );
        anyhow::ensure!(
            manifest
                .exec
                .iter()
                .chain(&manifest.settings)
                .all(|s| s.len() <= 4096 && !s.contains('\0')),
            "Invalid command value"
        );
        let directory = path
            .parent()
            .context("Missing plugin directory")?
            .to_path_buf();
        let mut requirements = manifest.requires.clone();
        requirements.push(manifest.exec[0].clone());
        let unavailable = requirements
            .into_iter()
            .filter(|req| {
                command_path(&directory, req).map_or(true, |path| !executable_available(&path))
            })
            .collect();
        Ok(Self {
            manifest,
            directory,
            unavailable,
        })
    }
    pub fn command(&self, settings: bool) -> Result<std::process::Command> {
        let args = if settings {
            &self.manifest.settings
        } else {
            &self.manifest.exec
        };
        let executable = args.first().context("This plugin has no settings window")?;
        let mut cmd = std::process::Command::new(command_path(&self.directory, executable)?);
        cmd.args(&args[1..])
            .current_dir(&self.directory)
            .stdin(std::process::Stdio::null());
        Ok(cmd)
    }
}
pub fn directories() -> Vec<PathBuf> {
    let user = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
        });
    let dirs =
        std::env::var_os("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    std::iter::once(user.join("rix/plugins"))
        .chain(std::env::split_paths(&dirs).map(|p| p.join("rix/plugins")))
        .collect()
}
pub fn discover() -> Vec<Plugin> {
    discover_in(&directories())
}
pub fn discover_in(dirs: &[PathBuf]) -> Vec<Plugin> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten().take(256) {
            if !entry.path().is_dir() {
                continue;
            }

            if !seen.insert(entry.file_name()) {
                continue;
            }

            let alternate = entry.path().join("plugin.remote-desktop.json");
            let manifest = match std::fs::symlink_metadata(&alternate) {
                Ok(_) => alternate,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    entry.path().join("plugin.json")
                }
                Err(error) => {
                    tracing::warn!(%error, "Plugin manifest could not be inspected");
                    continue;
                }
            };
            match Plugin::load(&manifest) {
                Ok(plugin) if entry.file_name().to_string_lossy() == plugin.manifest.id => {
                    result.push(plugin)
                }
                Ok(_) => tracing::warn!("Plugin directory and ID must match"),
                Err(error) => tracing::warn!(%error,"Plugin manifest rejected"),
            }
        }
    }
    result.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_api_and_argv_without_shell_expansion() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join("plugin.json");
        let json = r#"{"api":1,"id":"org.example.demo","name":"Demo","description":"Test","exec":["true","$(touch /never)","a b"]}"#;
        std::fs::write(&manifest, json).unwrap();
        let plugin = Plugin::load(&manifest).unwrap();
        let command = plugin.command(false).unwrap();
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec!["$(touch /never)", "a b"]
        );
        std::fs::write(&manifest, json.replace("\"api\":1", "\"api\":2")).unwrap();
        assert!(Plugin::load(&manifest).is_err());
        assert!(!valid_id("../../bad"));
    }

    fn fixture_manifest(id: &str, name: &str, remote: bool) -> String {
        serde_json::json!({
            "api": 1,
            "id": id,
            "name": name,
            "description": "Disposable plugin",
            "exec": ["/bin/true"],
            "capabilities": if remote {
                vec!["screen-capture", "remote-input"]
            } else {
                vec!["screen-capture"]
            },
            "session_bus": true,
        })
        .to_string()
    }

    fn install_fixture(directory: &Path, id: &str, name: &str) -> PathBuf {
        let plugin = directory.join(id);
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join("plugin.json"),
            fixture_manifest(id, name, false),
        )
        .unwrap();
        plugin
    }

    #[test]
    fn remote_desktop_sidecar_is_preferred_without_changing_the_legacy_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let directory = install_fixture(temp.path(), "org.example.cast", "Legacy capture");
        let legacy_path = directory.join("plugin.json");
        let legacy = std::fs::read(&legacy_path).unwrap();
        std::fs::write(
            directory.join("plugin.remote-desktop.json"),
            fixture_manifest("org.example.cast", "Native remote desktop", true),
        )
        .unwrap();
        let plugins = discover_in(&[temp.path().to_owned()]);
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].directory, directory);
        assert_eq!(plugins[0].manifest.name, "Native remote desktop");
        assert!(plugins[0]
            .manifest
            .capabilities
            .contains(&Capability::RemoteInput));

        let old = Plugin::load(&legacy_path).unwrap();
        assert_eq!(old.manifest.name, "Legacy capture");
        assert!(!old.manifest.capabilities.contains(&Capability::RemoteInput));
        assert_eq!(std::fs::read(legacy_path).unwrap(), legacy);
    }

    #[test]
    fn malformed_or_mismatched_sidecar_never_falls_back_to_legacy_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let directory = install_fixture(temp.path(), "org.example.cast", "Legacy capture");
        let sidecar = directory.join("plugin.remote-desktop.json");
        for contents in [
            "invalid json".to_owned(),
            fixture_manifest("org.example.other", "Wrong identity", true),
            fixture_manifest("org.example.cast", "Unsupported API", true)
                .replace("\"api\":1", "\"api\":2"),
        ] {
            std::fs::write(&sidecar, contents).unwrap();
            assert!(discover_in(&[temp.path().to_owned()]).is_empty());
        }
    }

    #[test]
    fn broken_sidecar_symlink_does_not_fall_back_to_legacy_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let directory = install_fixture(temp.path(), "org.example.cast", "Legacy capture");
        std::os::unix::fs::symlink(
            directory.join("missing.json"),
            directory.join("plugin.remote-desktop.json"),
        )
        .unwrap();
        assert!(discover_in(&[temp.path().to_owned()]).is_empty());
    }

    #[test]
    fn user_override_masks_system_sidecar_and_malformed_user_sidecar() {
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user");
        let system = temp.path().join("system");
        let user_plugin = install_fixture(&user, "org.example.cast", "User capture");
        let system_plugin = install_fixture(&system, "org.example.cast", "System capture");
        std::fs::write(
            system_plugin.join("plugin.remote-desktop.json"),
            fixture_manifest("org.example.cast", "System remote desktop", true),
        )
        .unwrap();
        let plugins = discover_in(&[user.clone(), system.clone()]);
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].manifest.name, "User capture");
        assert_eq!(plugins[0].directory, user_plugin);
        std::fs::write(user_plugin.join("plugin.remote-desktop.json"), "broken").unwrap();
        assert!(discover_in(&[user, system]).is_empty());
    }

    #[test]
    fn legacy_plugins_without_a_sidecar_keep_their_existing_contract() {
        let temp = tempfile::tempdir().unwrap();
        let directory = install_fixture(temp.path(), "org.example.cast", "Legacy capture");
        let loaded = Plugin::load(&directory.join("plugin.json")).unwrap();
        let discovered = discover_in(&[temp.path().to_owned()]);
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].manifest, loaded.manifest);
        assert_eq!(discovered[0].directory, loaded.directory);
        assert_eq!(discovered[0].unavailable, loaded.unavailable);
    }
}
