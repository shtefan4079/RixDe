use crate::{ClavisProviderChoice, ClavisSettings, SettingsWindow};
use anyhow::{bail, Context, Result};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::{
    fs,
    io::Read,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    rc::Rc,
};

const PROVIDERS: &[&str] = &[
    "clavis",
    "auto",
    "kwallet6",
    "kwallet5",
    "kwallet",
    "gnome-libsecret",
    "none",
];
fn provider_name(id: &str) -> SharedString {
    match id {
        "clavis" => "Clavis".into(),
        "kwallet6" => "KWallet 6".into(),
        "kwallet5" => "KWallet 5".into(),
        "kwallet" => "KWallet".into(),
        "gnome-libsecret" | "gnome" => crate::i18n::tr("Secret Service"),
        "none" => crate::i18n::tr("None"),
        _ => crate::i18n::tr("Automatic"),
    }
}
fn config_directory() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("Configuration directory is unavailable")?;
    if !base.is_absolute() {
        bail!("Configuration directory must be absolute");
    }
    Ok(base.join("rix"))
}
fn read_provider(directory: &Path) -> Result<String> {
    if !directory.exists() {
        return Ok("auto".into());
    }
    validate_directory(directory)?;
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join("secret-store"))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok("auto".into()),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
    {
        bail!("Provider preference must be an owned protected regular file");
    }
    let mut text = String::new();
    Read::by_ref(&mut file).take(65).read_to_string(&mut text)?;
    let provider = text.trim();
    if text.len() > 64 || !PROVIDERS.contains(&provider) {
        bail!("Invalid saved password provider");
    }
    Ok(provider.into())
}
fn validate_directory(directory: &Path) -> Result<()> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(directory)?;
    let metadata = file.metadata()?;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
        bail!("Provider configuration directory must be owned and protected");
    }
    Ok(())
}
fn ensure_protected_directory(directory: &Path) -> Result<()> {
    let created = match fs::symlink_metadata(directory) {
        Ok(_) => false,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => return Err(error.into()),
    };
    fs::create_dir_all(directory)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(directory)?;
    let metadata = file.metadata()?;
    if metadata.uid() != unsafe { libc::geteuid() } {
        bail!("Provider configuration directory must be owned by the current user");
    }

    if created {
        let protected_mode = metadata.mode() & 0o777 & !0o022;
        if protected_mode != metadata.mode() & 0o777
            && unsafe { libc::fchmod(file.as_raw_fd(), protected_mode) } != 0
        {
            return Err(std::io::Error::last_os_error())
                .context("Protect provider configuration directory");
        }
    }
    validate_directory(directory)
}
fn save_provider(directory: &Path, provider: &str) -> Result<()> {
    if !PROVIDERS.contains(&provider) {
        bail!("Unknown password provider");
    }
    ensure_protected_directory(directory)?;
    match fs::symlink_metadata(directory.join("secret-store")) {
        Ok(metadata)
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o022 != 0 =>
        {
            bail!("Provider preference must be an owned regular file")
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    rix_config::write_atomic(
        &directory.join("secret-store"),
        format!("{provider}\n").as_bytes(),
    )?;
    Ok(())
}
fn command_available(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|path| {
            fs::metadata(path.join(name))
                .is_ok_and(|metadata| metadata.is_file() && metadata.mode() & 0o111 != 0)
        })
    })
}
fn provider_available(id: &str) -> bool {
    match id {
        "auto" | "none" => true,
        "clavis" => {
            ["/usr", "/usr/local"].iter().any(|prefix| {
                let base = Path::new(prefix);
                base.join("libexec/rixde/clavis-daemon").is_file()
                    && std::ffi::CString::new(
                        base.join("libexec/rixde/clavis-daemon")
                            .as_os_str()
                            .as_encoded_bytes(),
                    )
                    .is_ok_and(|path| unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 })
                    && fs::File::open(base.join(
                        "share/clavis/session/dbus-1/services/org.freedesktop.secrets.service",
                    ))
                    .is_ok()
            })
        }
        "kwallet6" => command_available("ksecretd") || command_available("kwalletd6"),
        "kwallet5" => command_available("kwalletd5"),
        "kwallet" => command_available("kwalletd"),
        "gnome-libsecret" => command_available("gnome-keyring-daemon"),
        _ => false,
    }
}
fn refresh(window: &SettingsWindow, selected: &str, available: Vec<bool>) {
    let global = window.global::<ClavisSettings>();
    global.set_selected(selected.into());
    global.set_next_provider(provider_name(selected));
    global.set_choices(ModelRc::from(Rc::new(VecModel::from(
        PROVIDERS
            .iter()
            .zip(available)
            .map(|(id, available)| ClavisProviderChoice {
                id: (*id).into(),
                name: provider_name(id),
                available,
                detail: if !available {
                    crate::i18n::tr("Install this provider before selecting it")
                } else if *id == "auto" {
                    crate::i18n::tr("Keep automatic provider selection")
                } else if *id == "none" {
                    crate::i18n::tr("Do not start a password provider in RixDE")
                } else {
                    crate::i18n::tr("Use this provider after the next login")
                },
            })
            .collect::<Vec<_>>(),
    ))));
}
fn load_preferences() -> Result<(String, Vec<bool>)> {
    Ok((
        read_provider(&config_directory()?)?,
        PROVIDERS.iter().map(|id| provider_available(id)).collect(),
    ))
}

