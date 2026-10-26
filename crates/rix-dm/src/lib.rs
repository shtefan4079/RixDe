pub use rix_auth::SecretString;
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Read, Write},
    os::fd::AsRawFd,
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};

pub const DEFAULT_SOCKET: &str = "/run/rix-dm/control.sock";
pub const MAX_FRAME: usize = 16 * 1024;
pub const MAX_PASSWORD: usize = 4096;

#[derive(Debug, Serialize)]
#[serde(tag = "request", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    GetCatalog,
    Login {
        username: String,
        password: SecretString,
        session: String,
    },
    Power {
        action: PowerAction,
    },
}
impl<'de> Deserialize<'de> for Request {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {

        #[derive(Deserialize)]
        #[serde(tag = "request", rename_all = "snake_case", deny_unknown_fields)]
        enum WireRequest {
            GetCatalog {},
            Login {
                username: String,
                password: SecretString,
                session: String,
            },
            Power {
                action: PowerAction,
            },
        }
        Ok(match WireRequest::deserialize(deserializer)? {
            WireRequest::GetCatalog {} => Self::GetCatalog,
            WireRequest::Login {
                username,
                password,
                session,
            } => Self::Login {
                username,
                password,
                session,
            },
            WireRequest::Power { action } => Self::Power { action },
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerAction {
    PowerOff,
    Reboot,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub username: String,
    pub display_name: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub name: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    pub users: Vec<User>,
    pub sessions: Vec<Session>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginError {
    AuthenticationFailed,
    PasswordExpired,
    InvalidSession,
    RateLimited,
    SessionAlreadyRunning,
    SessionFailed,
    InvalidRequest,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", content = "data", rename_all = "snake_case")]
pub enum Reply {
    Catalog(Catalog),
    Authenticated,
    Rejected {
        reason: LoginError,
        retry_after_secs: u64,
    },
    Ok,
}

pub struct Client(UnixStream);
impl Client {
    pub fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;

        if peer_credentials(&stream)?.uid != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Display manager is not root",
            ));
        }
        stream.set_read_timeout(Some(Duration::from_secs(120)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(Self(stream))
    }
    pub fn request(&mut self, request: Request) -> io::Result<Reply> {
        send(&mut self.0, &request)?;
        receive(&mut self.0)
    }
}
pub fn socket_path() -> std::path::PathBuf {
    std::env::var_os("RIX_DM_SOCKET")
        .map(Into::into)
        .unwrap_or_else(|| DEFAULT_SOCKET.into())
}

struct Frame(Vec<u8>);
impl Drop for Frame {
    fn drop(&mut self) {
        rix_auth::wipe(&mut self.0);
    }
}
pub fn send<T: Serialize>(stream: &mut impl Write, value: &T) -> io::Result<()> {
    let frame = Frame(serde_json::to_vec(value).map_err(|_| invalid_frame())?);
    if frame.0.is_empty() || frame.0.len() > MAX_FRAME {
        return Err(invalid_frame());
    }
    stream.write_all(&(frame.0.len() as u32).to_be_bytes())?;
    stream.write_all(&frame.0)?;
    stream.flush()
}
pub fn receive<T: for<'de> Deserialize<'de>>(stream: &mut impl Read) -> io::Result<T> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header)?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(invalid_frame());
    }
    let mut frame = Frame(vec![0u8; length]);
    stream.read_exact(&mut frame.0)?;

    serde_json::from_slice(&frame.0).map_err(|_| invalid_frame())
}

pub fn receive_before<T: for<'de> Deserialize<'de>>(
    stream: &mut UnixStream,
    deadline: Instant,
) -> io::Result<T> {
    struct Reader<'a> {
        stream: &'a mut UnixStream,
        deadline: Instant,
    }
    impl Read for Reader<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            let remaining = self
                .deadline
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or_else(request_timeout)?;
            self.stream.set_read_timeout(Some(remaining))?;
            match self.stream.read(bytes) {
                Ok(_) if Instant::now() >= self.deadline => Err(request_timeout()),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    Err(request_timeout())
                }
                result => result,
            }
        }
    }
    let previous = stream.read_timeout()?;
    let result = receive(&mut Reader { stream, deadline });
    let restored = stream.set_read_timeout(previous);
    match (result, restored) {
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
    }
}

pub fn receive_with_deadline<T: for<'de> Deserialize<'de>>(
    stream: &mut UnixStream,
    timeout: Duration,
) -> io::Result<T> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(invalid_frame)?;
    receive_before(stream, deadline)
}
fn request_timeout() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "Display-manager request timed out")
}
fn invalid_frame() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "Invalid display-manager frame")
}

#[derive(Clone, Copy, Debug)]
pub struct PeerCredentials {
    pub uid: u32,
    pub gid: u32,
    pub pid: i32,
}
pub fn peer_credentials(stream: &UnixStream) -> io::Result<PeerCredentials> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if length as usize != std::mem::size_of::<libc::ucred>() {
        return Err(invalid_frame());
    }
    Ok(PeerCredentials {
        uid: credentials.uid,
        gid: credentials.gid,
        pid: credentials.pid,
    })
}

