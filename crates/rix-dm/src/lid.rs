use anyhow::{ensure, Context, Result};
use std::{
    ffi::{c_char, c_int, c_void},
    fs,
    io::{self, Read},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    path::{Component, Path},
    ptr,
};

#[link(name = "systemd")]
unsafe extern "C" {
    fn sd_bus_open_system(bus: *mut *mut c_void) -> c_int;
    fn sd_bus_set_method_call_timeout(bus: *mut c_void, usec: u64) -> c_int;
    fn sd_bus_call_method(
        bus: *mut c_void,
        destination: *const c_char,
        path: *const c_char,
        interface: *const c_char,
        member: *const c_char,
        error: *mut c_void,
        reply: *mut *mut c_void,
        types: *const c_char,
        ...
    ) -> c_int;
    fn sd_bus_message_read(message: *mut c_void, types: *const c_char, ...) -> c_int;
    fn sd_bus_message_unref(message: *mut c_void) -> *mut c_void;
    fn sd_bus_close_unref(bus: *mut c_void) -> *mut c_void;
}

struct Bus(*mut c_void);
impl Drop for Bus {
    fn drop(&mut self) {
        unsafe {
            sd_bus_close_unref(self.0);
        }
    }
}
struct Message(*mut c_void);
impl Drop for Message {
    fn drop(&mut self) {
        unsafe {
            sd_bus_message_unref(self.0);
        }
    }
}
fn check(status: c_int) -> io::Result<()> {
    if status < 0 {
        return Err(io::Error::from_raw_os_error(-status));
    }
    Ok(())
}

fn acquire() -> Result<OwnedFd> {
    acquire_on(sd_bus_open_system)
}
fn acquire_on(open: unsafe extern "C" fn(*mut *mut c_void) -> c_int) -> Result<OwnedFd> {
    let mut bus = Bus(ptr::null_mut());
    check(unsafe { open(&mut bus.0) }).context("Cannot connect to logind")?;

    check(unsafe { sd_bus_set_method_call_timeout(bus.0, 2_000_000) })?;
    let mut reply = Message(ptr::null_mut());
    check(unsafe {
        sd_bus_call_method(
            bus.0,
            c"org.freedesktop.login1".as_ptr(),
            c"/org/freedesktop/login1".as_ptr(),
            c"org.freedesktop.login1.Manager".as_ptr(),
            c"Inhibit".as_ptr(),
            ptr::null_mut(),
            &mut reply.0,
            c"ssss".as_ptr(),
            c"handle-lid-switch".as_ptr(),
            c"Rix DM".as_ptr(),
            c"Keep the external display on across login and logout".as_ptr(),
            c"block".as_ptr(),
        )
    })
    .context("Cannot inhibit the lid switch during graphical handoff")?;
    let mut borrowed = -1;
    check(unsafe { sd_bus_message_read(reply.0, c"h".as_ptr(), &mut borrowed as *mut c_int) })?;
    ensure!(
        borrowed >= 0,
        "logind did not return an inhibitor descriptor"
    );
    let owned = unsafe { libc::fcntl(borrowed, libc::F_DUPFD_CLOEXEC, 3) };
    if owned < 0 {
        return Err(io::Error::last_os_error().into());
    }

    Ok(unsafe { OwnedFd::from_raw_fd(owned) })
}

#[derive(Default)]
pub(super) struct LidGuard {
    lease: Option<OwnedFd>,
}
impl LidGuard {
    pub(super) fn update(&mut self, external: bool, keep_running: bool) -> Result<()> {
        self.update_with(external && keep_running, acquire)
    }
    fn update_with(
        &mut self,
        enabled: bool,
        acquire: impl FnOnce() -> Result<OwnedFd>,
    ) -> Result<()> {
        if enabled && self.lease.is_none() {
            self.lease = Some(acquire()?);
            eprintln!("rix-dm: external display connected; lid switch inhibited across graphical sessions");
        } else if !enabled && self.lease.take().is_some() {
            eprintln!("rix-dm: lid switch returned to logind");
        }
        Ok(())
    }
    pub(super) fn fd(&self) -> Option<RawFd> {
        self.lease.as_ref().map(AsRawFd::as_raw_fd)
    }
}

