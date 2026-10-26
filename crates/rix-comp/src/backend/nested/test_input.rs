use crate::state::RixState;
use serde_json::{json, Value};
use smithay::{
    backend::input::{ButtonState, KeyState},
    input::pointer::MotionEvent,
    utils::SERIAL_COUNTER,
};
use std::{
    io::{self, Read, Write},
    os::unix::{fs::MetadataExt, fs::PermissionsExt, net::UnixListener, net::UnixStream},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const MAX_BYTES: usize = 6000;
const MAX_CONNECTIONS: usize = 4;
const DEADLINE: Duration = Duration::from_secs(2);
struct Connection {
    stream: UnixStream,
    bytes: Vec<u8>,
    started: Instant,
}
pub(super) struct TestInput {
    listener: UnixListener,
    path: PathBuf,
    inode: u64,
    connections: Vec<Connection>,
    started: Instant,
}

fn private_directory(path: &Path) -> io::Result<PathBuf> {
    let canonical = path.canonicalize()?;
    let metadata = std::fs::metadata(&canonical)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Fixture directory must be owned by this user with mode 0700",
        ));
    }
    Ok(canonical)
}
fn validate_path(
    socket: &Path,
    fixture: &Path,
    home: &Path,
    runtime: &Path,
) -> io::Result<PathBuf> {
    let fixture = private_directory(fixture)?;
    let home = private_directory(home)?;
    let runtime = private_directory(runtime)?;
    if home != fixture.join("home")
        || runtime != fixture.join("runtime")
        || !socket.is_absolute()
        || socket.parent().and_then(|p| p.canonicalize().ok()).as_ref() != Some(&runtime)
        || socket.file_name().is_none()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Test input requires the disposable fixture HOME and runtime",
        ));
    }
    Ok(runtime.join(socket.file_name().unwrap()))
}
impl TestInput {
    pub(super) fn from_environment() -> anyhow::Result<Option<Self>> {
        let Some(path) = std::env::var_os("RIX_NESTED_TEST_INPUT_SOCKET") else {
            return Ok(None);
        };
        let required = |name| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .ok_or_else(|| anyhow::anyhow!("Test input requires {name}"))
        };
        let path = validate_path(
            &PathBuf::from(path),
            &required("RIX_FILES_PRIVATE_FIXTURE")?,
            &required("HOME")?,
            &required("XDG_RUNTIME_DIR")?,
        )?;
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let inode = std::fs::metadata(&path)?.ino();
        Ok(Some(Self {
            listener,
            path,
            inode,
            connections: Vec::new(),
            started: Instant::now(),
        }))
    }
    pub(super) fn poll(&mut self, state: &mut RixState) {
        for _ in 0..MAX_CONNECTIONS {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if self.connections.len() < MAX_CONNECTIONS
                        && stream.set_nonblocking(true).is_ok()
                    {
                        self.connections.push(Connection {
                            stream,
                            bytes: Vec::new(),
                            started: Instant::now(),
                        });
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        let time = self.started.elapsed().as_millis() as u32;
        self.connections.retain_mut(|connection| {
            let mut chunk = [0u8; 1024];
            let request = loop {
                match connection.stream.read(&mut chunk) {
                    Ok(0) => return false,
                    Ok(count) => {
                        connection.bytes.extend_from_slice(&chunk[..count]);
                        if connection.bytes.len() > MAX_BYTES {
                            break Err("Test input request exceeds 6000 bytes".to_owned());
                        }
                        if let Some(end) = connection.bytes.iter().position(|b| *b == b'\n') {
                            break serde_json::from_slice::<Value>(&connection.bytes[..end])
                                .map_err(|e| e.to_string());
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if connection.started.elapsed() >= DEADLINE {
                            break Err("Test input request timed out".to_owned());
                        }
                        return true;
                    }
                    Err(_) => return false,
                }
            };
            let result = request.and_then(|request| dispatch(state, request, time));
            let response = result.unwrap_or_else(|error| json!({"ok":false,"error":error}));
            if let Ok(mut encoded) = serde_json::to_vec(&response) {
                encoded.push(b'\n');
                let _ = connection.stream.write_all(&encoded);
            }
            false
        });
    }
}
impl Drop for TestInput {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path).is_ok_and(|metadata| metadata.ino() == self.inode)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
fn dispatch(state: &mut RixState, request: Value, time: u32) -> Result<Value, String> {
    if state.lock.active {
        return Err("Test input is unavailable while the fixture is locked".into());
    }
    let serial = SERIAL_COUNTER.next_serial();
    match request.get("type").and_then(Value::as_str) {
        Some("windows") => {
            let windows: Vec<_> = state.windows.entries.iter().take(4).filter_map(|(id, window)| {
                let geometry = state.space.element_geometry(window)?;
                let info = state.list_windows().into_iter().find(|w| w.id == *id)?;
                Some(json!({"id":id,"app_id":info.app_id.chars().take(64).collect::<String>(),"title":info.title.chars().take(64).collect::<String>(),"x":geometry.loc.x,"y":geometry.loc.y,"width":geometry.size.w,"height":geometry.size.h}))
            }).collect();
            return Ok(json!({"ok":true,"windows":windows}));
        }
        Some("motion") => {
            let number = |key| {
                request
                    .get(key)
                    .and_then(Value::as_f64)
                    .filter(|n| n.is_finite() && n.abs() < 100_000.)
                    .ok_or_else(|| format!("Invalid {key} coordinate"))
            };
            let location = state.constrain_pointer_location((number("x")?, number("y")?).into());
            let pointer = state
                .seat
                .get_pointer()
                .ok_or("Fixture pointer is unavailable")?;
            pointer.motion(
                state,
                state.surface_under(location),
                &MotionEvent {
                    location,
                    serial,
                    time,
                },
            );
            state.update_pointer_constraint();
            pointer.frame(state);
        }
        Some(kind @ ("button" | "key")) => {
            let code = request
                .get("code")
                .and_then(Value::as_u64)
                .filter(|n| *n <= 767)
                .ok_or("Invalid evdev code")? as u32;
            let pressed = request
                .get("pressed")
                .and_then(Value::as_bool)
                .ok_or("Missing pressed state")?;
            if kind == "button" {
                if !(272..=274).contains(&code) {
                    return Err("Unsupported pointer button".into());
                }
                state.handle_pointer_button(
                    code,
                    if pressed {
                        ButtonState::Pressed
                    } else {
                        ButtonState::Released
                    },
                    serial,
                    time,
                );
            } else {
                if code == 0 {
                    return Err("Invalid evdev key".into());
                }
                state.handle_keyboard_key(
                    (code + 8).into(),
                    if pressed {
                        KeyState::Pressed
                    } else {
                        KeyState::Released
                    },
                    serial,
                    time,
                    true,
                );
            }
        }
        _ => return Err("Unknown test input request".into()),
    }
    state
        .display_handle
        .flush_clients()
        .map_err(|error| error.to_string())?;
    let pointer = state.seat.get_pointer().map(|pointer| {
        let position = pointer.current_location();
        [position.x, position.y]
    });
    Ok(json!({"ok":true,"pointer":pointer}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_socket_requires_the_private_fixture_home_and_runtime() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let home = root.path().join("home");
        let runtime = root.path().join("runtime");
        for path in [&home, &runtime] {
            std::fs::create_dir(path).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert!(validate_path(&runtime.join("input.sock"), root.path(), &home, &runtime).is_ok());
        assert!(validate_path(
            &root.path().join("input.sock"),
            root.path(),
            &home,
            &runtime
        )
        .is_err());
        assert!(validate_path(
            &runtime.join("input.sock"),
            root.path(),
            root.path(),
            &runtime
        )
        .is_err());
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(validate_path(&runtime.join("input.sock"), root.path(), &home, &runtime).is_err());
    }
}