pub fn authorize_peer(credentials: PeerCredentials, greeter_uid: u32, supervisor_pid: i32) -> bool {
    credentials.uid == greeter_uid
        && greeter_uid != 0
        && credentials.pid > 0
        && unsafe { libc::getsid(credentials.pid) } == supervisor_pid
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_frames_reject_oversize_before_allocating() {
        assert!(
            receive::<Request>(&mut ((MAX_FRAME as u32 + 1).to_be_bytes().as_slice())).is_err()
        );
        assert!(receive::<Request>(&mut 0u32.to_be_bytes().as_slice()).is_err());
    }
    #[test]
    fn credentials_never_enter_diagnostics_or_replies() {
        let request = Request::Login {
            username: "alice".into(),
            password: SecretString::new("unique-password".into()),
            session: "rix".into(),
        };
        assert!(!format!("{request:?}").contains("unique-password"));
        let mut bytes = Vec::new();
        send(&mut bytes, &request).unwrap();
        let mut decoded = receive::<Request>(&mut bytes.as_slice()).unwrap();
        if let Request::Login { password, .. } = &mut decoded {
            assert_eq!(password.expose(), "unique-password");
        }
        let mut malformed = Vec::new();
        send(
            &mut malformed,
            &serde_json::json!({"request":"unique-password"}),
        )
        .unwrap();
        assert!(!receive::<Request>(&mut malformed.as_slice())
            .unwrap_err()
            .to_string()
            .contains("unique-password"));
        assert!(!serde_json::to_string(&Reply::Rejected {
            reason: LoginError::AuthenticationFailed,
            retry_after_secs: 2
        })
        .unwrap()
        .contains("unique-password"));
        rix_auth::wipe(&mut bytes);
    }
    #[test]
    fn same_uid_outside_greeter_session_and_root_are_denied() {
        let (one, _) = UnixStream::pair().unwrap();
        let peer = peer_credentials(&one).unwrap();
        assert!(!authorize_peer(peer, peer.uid.wrapping_add(1), unsafe {
            libc::getsid(0)
        }));
        assert!(!authorize_peer(peer, peer.uid, -1));
        assert!(!authorize_peer(
            PeerCredentials {
                uid: 0,
                gid: 0,
                pid: std::process::id() as i32
            },
            0,
            unsafe { libc::getsid(0) }
        ));
    }
    #[test]
    fn unknown_fields_and_commands_are_rejected() {
        let mut bytes = Vec::new();
        send(
            &mut bytes,
            &serde_json::json!({"request":"get_catalog", "command":"/bin/sh"}),
        )
        .unwrap();
        assert!(receive::<Request>(&mut bytes.as_slice()).is_err());
    }
    #[test]
    fn a_partial_frame_has_one_absolute_deadline() {
        let (mut one, mut two) = UnixStream::pair().unwrap();
        two.write_all(&4u32.to_be_bytes()).unwrap();
        two.write_all(b"{").unwrap();
        let started = Instant::now();
        assert!(receive_before::<Request>(&mut one, started + Duration::from_millis(40)).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn trickling_bytes_cannot_extend_the_frame_deadline() {
        let (mut one, mut two) = UnixStream::pair().unwrap();
        one.set_read_timeout(Some(Duration::from_secs(7))).unwrap();
        let original = one.read_timeout().unwrap();
        let mut bytes = Vec::new();
        send(&mut bytes, &Request::GetCatalog).unwrap();
        let sender = std::thread::spawn(move || {
            for byte in bytes {
                if two.write_all(&[byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        });
        let started = Instant::now();
        let error =
            receive_with_deadline::<Request>(&mut one, Duration::from_millis(90)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(one.read_timeout().unwrap(), original);
        drop(one);
        sender.join().unwrap();
    }
    #[test]
    fn complete_partial_reads_succeed_and_restore_socket_timeout() {
        let (mut one, mut two) = UnixStream::pair().unwrap();
        let mut bytes = Vec::new();
        send(&mut bytes, &Request::GetCatalog).unwrap();
        let sender = std::thread::spawn(move || {
            for chunk in bytes.chunks(2) {
                two.write_all(chunk).unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        assert!(matches!(
            receive_with_deadline::<Request>(&mut one, Duration::from_secs(1)).unwrap(),
            Request::GetCatalog
        ));
        assert_eq!(one.read_timeout().unwrap(), None);
        sender.join().unwrap();
    }
    #[test]
    fn framing_deadline_rejects_expired_and_oversized_requests() {
        let (mut one, mut two) = UnixStream::pair().unwrap();
        assert_eq!(
            receive_with_deadline::<Request>(&mut one, Duration::ZERO)
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        two.write_all(&((MAX_FRAME + 1) as u32).to_be_bytes())
            .unwrap();
        assert_eq!(
            receive_with_deadline::<Request>(&mut one, Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(one.read_timeout().unwrap(), None);
    }
}
