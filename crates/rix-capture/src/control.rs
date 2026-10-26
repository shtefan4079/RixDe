use anyhow::{Context, Result};
use std::{
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::UnixDatagram,
    },
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

pub struct Listener {
    path: PathBuf,
    inode: u64,
    running: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

fn is_request(bytes: &[u8]) -> bool {
    bytes == b"area" || bytes == b"panel"
}

pub fn install(weak: slint::Weak<super::CaptureWindow>) -> Result<Listener> {
    let directory = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("XDG_RUNTIME_DIR is missing")?;
    let metadata = std::fs::symlink_metadata(&directory)?;

    let uid = unsafe { libc::geteuid() };
    anyhow::ensure!(
        metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0,
        "Capture runtime directory must be private and owned by this user"
    );
    let path = directory.join(format!("rix-capture-{}.sock", std::process::id()));
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        anyhow::ensure!(
            metadata.file_type().is_socket() && metadata.uid() == uid,
            "Capture control path is occupied"
        );

        std::fs::remove_file(&path)?;
    }
    let socket = UnixDatagram::bind(&path)?;
    let inode = std::fs::symlink_metadata(&path)?.ino();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    let running = Arc::new(AtomicBool::new(true));
    let active = running.clone();
    let thread = std::thread::spawn(move || {
        let mut bytes = [0; 16];
        while active.load(Ordering::Acquire) {
            match socket.recv(&mut bytes) {
                Ok(size) if is_request(&bytes[..size]) => {
                    if weak
                        .upgrade_in_event_loop(|ui| ui.invoke_new_capture())
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => break,
            }
        }
    });
    Ok(Listener {
        path,
        inode,
        running,
        thread: Some(thread),
    })
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.file_type().is_socket() && metadata.ino() == self.inode)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_only_the_two_bounded_requests() {
        assert!(is_request(b"area"));
        assert!(is_request(b"panel"));
        for bytes in [
            b"".as_slice(),
            b"AREA",
            b"area\n",
            b"panel-extra",
            b"ar",
            b"area\0",
        ] {
            assert!(!is_request(bytes));
        }
    }
}
