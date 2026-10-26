//! Initial PAM login only. Passwords never go to a named UID-only socket.
use crate::protocol::PamMessage;
use std::{
    ffi::{CStr, CString},
    io,
    os::unix::fs::MetadataExt,
    path::{Component, Path},
    time::{Duration, Instant},
};

#[derive(Debug)]
pub enum SocketError {
    Connect(io::Error),
    Send(io::Error),
    Timeout,
    InvalidSocket(String),
}
impl std::fmt::Display for SocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "Login activation failed: {e}"),
            Self::Send(e) => write!(f, "Private login pipe failed: {e}"),
            Self::Timeout => write!(f, "Protected login helper readiness timed out"),
            Self::InvalidSocket(s) => write!(f, "{s}"),
        }
    }
}
impl std::error::Error for SocketError {}

fn protected_helper(path: &Path) -> Result<(), SocketError> {
    if !path.is_absolute() {
        return Err(SocketError::InvalidSocket(
            "Helper path must be absolute".into(),
        ));
    }
    let mut current = std::path::PathBuf::from("/");
    let root = current.symlink_metadata().map_err(SocketError::Connect)?;
    if root.uid() != 0 || root.mode() & 0o022 != 0 {
        return Err(SocketError::InvalidSocket(
            "Unprotected installation root".into(),
        ));
    }
    for component in path.components().skip(1) {
        if !matches!(component, Component::Normal(_)) {
            return Err(SocketError::InvalidSocket("Invalid helper path".into()));
        }
        current.push(component);
        let metadata = current.symlink_metadata().map_err(SocketError::Connect)?;
        if metadata.uid() != 0
            || metadata.mode() & 0o022 != 0
            || metadata.file_type().is_symlink()
            || (current != path && !metadata.is_dir())
            || (current == path
                && (!metadata.is_file()
                    || metadata.mode() & 0o111 == 0
                    || metadata.mode() & 0o6000 != 0))
        {
            return Err(SocketError::InvalidSocket(
                "PAM auto-unlock requires a root-owned protected Clavis installation".into(),
            ));
        }
    }
    Ok(())
}

fn user_home(uid: u32) -> Result<String, SocketError> {
    let mut buffer = vec![0u8; 65536];
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() || entry.pw_dir.is_null() {
        return Err(SocketError::Connect(io::Error::new(
            io::ErrorKind::NotFound,
            "Authenticated user home is unavailable",
        )));
    }
    let home = unsafe { CStr::from_ptr(entry.pw_dir) }
        .to_str()
        .map_err(|_| SocketError::InvalidSocket("Invalid user home".into()))?
        .to_owned();
    if !Path::new(&home).is_absolute() {
        return Err(SocketError::InvalidSocket(
            "User home must be absolute".into(),
        ));
    }
    Ok(home)
}

struct SavedSignals {
    sigpipe: libc::sigaction,
    sigchld: libc::sigaction,
}
impl SavedSignals {
    fn new() -> Self {
        unsafe {
            let mut saved = Self {
                sigpipe: std::mem::zeroed(),
                sigchld: std::mem::zeroed(),
            };
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = libc::SIG_IGN;
            libc::sigaction(libc::SIGPIPE, &action, &mut saved.sigpipe);
            action.sa_sigaction = libc::SIG_DFL;
            libc::sigaction(libc::SIGCHLD, &action, &mut saved.sigchld);
            saved
        }
    }
}
impl Drop for SavedSignals {
    fn drop(&mut self) {
        unsafe {
            libc::sigaction(libc::SIGPIPE, &self.sigpipe, std::ptr::null_mut());
            libc::sigaction(libc::SIGCHLD, &self.sigchld, std::ptr::null_mut());
        }
    }
}

