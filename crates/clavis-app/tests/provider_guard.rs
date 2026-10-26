use clavis_client::{Client, Interaction};
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const SERVICE: &str = "org.freedesktop.secrets";
const ROOT: &str = "/org/freedesktop/secrets";
const SESSION: &str = "/org/freedesktop/secrets/session/fixture";
const COLLECTION: &str = "/org/freedesktop/secrets/collection/fixture";
const DH: &str = "dh-ietf1024-sha256-aes128-cbc-pkcs7";

struct PrivateBus {
    child: Child,
    directory: tempfile::TempDir,
    address: String,
}
impl PrivateBus {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--nopidfile", "--print-address=1"])
            .arg(format!(
                "--address=unix:path={}",
                directory.path().join("bus").display()
            ))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let mut bus = Self {
            child,
            directory,
            address: String::new(),
        };
        let stdout = bus.child.stdout.take().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut address = String::new();
            let result = BufReader::new(stdout)
                .read_line(&mut address)
                .map(|_| address.trim().to_owned());
            let _ = sender.send(result);
        });
        bus.address = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("Private bus startup timed out")
            .unwrap();
        reader.join().unwrap();
        assert!(bus.address.starts_with("unix:"));
        bus
    }
}
impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
struct Calls {
    reject_dh: AtomicBool,
    dh: AtomicUsize,
    plain: AtomicUsize,
    lock: AtomicUsize,
    locked: AtomicBool,
}
struct OtherSecretService(Arc<Calls>);
#[zbus::interface(name = "org.freedesktop.Secret.Service")]
impl OtherSecretService {
    fn open_session(
        &self,
        algorithm: &str,
        _input: Value<'_>,
    ) -> zbus::fdo::Result<(OwnedValue, OwnedObjectPath)> {
        let output = match algorithm {
            DH => {
                self.0.dh.fetch_add(1, Ordering::SeqCst);
                if self.0.reject_dh.load(Ordering::SeqCst) {
                    return Err(zbus::fdo::Error::NotSupported(
                        "This fixture rejects encrypted sessions".into(),
                    ));
                }

                Value::from(vec![2u8]).try_to_owned().unwrap()
            }
            "plain" => {
                self.0.plain.fetch_add(1, Ordering::SeqCst);
                Value::from("").try_to_owned().unwrap()
            }
            _ => return Err(zbus::fdo::Error::NotSupported("Unknown algorithm".into())),
        };
        Ok((output, SESSION.try_into().unwrap()))
    }

    #[zbus(property)]
    fn collections(&self) -> Vec<OwnedObjectPath> {
        vec![COLLECTION.try_into().unwrap()]
    }

    fn lock(&self, objects: Vec<OwnedObjectPath>) -> (Vec<OwnedObjectPath>, OwnedObjectPath) {
        self.0.lock.fetch_add(1, Ordering::SeqCst);
        self.0.locked.store(true, Ordering::SeqCst);
        (objects, "/".try_into().unwrap())
    }
}
struct OtherSession;
#[zbus::interface(name = "org.freedesktop.Secret.Session")]
impl OtherSession {
    fn close(&self) {}
}
struct OtherCollection(Arc<Calls>);
#[zbus::interface(name = "org.freedesktop.Secret.Collection")]
impl OtherCollection {
    #[zbus(property)]
    fn locked(&self) -> bool {
        self.0.locked.load(Ordering::SeqCst)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_provider_backup_preserves_wallet_and_sdk_never_downgrades_to_plain() {
    let bus = PrivateBus::new();
    let calls = Arc::new(Calls::default());
    let _provider = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .name(SERVICE)
        .unwrap()
        .serve_at(ROOT, OtherSecretService(calls.clone()))
        .unwrap()
        .serve_at(SESSION, OtherSession)
        .unwrap()
        .serve_at(COLLECTION, OtherCollection(calls.clone()))
        .unwrap()
        .build()
        .await
        .unwrap();
    let connection = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .method_timeout(Duration::from_secs(3))
        .build()
        .await
        .unwrap();

    calls.reject_dh.store(true, Ordering::SeqCst);
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        Client::with_connection(connection.clone(), Interaction::Never),
    )
    .await
    .unwrap();
    assert!(
        result.is_err(),
        "Rejecting DH must prevent an SDK connection"
    );
    assert_eq!(calls.dh.load(Ordering::SeqCst), 1);
    assert_eq!(calls.plain.load(Ordering::SeqCst), 0);

    calls.reject_dh.store(false, Ordering::SeqCst);
    let client = Client::with_connection(connection, Interaction::Never)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), client.lock_all())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(calls.lock.load(Ordering::SeqCst), 1);
    assert!(calls.locked.load(Ordering::SeqCst));
    drop(client);
    calls.lock.store(0, Ordering::SeqCst);
    calls.locked.store(false, Ordering::SeqCst);
    let sessions_before = calls.dh.load(Ordering::SeqCst);

    let home = bus.directory.path().join("home");
    let data = home.join("data");
    let keyrings = data.join("clavis/keyrings/v1");
    std::fs::create_dir_all(&keyrings).unwrap();
    let original = keyrings.join("fixture.keyring");
    let original_bytes = b"fixture bytes must be preserved without being backed up";
    std::fs::write(&original, original_bytes).unwrap();
    let archive = home.join("rejected.backup");
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_clavis"));
    command
        .args(["backup", archive.to_str().unwrap()])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("XDG_DATA_HOME", &data)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_RUNTIME_DIR", bus.directory.path())
        .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
        .env("DBUS_SYSTEM_BUS_ADDRESS", &bus.address)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let result = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .expect("CLI backup did not exit within its fixture deadline")
        .unwrap();
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr)
            .contains("The active provider must be Clavis before creating a Clavis backup"),
        "CLI must fail at the provider guard: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(calls.dh.load(Ordering::SeqCst), sessions_before + 1);
    assert_eq!(calls.plain.load(Ordering::SeqCst), 0);
    assert_eq!(calls.lock.load(Ordering::SeqCst), 0);
    assert!(!calls.locked.load(Ordering::SeqCst));
    assert!(
        !archive.exists(),
        "Wrong-provider backup must not create an archive"
    );
    assert_eq!(std::fs::read(original).unwrap(), original_bytes);
}