pub(super) fn external_display_connected() -> bool {
    external_display_connected_in(Path::new("/sys/class/drm"))
}
fn external_display_connected_in(root: &Path) -> bool {
    let Ok(entries) = fs::read_dir(root) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let Some((card, connector)) = name.to_str().and_then(|name| name.split_once('-')) else {
            return false;
        };
        card.strip_prefix("card")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
            && !["eDP-", "LVDS-", "DSI-"]
                .iter()
                .any(|prefix| connector.starts_with(prefix))
            && fs::read_to_string(entry.path().join("status"))
                .is_ok_and(|status| status.trim() == "connected")
    })
}

pub(super) fn keep_running_from(path: &Path) -> bool {
    let read = || -> Option<bool> {
        let mut file = open_config_without_symlinks(path).ok()?;
        let info = file.metadata().ok()?;
        if !info.is_file() || info.len() > 64 * 1024 {
            return None;
        }
        let mut bytes = Vec::new();
        file.by_ref()
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > 64 * 1024 {
            return None;
        }
        let document: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        document
            .get("session")?
            .get("keep_running_with_external_display")?
            .as_bool()
    };
    read().unwrap_or(true)
}

fn open_config_without_symlinks(path: &Path) -> io::Result<fs::File> {
    if !path.is_absolute() {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    let mut directory = fs::File::open("/")?;
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => {
                let name = std::ffi::CString::new(name.as_encoded_bytes())?;
                let last = components.peek().is_none();

                let flags = libc::O_PATH
                    | libc::O_CLOEXEC
                    | libc::O_NOFOLLOW
                    | if last { 0 } else { libc::O_DIRECTORY };
                let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
                if fd < 0 {
                    return Err(io::Error::last_os_error());
                }
                directory = unsafe { fs::File::from_raw_fd(fd) };
                if last {
                    let info = directory.metadata()?;
                    if !info.is_file() || info.len() > 64 * 1024 {
                        return Err(io::Error::from_raw_os_error(libc::EINVAL));
                    }

                    return fs::File::open(format!("/proc/self/fd/{}", directory.as_raw_fd()));
                }
            }
            _ => return Err(io::Error::from_raw_os_error(libc::EINVAL)),
        }
    }
    Err(io::Error::from_raw_os_error(libc::EINVAL))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::{fs::symlink, net::UnixStream};

    fn lease(guard: &mut LidGuard, enabled: bool) -> Option<UnixStream> {
        let mut peer = None;
        guard
            .update_with(enabled, || {
                let (fd, other) = UnixStream::pair()?;
                peer = Some(other);
                Ok(fd.into())
            })
            .unwrap();
        peer
    }
    fn released(mut peer: UnixStream) {
        peer.set_read_timeout(Some(std::time::Duration::from_millis(100)))
            .unwrap();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
    #[test]
    fn lease_survives_logout_and_releases_for_user_policy_or_monitor_disconnect() {
        let mut guard = LidGuard::default();
        let peer = lease(&mut guard, true).unwrap();
        let descriptor = guard.fd();
        assert!(lease(&mut guard, true).is_none());
        assert!(lease(&mut guard, true).is_none());
        assert_eq!(
            guard.fd(),
            descriptor,
            "logout must not drop and reacquire the lease"
        );
        lease(&mut guard, false);
        released(peer);
        let peer = lease(&mut guard, true).unwrap();
        lease(&mut guard, false);
        released(peer);
        let peer = lease(&mut guard, true).unwrap();
        drop(guard);
        released(peer);
    }
    #[test]
    fn only_connected_external_outputs_enable_the_lease() {
        let root = tempfile::tempdir().unwrap();
        let connector = |name: &str, status: &str| {
            let path = root.path().join(name);
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("status"), status).unwrap();
        };
        connector("card0-eDP-1", "connected\n");
        connector("card1-DP-1", "disconnected\n");
        connector("not-a-card-HDMI-A-1", "connected\n");
        assert!(!external_display_connected_in(root.path()));
        connector("card1-DP-1", "connected\n");
        assert!(external_display_connected_in(root.path()));
        connector("card1-DP-1", "disconnected\n");
        assert!(!external_display_connected_in(root.path()));
    }
    #[test]
    fn preference_reader_handles_old_profiles_and_rejects_symlinks_special_and_large_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("desktop.json");
        assert!(keep_running_from(&path));
        fs::write(
            &path,
            br#"{"session":{"keep_running_with_external_display":false},"other":42}"#,
        )
        .unwrap();
        assert!(!keep_running_from(&path));
        let link = root.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(keep_running_from(&link));
        let directory_link = root.path().join("directory-link");
        symlink(root.path(), &directory_link).unwrap();
        assert!(keep_running_from(&directory_link.join("desktop.json")));
        assert!(keep_running_from(root.path()));
        fs::write(&path, vec![b' '; 64 * 1024 + 1]).unwrap();
        assert!(keep_running_from(&path));
        fs::write(&path, b"{}").unwrap();
        assert!(keep_running_from(&path));
        let fifo = root.path().join("fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(keep_running_from(&fifo));
    }

    struct Manager(tokio::sync::mpsc::UnboundedSender<UnixStream>);
    #[zbus::interface(name = "org.freedesktop.login1.Manager")]
    impl Manager {
        fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::zvariant::OwnedFd {
            assert_eq!(
                what, "handle-lid-switch",
                "DM must not block explicit sleep"
            );
            assert_eq!(mode, "block");
            assert_eq!(who, "Rix DM");
            assert!(why.contains("logout"));
            let (lease, peer) = UnixStream::pair().unwrap();
            self.0.send(peer).unwrap();
            let fd: OwnedFd = lease.into();
            fd.into()
        }
    }
    #[tokio::test]
    #[ignore = "requires isolated dbus-run-session; never use the system bus"]
    async fn private_bus_daemon_lease_survives_connection_close_and_session_handoff() {
        unsafe extern "C" {
            fn sd_bus_open_user(bus: *mut *mut c_void) -> c_int;
        }
        let (leases, mut peers) = tokio::sync::mpsc::unbounded_channel();
        let _service = zbus::connection::Builder::session()
            .unwrap()
            .name("org.freedesktop.login1")
            .unwrap()
            .serve_at("/org/freedesktop/login1", Manager(leases))
            .unwrap()
            .build()
            .await
            .unwrap();
        let mut guard = LidGuard {
            lease: Some(
                tokio::task::spawn_blocking(|| acquire_on(sd_bus_open_user))
                    .await
                    .unwrap()
                    .unwrap(),
            ),
        };
        let mut peer = peers.recv().await.unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_millis(50)))
            .unwrap();
        assert!(
            peer.read(&mut [0]).is_err(),
            "closing sd-bus must not release the fd lease"
        );
        let descriptor = guard.fd().unwrap();
        assert_ne!(
            unsafe { libc::fcntl(descriptor, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        assert!(lease(&mut guard, true).is_none());
        assert!(lease(&mut guard, true).is_none());
        assert_eq!(guard.fd(), Some(descriptor));
        assert!(
            peers.try_recv().is_err(),
            "handoff must reuse the daemon lease"
        );
        lease(&mut guard, false);
        released(peer);
        let lease = tokio::task::spawn_blocking(|| acquire_on(sd_bus_open_user))
            .await
            .unwrap()
            .unwrap();
        let peer = peers.recv().await.unwrap();
        drop(lease);
        released(peer);
    }
}