fn wait_pipe(fd: libc::c_int, events: libc::c_short, deadline: Instant) -> Result<(), SocketError> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(SocketError::Timeout);
        }
        let mut pollfd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        let result = unsafe {
            libc::poll(
                &mut pollfd,
                1,
                remaining.as_millis().max(1).min(5000) as i32,
            )
        };
        if result > 0 {
            return Ok(());
        }
        if result == 0 {
            return Err(SocketError::Timeout);
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(SocketError::Connect(io::Error::last_os_error()));
        }
    }
}

fn read_ack(fd: libc::c_int, expected: &[u8], deadline: Instant) -> Result<(), SocketError> {
    let mut response = vec![0u8; expected.len()];
    let mut offset = 0;
    while offset < expected.len() {
        wait_pipe(fd, libc::POLLIN, deadline)?;
        let count = unsafe {
            libc::read(
                fd,
                response[offset..].as_mut_ptr().cast(),
                expected.len() - offset,
            )
        };
        if count <= 0 {
            let error = io::Error::last_os_error();
            if count < 0
                && matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                )
            {
                continue;
            }
            return Err(SocketError::InvalidSocket(
                "Protected helper closed before readiness".into(),
            ));
        }
        offset += count as usize;
    }
    if response != expected {
        return Err(SocketError::InvalidSocket(
            "Invalid protected helper acknowledgment".into(),
        ));
    }
    Ok(())
}

