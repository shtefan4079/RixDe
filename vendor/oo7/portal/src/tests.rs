use super::*;
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
};
struct Bus(Child);
impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
async fn connections() -> (Bus, zbus::Connection, zbus::Connection, zbus::Connection) {
    let mut process = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut address = String::new();
    BufReader::new(process.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    let server = zbus::connection::Builder::address(address.trim())
        .unwrap()
        .name(PORTAL_NAME)
        .unwrap()
        .serve_at(PORTAL_PATH, Secret::default())
        .unwrap()
        .build()
        .await
        .unwrap();
    let frontend = zbus::connection::Builder::address(address.trim())
        .unwrap()
        .name("org.freedesktop.portal.Desktop")
        .unwrap()
        .build()
        .await
        .unwrap();
    let attacker = zbus::connection::Builder::address(address.trim())
        .unwrap()
        .build()
        .await
        .unwrap();
    (Bus(process), server, frontend, attacker)
}

#[tokio::test]
async fn only_the_actual_public_portal_owner_may_supply_app_identity() {
    let (_bus, server, frontend, attacker) = connections().await;
    assert!(
        require_portal_owner(&server, frontend.unique_name().map(|v| &**v))
            .await
            .is_ok()
    );
    assert!(
        require_portal_owner(&server, attacker.unique_name().map(|v| &**v))
            .await
            .is_err()
    );
    assert!(require_portal_owner(&server, None).await.is_err());
    frontend
        .release_name("org.freedesktop.portal.Desktop")
        .await
        .unwrap();
    assert!(
        require_portal_owner(&server, frontend.unique_name().map(|v| &**v))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn untrusted_retrieve_is_rejected_before_any_vault_access() {
    let (_bus, _server, _frontend, attacker) = connections().await;
    let proxy = zbus::Proxy::new(
        &attacker,
        PORTAL_NAME,
        PORTAL_PATH,
        "org.freedesktop.impl.portal.Secret",
    )
    .await
    .unwrap();
    let (read, _write) = std::os::unix::net::UnixStream::pair().unwrap();
    let fd = OwnedFd::from(std::os::fd::OwnedFd::from(read));
    let result: zbus::Result<(u32, HashMap<String, OwnedValue>)> = proxy
        .call(
            "RetrieveSecret",
            &(
                OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/request/fake/test")
                    .unwrap(),
                "org.example.Victim",
                fd,
                HashMap::<String, OwnedValue>::new(),
            ),
        )
        .await;
    assert!(
        matches!(result, Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == "org.freedesktop.DBus.Error.AccessDenied")
    );
}

#[tokio::test]
async fn request_close_requires_its_portal_owner() {
    let (_bus, server, frontend, attacker) = connections().await;
    let cancel = Arc::new(Notify::new());
    let path = "/org/freedesktop/portal/desktop/request/test/close";
    server
        .object_server()
        .at(
            path,
            Request {
                owner: frontend.unique_name().unwrap().to_owned().into(),
                cancel: cancel.clone(),
            },
        )
        .await
        .unwrap();
    let attacker = zbus::Proxy::new(
        &attacker,
        PORTAL_NAME,
        path,
        "org.freedesktop.impl.portal.Request",
    )
    .await
    .unwrap();
    assert!(attacker.call::<_, _, ()>("Close", &()).await.is_err());
    let frontend = zbus::Proxy::new(
        &frontend,
        PORTAL_NAME,
        path,
        "org.freedesktop.impl.portal.Request",
    )
    .await
    .unwrap();
    frontend.call::<_, _, ()>("Close", &()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), cancel.notified())
        .await
        .unwrap();
}

#[tokio::test]
async fn socket_secret_output_transfers_exact_bytes() {
    use std::io::Read;
    let (mut read, write) = std::os::unix::net::UnixStream::pair().unwrap();
    write_secret_fd(write.into(), b"fixture-token")
        .await
        .unwrap();
    let mut token = [0u8; 13];
    read.read_exact(&mut token).unwrap();
    assert_eq!(&token, b"fixture-token");
}

#[tokio::test]
async fn full_socket_secret_output_is_cancellable() {
    use std::io::Write;
    let (_read, mut write) = std::os::unix::net::UnixStream::pair().unwrap();
    write.set_nonblocking(true).unwrap();
    while write.write(&[0u8; 4096]).is_ok() {}
    let task = tokio::spawn(async move { write_secret_fd(write.into(), b"fixture-token").await });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(!task.is_finished());
    task.abort();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap_err()
            .is_cancelled()
    );
}

#[tokio::test]
async fn memfd_secret_output_is_supported() {
    use std::{io::Read, os::fd::AsFd};
    let fd =
        rustix::fs::memfd_create(c"clavis-test-token", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
    let mut file = std::fs::File::from(fd.as_fd().try_clone_to_owned().unwrap());
    write_secret_fd(fd, b"fixture-token").await.unwrap();
    rustix::fs::seek(&file, rustix::fs::SeekFrom::Start(0)).unwrap();
    let mut token = [0u8; 13];
    file.read_exact(&mut token).unwrap();
    assert_eq!(&token, b"fixture-token");
}
