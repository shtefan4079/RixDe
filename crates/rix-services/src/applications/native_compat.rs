use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
};

const MAX_CONFIGURATION: u64 = 1024 * 1024;

fn native_rix() -> bool {
    native_environment(
        &std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
        &std::env::var("XDG_SESSION_TYPE").unwrap_or_default(),
        std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var("RIX_NATIVE_SESSION").is_ok_and(|native| native == "1"),
        std::env::var_os("DISPLAY").is_some_and(|display| !display.is_empty()),
        unsafe { libc::geteuid() },
    )
}

fn native_environment(
    desktop: &str,
    session: &str,
    native_display: bool,
    x11_display: bool,
    uid: u32,
) -> bool {
    desktop.split(':').any(|part| part == "RIX")
        && session == "wayland"
        && native_display
        && !x11_display
        && uid != 0
}

pub fn prepare(executable: &Path) -> io::Result<()> {
    if !native_rix()
        || !matches!(
            executable.to_str(),
            Some("zoom" | "/usr/bin/zoom" | "/opt/zoom/ZoomLauncher" | "/opt/zoom/zoom")
        )
    {
        return Ok(());
    }
    let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME is unavailable"))?;
    let home = Path::new(&home);
    if !home.is_absolute() {
        return Err(io::Error::other("HOME must be absolute"));
    }
    prepare_zoom(home).map(|_| ())
}

pub fn prepare_desktop(info: &gio::DesktopAppInfo) -> io::Result<()> {
    use gio::{glib::translate::ToGlibPtr, prelude::*};
    use std::{
        ffi::{CStr, OsStr},
        os::unix::ffi::OsStrExt,
    };
    let executable = unsafe {
        gio::ffi::g_app_info_get_executable(info.upcast_ref::<gio::AppInfo>().to_glib_none().0)
    };
    if executable.is_null() {
        return Ok(());
    }
    let bytes = unsafe { CStr::from_ptr(executable) }.to_bytes();
    prepare(Path::new(OsStr::from_bytes(bytes)))
}

pub fn prepare_installed_zoom() -> io::Result<()> {
    if Path::new("/usr/bin/zoom").exists() {
        prepare(Path::new("/usr/bin/zoom"))?;
    }
    Ok(())
}

fn safe_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.len() > MAX_CONFIGURATION
    {
        return Err(io::Error::other(
            "Zoom configuration is not a bounded, owned regular file",
        ));
    }
    Ok(())
}

struct ConfigurationLock(File);
impl Drop for ConfigurationLock {
    fn drop(&mut self) {

        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn configuration_lock(directory: &Path) -> io::Result<ConfigurationLock> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join(".rix-zoom-native.lock"))?;
    safe_file(&file)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ConfigurationLock(file))
}

fn prepare_zoom(home: &Path) -> io::Result<bool> {

    let directory = home.join(".config");
    fs::create_dir_all(&directory)?;
    let metadata = fs::metadata(&directory)?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::other(
            "Zoom configuration directory has a different owner",
        ));
    }

    let _lock = configuration_lock(&directory)?;
    let path = directory.join("zoomus.conf");
    let mut original = Vec::new();
    let existed = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(mut file) => {
            safe_file(&file)?;
            Read::by_ref(&mut file)
                .take(MAX_CONFIGURATION + 1)
                .read_to_end(&mut original)?;
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    if original.len() as u64 > MAX_CONFIGURATION || original.contains(&0) {
        return Err(io::Error::other(
            "Zoom configuration is too large or malformed",
        ));
    }
    let text = std::str::from_utf8(&original)
        .map_err(|_| io::Error::other("Zoom configuration is not UTF-8"))?;
    let updated = native_zoom_configuration(text);
    if updated.as_bytes() == original {
        return Ok(false);
    }
    if existed {
        let backup = directory.join("zoomus.conf.rix-wayland-backup");
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&backup)
        {
            Ok(mut file) => {
                file.write_all(&original)?;
                file.sync_all()?;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(&backup)?;
                safe_file(&file)?;
            }
            Err(error) => return Err(error),
        }
    }
    let mut replacement = tempfile::NamedTempFile::new_in(&directory)?;
    replacement.write_all(updated.as_bytes())?;
    replacement.as_file().sync_all()?;

    let current = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path);
    match current {
        Ok(mut file) => {
            safe_file(&file)?;
            let mut bytes = Vec::new();
            Read::by_ref(&mut file)
                .take(MAX_CONFIGURATION + 1)
                .read_to_end(&mut bytes)?;
            if !existed || bytes != original {
                return Err(io::Error::other(
                    "Zoom configuration changed during the update",
                ));
            }
        }
        Err(error) if !existed && error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    replacement.persist(&path).map_err(|error| error.error)?;
    File::open(directory)?.sync_all()?;
    Ok(true)
}