/// Start a verified helper as the authenticated user with two private pipes.
/// The password pipe stays empty until the untraced, NotDumpable helper READY.
pub fn send_secret_to_daemon(
    message: PamMessage,
    uid: u32,
    gid: u32,
    auto_start: bool,
) -> Result<(), SocketError> {
    if !auto_start {
        return Err(SocketError::InvalidSocket("Repeated PAM unlock/password sync requires manual Clavis unlock; no password is sent to an existing socket".into()));
    }
    if message.new_secret.is_empty() || message.new_secret.len() > 65536 {
        return Err(SocketError::InvalidSocket(
            "A nonempty login password of at most 64 KiB is required".into(),
        ));
    }
    let path = option_env!("CLAVIS_LOGIN_PATH").unwrap_or("/usr/libexec/rixde/clavis-login");
    protected_helper(Path::new(path))?;
    let home = user_home(uid)?;
    let path = CString::new(path).unwrap();
    let ready_arg = CString::new("--pam-ready").unwrap();
    // Loader variables, bus addresses, credentials directories and caller XDG
    // overrides are deliberately absent from a root/PAM-generated environment.
    let environment = [
        format!("HOME={home}"),
        format!("USER={}", message.username),
        format!("LOGNAME={}", message.username),
        format!("XDG_RUNTIME_DIR=/run/user/{uid}"),
        "PATH=/usr/bin:/bin".into(),
        "LANG=C.UTF-8".into(),
    ]
    .into_iter()
    .map(|s| {
        CString::new(s).map_err(|_| SocketError::InvalidSocket("Invalid login environment".into()))
    })
    .collect::<Result<Vec<_>, _>>()?;
    let mut environment_ptrs = environment.iter().map(|s| s.as_ptr()).collect::<Vec<_>>();
    environment_ptrs.push(std::ptr::null());
    let mut password_pipe = [0; 2];
    let mut ready_pipe = [0; 2];
    if unsafe { libc::pipe2(password_pipe.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(SocketError::Connect(io::Error::last_os_error()));
    }
    if unsafe { libc::pipe2(ready_pipe.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        unsafe {
            libc::close(password_pipe[0]);
            libc::close(password_pipe[1]);
        }
        return Err(SocketError::Connect(io::Error::last_os_error()));
    }
    let saved = SavedSignals::new();
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            for fd in password_pipe.into_iter().chain(ready_pipe) {
                libc::close(fd);
            }
        }
        return Err(SocketError::Connect(io::Error::last_os_error()));
    }
    if pid == 0 {
        unsafe {
            if libc::dup2(password_pipe[0], 0) < 0 || libc::dup2(ready_pipe[1], 1) < 0 {
                libc::_exit(1);
            }
            if libc::getuid() != uid || libc::geteuid() != uid {
                if libc::setgroups(0, std::ptr::null()) < 0
                    || libc::setgid(gid) < 0
                    || libc::setuid(uid) < 0
                {
                    libc::_exit(1);
                }
            }
            // No unrelated DM/PAM descriptors reach the helper.
            if libc::close_range(3, u32::MAX, 0) != 0 {
                libc::_exit(1);
            }
            // Only the readiness acknowledgment is passed: `clavis-login`
            // never receives an activation bus from this caller.
            let arguments = [path.as_ptr(), ready_arg.as_ptr(), std::ptr::null()];
            libc::execve(path.as_ptr(), arguments.as_ptr(), environment_ptrs.as_ptr());
            libc::_exit(1);
        }
    }
    unsafe {
        libc::close(password_pipe[0]);
        libc::close(ready_pipe[1]);
    }
    let mut password_pipe_closed = false;
    let result = (|| {
        let deadline = Instant::now() + Duration::from_secs(5);
        for fd in [ready_pipe[0], password_pipe[1]] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(SocketError::Connect(io::Error::last_os_error()));
            }
        }
        read_ack(ready_pipe[0], b"CLAVIS-LOGIN-READY\n", deadline)?;
        let mut offset = 0;
        while offset < message.new_secret.len() {
            wait_pipe(password_pipe[1], libc::POLLOUT, deadline)?;
            let count = unsafe {
                libc::write(
                    password_pipe[1],
                    message.new_secret[offset..].as_ptr().cast(),
                    message.new_secret.len() - offset,
                )
            };
            if count <= 0 {
                let error = io::Error::last_os_error();
                if count < 0
                    && matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    )
                {
                    continue;
                }
                return Err(SocketError::Send(io::Error::last_os_error()));
            }
            offset += count as usize;
        }
        unsafe {
            libc::close(password_pipe[1]);
        }
        password_pipe_closed = true;
        read_ack(ready_pipe[0], b"CLAVIS-LOGIN-WAITING\n", deadline)?;
        Ok(())
    })();
    unsafe {
        if !password_pipe_closed {
            libc::close(password_pipe[1]);
        }
        libc::close(ready_pipe[0]);
    }
    if result.is_err() {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
    }
    drop(saved);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, os::fd::AsRawFd};
    #[test]
    fn fragmented_readiness_ack_is_reassembled_with_one_deadline() {
        let (receiver, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
        receiver.set_nonblocking(true).unwrap();
        let thread = std::thread::spawn(move || {
            writer.write_all(b"CLAVIS-LOGIN-").unwrap();
            std::thread::sleep(Duration::from_millis(5));
            writer.write_all(b"WAITING\n").unwrap();
        });
        read_ack(
            receiver.as_raw_fd(),
            b"CLAVIS-LOGIN-WAITING\n",
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        thread.join().unwrap();
    }
    #[test]
    fn partial_readiness_never_causes_an_unbounded_read() {
        let (receiver, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
        receiver.set_nonblocking(true).unwrap();
        writer.write_all(b"CLAVIS-").unwrap();
        assert!(matches!(
            read_ack(
                receiver.as_raw_fd(),
                b"CLAVIS-LOGIN-READY\n",
                Instant::now() + Duration::from_millis(20)
            ),
            Err(SocketError::Timeout)
        ));
    }
    #[test]
    fn repeated_pam_never_connects_to_uid_only_socket() {
        let message = PamMessage::unlock("fixture".into(), b"fixture-password-only".to_vec());
        assert!(
            send_secret_to_daemon(
                message,
                unsafe { libc::getuid() },
                unsafe { libc::getgid() },
                false
            )
            .is_err()
        );
    }
    #[test]
    fn writable_installation_cannot_receive_a_password() {
        let directory = tempfile::tempdir().unwrap();
        let helper = directory.path().join("clavis-login");
        std::fs::write(&helper, b"fixture").unwrap();
        assert!(protected_helper(&helper).is_err());
        assert!(protected_helper(Path::new("relative-helper")).is_err());
    }
}