pub fn install(window: &SettingsWindow) {
    let current = std::env::var("RIX_SECRET_STORE").unwrap_or_else(|_| "auto".into());
    let global = window.global::<ClavisSettings>();
    global.set_provider(provider_name(&current));
    global.set_busy(true);
    let weak = window.as_weak();
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(load_preferences)
            .await
            .unwrap_or_else(|error| Err(error.into()));
        let _ = weak.upgrade_in_event_loop(move |window| {
            let global = window.global::<ClavisSettings>();
            global.set_busy(false);
            match result {
                Ok((selected, choices)) => refresh(&window, &selected, choices),
                Err(error) => global.set_status(error.to_string().into()),
            }
        });
    });
    let weak = window.as_weak();
    global.on_choose(move |selected| {
        let Some(window) = weak.upgrade() else { return };
        if window.global::<ClavisSettings>().get_busy() { return; }
        let selected = selected.to_string();
        if !PROVIDERS.contains(&selected.as_str()) { return; }
        window.global::<ClavisSettings>().set_busy(true);
        window.global::<ClavisSettings>().set_status(crate::i18n::tr("Applying…"));
        let weak = window.as_weak();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                if !provider_available(&selected) { bail!("Selected password provider is not installed or its activation files are inaccessible"); }
                save_provider(&config_directory()?, &selected)?;
                load_preferences()
            }).await.unwrap_or_else(|error| Err(error.into()));
            let _ = weak.upgrade_in_event_loop(move |window| {
                let global = window.global::<ClavisSettings>();
                global.set_busy(false);
                match result {
                    Ok((selected, choices)) => {
                        refresh(&window, &selected, choices);
                        global.set_status(crate::i18n::tr("Saved. The provider changes after the next login; open applications keep their current storage."));
                    },
                    Err(error) => global.set_status(error.to_string().into()),
                }
            });
        });
    });
    let weak = window.as_weak();
    global.on_open(move || {
        let mut command = std::process::Command::new("clavis");
        command.arg("gui");
        rix_services::resources::ordinary_command(&mut command);
        match command.spawn() {
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(_) => {
                if let Some(window) = weak.upgrade() {
                    window.set_status_message(crate::i18n::tr(
                        "Clavis is not installed or could not be opened",
                    ));
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    #[test]
    fn selecting_provider_is_private_persistent_and_does_not_touch_vaults() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(read_provider(directory.path()).unwrap(), "auto");
        fs::write(directory.path().join("vault"), b"keep encrypted data").unwrap();
        save_provider(directory.path(), "clavis").unwrap();
        assert_eq!(read_provider(directory.path()).unwrap(), "clavis");
        assert_eq!(
            fs::metadata(directory.path().join("secret-store"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::read(directory.path().join("vault")).unwrap(),
            b"keep encrypted data"
        );
        assert!(save_provider(directory.path(), "bad\nclavis").is_err());
        assert_eq!(read_provider(directory.path()).unwrap(), "clavis");
    }
    #[test]
    fn unsafe_preference_is_rejected_without_following_or_blocking() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("sentinel");
        fs::write(&target, b"keep").unwrap();
        let path = directory.path().join("secret-store");
        symlink(&target, &path).unwrap();
        assert!(read_provider(directory.path()).is_err());
        assert!(save_provider(directory.path(), "none").is_err());
        assert_eq!(fs::read(&target).unwrap(), b"keep");
        fs::remove_file(&path).unwrap();
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(read_provider(directory.path()).is_err());
        assert!(save_provider(directory.path(), "none").is_err());
    }
    #[test]
    fn writable_preference_or_directory_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        save_provider(directory.path(), "clavis").unwrap();
        let path = directory.path().join("secret-store");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(read_provider(directory.path()).is_err());
        assert!(save_provider(directory.path(), "none").is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(read_provider(directory.path()).is_err());
        assert!(save_provider(directory.path(), "none").is_err());
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    }
}