fn native_zoom_configuration(text: &str) -> String {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut result = String::with_capacity(text.len() + 40);
    let mut in_general = false;
    let mut wrote_setting = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if in_general && !wrote_setting {
                result.push_str(&format!("xwayland=false{newline}"));
                wrote_setting = true;
            }
            in_general = trimmed == "[General]";
        }
        if in_general
            && trimmed
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == "xwayland")
        {
            if !wrote_setting {
                result.push_str(&format!("xwayland=false{newline}"));
                wrote_setting = true;
            }
        } else {
            result.push_str(line);
        }
    }
    if !wrote_setting {
        if !result.is_empty() && !result.ends_with('\n') {
            result.push_str(newline);
        }
        if !in_general {
            result.push_str(&format!("[General]{newline}"));
        }
        result.push_str(&format!("xwayland=false{newline}"));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn completed_repair_releases_lock_even_when_a_fork_inherited_the_descriptor() {
        let home = tempfile::tempdir().unwrap();
        let directory = home.path().join(".config");
        fs::create_dir(&directory).unwrap();
        let guard = configuration_lock(&directory).unwrap();
        let pid = unsafe { libc::fork() };
        if pid == 0 {

            unsafe {
                libc::pause();
                libc::_exit(0);
            }
        }
        assert!(pid > 0, "fork failed");
        drop(guard);
        let result = prepare_zoom(home.path());
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
        assert!(result.unwrap());
        assert!(!prepare_zoom(home.path()).unwrap());
    }

    #[test]
    fn repair_only_applies_to_the_native_user_rix_session() {
        assert!(native_environment(
            "RIX:GNOME",
            "wayland",
            true,
            false,
            1000
        ));
        for (desktop, session, native, x11, uid) in [
            ("GNOME", "wayland", true, false, 1000),
            ("RIX", "x11", true, false, 1000),
            ("RIX", "wayland", false, false, 1000),
            ("RIX", "wayland", true, true, 1000),
            ("RIX", "wayland", true, false, 0),
        ] {
            assert!(!native_environment(desktop, session, native, x11, uid));
        }
    }

    #[test]
    fn preserves_other_sections_comments_and_crlf_and_is_idempotent() {
        let original = "; header\r\n[General]\r\nremember=true\r\nxwayland=true\r\nxwayland=true\r\n[Other]\r\nxwayland=true\r\n";
        let expected = "; header\r\n[General]\r\nremember=true\r\nxwayland=false\r\n[Other]\r\nxwayland=true\r\n";
        assert_eq!(native_zoom_configuration(original), expected);
        assert_eq!(native_zoom_configuration(expected), expected);
        assert_eq!(
            native_zoom_configuration("[Other]\nkey=value"),
            "[Other]\nkey=value\n[General]\nxwayland=false\n"
        );
    }

    #[test]
    fn repair_creates_private_backup_and_does_not_rewrite_twice() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir(home.path().join(".config")).unwrap();
        let path = home.path().join(".config/zoomus.conf");
        let original = b"[General]\nlabel=preserved\nxwayland=true\n";
        fs::write(&path, original).unwrap();
        assert!(prepare_zoom(home.path()).unwrap());
        assert_eq!(
            fs::read(home.path().join(".config/zoomus.conf.rix-wayland-backup")).unwrap(),
            original
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!prepare_zoom(home.path()).unwrap());
        assert_eq!(
            fs::read(home.path().join(".config/zoomus.conf.rix-wayland-backup")).unwrap(),
            original
        );
    }

    #[test]
    fn repair_rejects_symlinks_fifos_and_malformed_configs_without_replacing_them() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir(home.path().join(".config")).unwrap();
        let target = home.path().join("target");
        fs::write(&target, "keep").unwrap();
        let path = home.path().join(".config/zoomus.conf");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(prepare_zoom(home.path()).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
        fs::remove_file(&path).unwrap();
        let filename = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(filename.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        assert!(prepare_zoom(home.path()).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"[General]\0broken").unwrap();
        assert!(prepare_zoom(home.path()).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"[General]\0broken");
    }
}
