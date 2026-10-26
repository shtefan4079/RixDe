use std::{
    os::unix::{
        fs::{FileTypeExt, MetadataExt},
        net::UnixDatagram,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
        Arc,
    },
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Panel,
    Area,
}

struct Launch {
    mode: Mode,
    display: String,
    forward_only: bool,
}

#[derive(Default)]
pub(crate) struct Launcher {
    sender: Option<SyncSender<Launch>>,
    active: Arc<AtomicBool>,
    #[cfg(test)]
    binary: Option<PathBuf>,
}

impl Launcher {

    pub(crate) fn request(&mut self, mode: Mode, display: &str) -> bool {
        if self
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {

            if let Some(sender) = &self.sender {
                let _ = sender.try_send(Launch {
                    mode,
                    display: display.into(),
                    forward_only: true,
                });
            }
            return false;
        }
        if self.sender.is_none() {
            let (sender, receiver) = mpsc::sync_channel(1);
            let active = Arc::clone(&self.active);
            #[cfg(test)]
            let binary = self.binary.clone();
            #[cfg(not(test))]
            let binary = None;
            match std::thread::Builder::new()
                .name("rix-capture-launch".into())
                .spawn(move || supervise(receiver, active, binary))
            {
                Ok(_) => self.sender = Some(sender),
                Err(error) => {
                    self.active.store(false, Ordering::Release);
                    tracing::warn!(%error, "Could not start capture UI launcher");
                    return false;
                }
            }
        }
        let request = Launch {
            mode,
            display: display.into(),
            forward_only: false,
        };
        if let Err(error) = self.sender.as_ref().unwrap().try_send(request) {
            self.active.store(false, Ordering::Release);
            tracing::warn!(%error, "Capture UI launcher is unavailable");
            return false;
        }
        true
    }
}

fn request_editor_capture(pid: u32, mode: Mode) -> std::io::Result<()> {
    let directory = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "Runtime directory is missing")
    })?;
    send_editor_request(
        &PathBuf::from(directory).join(format!("rix-capture-{pid}.sock")),
        mode,
    )
}

fn send_editor_request(path: &Path, mode: Mode) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Capture control socket has an invalid owner or type",
        ));
    }
    let socket = UnixDatagram::unbound()?;
    socket.set_nonblocking(true)?;
    socket.send_to(
        if mode == Mode::Area {
            b"area"
        } else {
            b"panel"
        },
        path,
    )?;
    Ok(())
}

fn sibling_binary(executable: &Path) -> PathBuf {
    executable.with_file_name("rix-capture")
}

fn start(request: Launch, binary: Option<&Path>) -> std::io::Result<Child> {
    let binary = match binary {
        Some(binary) => binary.to_owned(),
        None => sibling_binary(&std::env::current_exe()?),
    };
    let mut command = Command::new(binary);
    command
        .env("WAYLAND_DISPLAY", request.display)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .stdin(Stdio::null());
    if request.mode == Mode::Area {
        command.arg("--area");
    }
    rix_services::resources::ordinary_command(&mut command);
    command.spawn()
}

