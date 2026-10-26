//! Login activation protocol. Named sockets carry addresses/lifecycle only.
use std::{
    io::{self, Read, Write},
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Component, Path, PathBuf},
    time::Duration,
};

pub const READY: &[u8] = b"CLAVIS-READY\n";
pub const RUNNING: &[u8] = b"CLAVIS-RUNNING\n";

fn denied(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

pub fn require_untraced() -> io::Result<()> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let tracer = status
        .lines()
        .find_map(|line| line.strip_prefix("TracerPid:"))
        .ok_or_else(|| denied("Cannot verify that Clavis is untraced"))?;
    if tracer.trim() != "0" {
        return Err(denied(
            "Clavis refuses to receive a login password while traced",
        ));
    }
    Ok(())
}

pub fn private_directory(path: &Path, uid: u32) -> io::Result<()> {
    let metadata = path.symlink_metadata()?;
    if !path.is_absolute()
        || !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != uid
        || metadata.mode() & 0o077 != 0
    {
        return Err(denied(
            "Login runtime directory must be private and owned by this user",
        ));
    }
    Ok(())
}

/// Only a single local, owned Unix socket below the private runtime is accepted.
pub fn validate_bus(address: &str, runtime: &Path, uid: u32) -> io::Result<PathBuf> {
    private_directory(runtime, uid)?;
    let body = address
        .strip_prefix("unix:path=")
        .ok_or_else(|| denied("Login activation requires a local Unix path bus"))?;
    let mut parts = body.split(',');
    let path = parts.next().unwrap();
    if path.is_empty()
        || path
            .bytes()
            .any(|b| b <= 32 || matches!(b, b'%' | b';' | b'='))
    {
        return Err(denied("Invalid Unix bus path"));
    }
    if let Some(guid) = parts.next() {
        let guid = guid
            .strip_prefix("guid=")
            .ok_or_else(|| denied("Unknown bus address option"))?;
        if guid.len() != 32
            || !guid.bytes().all(|b| b.is_ascii_hexdigit())
            || parts.next().is_some()
        {
            return Err(denied("Invalid Unix bus guid"));
        }
    }
    let path = PathBuf::from(path);
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(denied("Invalid Unix bus path"));
    }
    let relative = path
        .strip_prefix(runtime)
        .map_err(|_| denied("Bus must be inside this user's private runtime"))?;
    let mut parent = runtime.to_owned();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        if !matches!(component, Component::Normal(_)) {
            return Err(denied("Invalid Unix bus component"));
        }
        parent.push(component);
        if components.peek().is_some() {
            private_directory(&parent, uid)?;
        }
    }
    let metadata = path.symlink_metadata()?;
    if !metadata.file_type().is_socket() || metadata.uid() != uid {
        return Err(denied("Bus socket must be real and owned by this user"));
    }
    Ok(path)
}

/// Every component must be controlled by root, with no writable/symlink hops.
#[allow(dead_code)] // Shared by daemon and helper bins; only the helper executes it.
pub fn protected_executable(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(denied("Installed Clavis daemon path must be absolute"));
    }
    let mut current = PathBuf::from("/");
    let root = current.symlink_metadata()?;
    if root.uid() != 0 || root.mode() & 0o022 != 0 {
        return Err(denied("Installed daemon root directory is not protected"));
    }
    for component in path.components().skip(1) {
        if !matches!(component, Component::Normal(_)) {
            return Err(denied("Invalid installed daemon path"));
        }
        current.push(component);
        let metadata = current.symlink_metadata()?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 || metadata.file_type().is_symlink()
        {
            return Err(denied(
                "Automatic login requires a root-owned protected Clavis installation",
            ));
        }
        if current != path && !metadata.is_dir() {
            return Err(denied("Invalid installed daemon directory"));
        }
        if current == path
            && (!metadata.is_file()
                || metadata.mode() & 0o111 == 0
                || metadata.mode() & 0o6000 != 0)
        {
            return Err(denied("Installed daemon must be an ordinary executable"));
        }
    }
    Ok(())
}

pub fn read_line(stream: &mut impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut result = Vec::new();
    let mut byte = [0u8; 1];
    while result.len() < limit {
        if stream.read(&mut byte)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Login activation closed",
            ));
        }
        result.push(byte[0]);
        if byte[0] == b'\n' {
            return Ok(result);
        }
    }
    Err(denied("Login activation message exceeds limit"))
}

#[allow(dead_code)] // Shared by daemon and helper bins; only the controller calls it.
pub fn request_start(
    socket: &Path,
    address: &str,
) -> io::Result<Option<std::os::unix::net::UnixStream>> {
    let uid = rustix::process::getuid().as_raw();
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| denied("No private runtime directory"))?;
    let metadata = match socket.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    private_directory(
        socket
            .parent()
            .ok_or_else(|| denied("Invalid login socket"))?,
        uid,
    )?;
    if !metadata.file_type().is_socket() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(denied("Unsafe login activation socket"));
    }
    validate_bus(address, &runtime, uid)?;
    let mut stream = match std::os::unix::net::UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => return Ok(None),
        Err(error) => return Err(error),
    };
    if rustix::net::sockopt::socket_peercred(&stream)?.uid.as_raw() != uid {
        return Err(denied("Login activation rejected another UID"));
    }
    stream.set_read_timeout(Some(Duration::from_secs(25)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    // The helper pins the canonical bus itself; this address is only a
    // cross-check and can never select or replace the helper's choice.
    writeln!(stream, "{address}")?;
    let response = read_line(&mut stream, 80)?;
    let Some(pid) = response
        .strip_prefix(b"CLAVIS-SPAWNED ")
        .and_then(|p| p.strip_suffix(b"\n"))
    else {
        return Err(denied("Login helper rejected activation"));
    };
    if std::str::from_utf8(pid)
        .ok()
        .and_then(|p| p.parse::<u32>().ok())
        .is_none_or(|pid| pid == 0)
    {
        return Err(denied("Invalid login child acknowledgment"));
    }
    // PID is informational only: lifecycle is tied to this connection, never
    // to a peer-supplied PID which could name an unrelated user process.
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    Ok(Some(stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn activation_addresses_are_local_owned_and_private() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path();
        let uid = rustix::process::getuid().as_raw();
        let socket = runtime.join("bus");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let address = format!("unix:path={}", socket.display());
        assert_eq!(validate_bus(&address, runtime, uid).unwrap(), socket);
        for invalid in [
            "tcp:host=localhost,port=1",
            "unix:abstract=fake",
            "unix:path=/tmp/outside",
            "unix:path=/tmp/a;unix:path=/tmp/b",
            "unix:path=/tmp/a,guid=bad",
        ] {
            assert!(validate_bus(invalid, runtime, uid).is_err());
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(validate_bus(&address, runtime, uid).is_err());
    }
    #[test]
    fn writable_user_binary_cannot_receive_a_login_password() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("clavis-daemon");
        std::fs::write(&binary, b"fixture").unwrap();
        assert!(protected_executable(&binary).is_err());
        assert!(protected_executable(Path::new("relative/daemon")).is_err());
    }
    #[test]
    fn protocol_lines_are_bounded_and_cannot_have_missing_terminators() {
        assert_eq!(read_line(&mut &b"READY\nmore"[..], 10).unwrap(), b"READY\n");
        assert!(read_line(&mut &b"too-long"[..], 3).is_err());
        assert!(read_line(&mut &b"missing-newline"[..], 20).is_err());
    }
}
