use std::{
    fs, io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            ffi::OsStrExt,
            fs::{FileTypeExt, MetadataExt},
            net::UnixStream,
        },
    },
    path::Path,
    time::{Duration, Instant},
};

pub fn has_compositor(uid: u32) -> bool {
    live_socket(Path::new(&format!("/run/user/{uid}/rix-ipc.sock")), uid)
}

fn live_socket(path: &Path, uid: u32) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.uid() != uid || !metadata.file_type().is_socket() {
        return false;
    }

    connect_before(path, Duration::from_millis(200))
        .and_then(|stream| rix_dm::peer_credentials(&stream))
        .is_ok_and(|peer| peer.uid == uid && peer.pid > 0)
}

fn connect_before(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty() || bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid socket path",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = *byte as libc::c_char;
    }
    let descriptor = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
    let length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    let result = unsafe {
        libc::connect(
            descriptor.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            length,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Socket probe timed out",
                ));
            }
            let mut poll = libc::pollfd {
                fd: descriptor.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            let result = unsafe {
                libc::poll(
                    &mut poll,
                    1,
                    remaining.as_millis().clamp(1, i32::MAX as u128) as i32,
                )
            };
            if result < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            if result == 0 {
                continue;
            }
            let mut error = 0i32;
            let mut length = std::mem::size_of::<i32>() as libc::socklen_t;
            if unsafe {
                libc::getsockopt(
                    descriptor.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    (&mut error as *mut i32).cast(),
                    &mut length,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error));
            }
            break;
        }
    }
    Ok(UnixStream::from(descriptor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn detects_a_live_owned_socket_without_stopping_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ipc.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let uid = unsafe { libc::geteuid() };
        assert!(live_socket(&path, uid));
        let (stream, _) = listener.accept().unwrap();
        drop(stream);
        assert!(
            live_socket(&path, uid),
            "The original listener must remain alive"
        );
        assert!(path.exists());
    }

    #[test]
    fn missing_and_stale_sockets_do_not_block_login_or_get_removed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ipc.sock");
        let uid = unsafe { libc::geteuid() };
        assert!(!live_socket(&path, uid));
        drop(UnixListener::bind(&path).unwrap());
        assert!(!live_socket(&path, uid));
        assert!(path.exists(), "The probe must not remove any socket");
    }

    #[test]
    fn wrong_owner_regular_files_and_symlinks_are_not_live_sessions() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("ipc.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        let uid = unsafe { libc::geteuid() };
        assert!(!live_socket(&socket, uid.wrapping_add(1)));
        let file = directory.path().join("file");
        fs::write(&file, "unchanged").unwrap();
        assert!(!live_socket(&file, uid));
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&socket, &link).unwrap();
        assert!(!live_socket(&link, uid));
        assert_eq!(fs::read_to_string(file).unwrap(), "unchanged");
    }

    #[test]
    fn a_full_listen_queue_cannot_stall_the_authentication_worker() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ipc.sock");
        let listener = UnixListener::bind(&path).unwrap();

        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 1) }, 0);
        let _one = UnixStream::connect(&path).unwrap();
        let _two = UnixStream::connect(&path).unwrap();
        let started = Instant::now();
        assert!(!live_socket(&path, unsafe { libc::geteuid() }));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