fn supervise(receiver: Receiver<Launch>, active: Arc<AtomicBool>, binary: Option<PathBuf>) {
    let mut child: Option<Child> = None;
    let mut reap_error_reported = false;
    loop {
        if let Some(process) = child.as_mut() {
            match process.try_wait() {
                Ok(Some(status)) => {
                    tracing::debug!(%status, "Capture UI exited");
                    child = None;
                    reap_error_reported = false;
                    active.store(false, Ordering::Release);
                }
                Ok(None) => {}
                Err(error) => {
                    if !reap_error_reported {
                        tracing::warn!(%error, "Could not reap capture UI");
                        reap_error_reported = true;
                    }

                }
            }
        }
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(request) => {
                if let Some(process) = &child {

                    let _ = request_editor_capture(process.id(), request.mode);
                    continue;
                }

                if request.forward_only {
                    continue;
                }
                match start(request, binary.as_deref()) {
                    Ok(process) => {
                        tracing::info!(pid = process.id(), "Started capture UI");
                        child = Some(process);
                    }
                    Err(error) => {
                        active.store(false, Ordering::Release);
                        tracing::warn!(%error, "Could not start capture UI; rebuild and install rix-capture");
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {

                if let Some(mut child) = child {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                active.store(false, Ordering::Release);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, time::Instant};

    fn wait_for(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "Capture launcher did not complete"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn binary_is_a_sibling_for_installed_and_development_compositors() {
        for directory in [
            "/usr/libexec/rixde",
            "/opt/rix/target/debug",
            "/opt/rix/target/release",
        ] {
            assert_eq!(
                sibling_binary(&Path::new(directory).join("rix-comp")),
                Path::new(directory).join("rix-capture")
            );
        }
    }

    #[test]
    fn repeated_requests_do_not_duplicate_helper_and_next_launch_receives_area_flag() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("capture-helper");
        let log = directory.path().join("launches");
        let finish = directory.path().join("finish");
        fs::write(
            &binary,
            format!(
                "#!/bin/sh\nprintf '%s|%s|%s|%s\\n' \"$WAYLAND_DISPLAY\" \"${{DISPLAY-unset}}\" \"${{WAYLAND_SOCKET-unset}}\" \"$*\" >> '{}'\nwhile [ ! -f '{}' ]; do sleep .02; done\n",
                log.display(), finish.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        let mut launcher = Launcher {
            binary: Some(binary),
            ..Default::default()
        };
        assert!(launcher.request(Mode::Panel, "wayland-capture-test"));
        for _ in 0..20 {
            assert!(!launcher.request(Mode::Area, "wrong-display"));
        }
        wait_for(|| log.exists());
        assert_eq!(
            fs::read_to_string(&log).unwrap(),
            "wayland-capture-test|unset|unset|\n"
        );
        fs::write(finish, "").unwrap();
        wait_for(|| !launcher.active.load(Ordering::Acquire));
        assert!(launcher.request(Mode::Area, "wayland-capture-test"));
        wait_for(|| {
            fs::read_to_string(&log).is_ok_and(|text| text.lines().count() == 2)
                && !launcher.active.load(Ordering::Acquire)
        });
        assert_eq!(
            fs::read_to_string(&log).unwrap().lines().last().unwrap(),
            "wayland-capture-test|unset|unset|--area"
        );
    }

    #[test]
    fn failed_spawn_releases_the_single_instance_guard() {
        let directory = tempfile::tempdir().unwrap();
        let mut launcher = Launcher {
            binary: Some(directory.path().join("missing-capture-helper")),
            ..Default::default()
        };
        assert!(launcher.request(Mode::Panel, "wayland-capture-test"));
        wait_for(|| !launcher.active.load(Ordering::Acquire));
        assert!(launcher.request(Mode::Area, "wayland-capture-test"));
        wait_for(|| !launcher.active.load(Ordering::Acquire));
    }

    #[test]
    fn stale_editor_notifications_do_not_start_a_helper_or_release_a_new_launch_guard() {
        let directory = tempfile::tempdir().unwrap();

        let active = Arc::new(AtomicBool::new(true));
        let worker_active = active.clone();
        let (sender, receiver) = mpsc::sync_channel(0);
        let missing = directory.path().join("missing-helper");
        let worker = std::thread::spawn(move || supervise(receiver, worker_active, Some(missing)));
        for _ in 0..2 {
            sender
                .send(Launch {
                    mode: Mode::Area,
                    display: "fixture-display".into(),
                    forward_only: true,
                })
                .unwrap();
        }

        assert!(active.load(Ordering::Acquire));
        drop(sender);
        worker.join().unwrap();
    }

    #[test]
    fn open_editor_receives_new_capture_modes_and_invalid_socket_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.sock");
        let receiver = UnixDatagram::bind(&path).unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut message = [0; 8];
        send_editor_request(&path, Mode::Area).unwrap();
        let length = receiver.recv(&mut message).unwrap();
        assert_eq!(&message[..length], b"area");
        send_editor_request(&path, Mode::Panel).unwrap();
        let length = receiver.recv(&mut message).unwrap();
        assert_eq!(&message[..length], b"panel");
        let regular = directory.path().join("not-a-socket");
        fs::write(&regular, "").unwrap();
        assert_eq!(
            send_editor_request(&regular, Mode::Area)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }
}
