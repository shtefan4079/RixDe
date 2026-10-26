pub async fn watch_lock_requests<F, Fut, R>(
    lock: F,
    locked: tokio::sync::watch::Receiver<bool>,
    resumed: R,
) -> anyhow::Result<()>
where
    R: Fn(),
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let conn = zbus::connection::Builder::system()?
        .method_timeout(std::time::Duration::from_secs(3))
        .build()
        .await?;
    watch_lock_requests_on(&conn, lock, locked, resumed).await
}

async fn watch_lock_requests_on<F, Fut, R>(
    conn: &zbus::Connection,
    lock: F,
    mut locked: tokio::sync::watch::Receiver<bool>,
    resumed: R,
) -> anyhow::Result<()>
where
    R: Fn(),
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    use futures_util::StreamExt;
    let manager = zbus::proxy::Builder::<zbus::Proxy<'_>>::new(conn)
        .destination("org.freedesktop.login1")?
        .path("/org/freedesktop/login1")?
        .interface("org.freedesktop.login1.Manager")?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;
    let session_path: zbus::zvariant::OwnedObjectPath =
        manager.call("GetSessionByPID", &std::process::id()).await?;
    let session = zbus::Proxy::new(
        conn,
        "org.freedesktop.login1",
        session_path,
        "org.freedesktop.login1.Session",
    )
    .await?;
    let mut sleeps = manager.receive_signal("PrepareForSleep").await?;
    let mut locks = session.receive_signal("Lock").await?;
    let initial_locked = *locked.borrow_and_update();
    session
        .call::<_, _, ()>("SetLockedHint", &(initial_locked))
        .await?;

    let mut inhibitor = sleep_inhibitor(&manager).await?;
    if inhibitor.is_none() {
        if let Err(error) = lock().await {
            tracing::error!(%error, "Could not lock a session started during sleep");
        }

        if !manager.get_property::<bool>("PreparingForSleep").await? {
            resumed();
            inhibitor = sleep_inhibitor(&manager).await?;
        }
    }
    loop {
        tokio::select! {
            changed = locked.changed() => {
                changed?;
                let value = *locked.borrow_and_update();
                session.call::<_, _, ()>("SetLockedHint", &(value)).await?;
            }
            message = sleeps.next() => {
                let Some(message) = message else { anyhow::bail!("logind sleep signal stream ended"); };
                let (sleeping,) = message.body().deserialize::<(bool,)>()?;
                if sleeping {
                    if let Err(error) = lock().await { tracing::error!(%error, "Could not confirm lock before sleep"); }
                    inhibitor.take();
                } else {
                    resumed();
                    if inhibitor.is_none() {
                        inhibitor = sleep_inhibitor(&manager).await?;
                    }
                }
            }
            message = locks.next() => {
                if message.is_none() { anyhow::bail!("logind lock signal stream ended"); }
                if let Err(error) = lock().await { tracing::error!(%error, "Could not handle logind lock request"); }
            }
        }
    }
}

async fn sleep_inhibitor(
    manager: &zbus::Proxy<'_>,
) -> anyhow::Result<Option<zbus::zvariant::OwnedFd>> {
    match manager
        .call(
            "Inhibit",
            &("sleep", "RixDE", "Lock the session before sleep", "delay"),
        )
        .await
    {
        Ok(fd) => Ok(Some(fd)),
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.freedesktop.login1.OperationInProgress" =>
        {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

pub struct LidInhibitor {
    connection: zbus::Connection,
    lease: Option<zbus::zvariant::OwnedFd>,
}
impl LidInhibitor {
    pub async fn new(enabled: bool) -> anyhow::Result<Self> {

        tokio::time::timeout(std::time::Duration::from_secs(2), Self::connect(enabled)).await?
    }
    async fn connect(enabled: bool) -> anyhow::Result<Self> {
        let connection = zbus::connection::Builder::system()?
            .method_timeout(std::time::Duration::from_secs(2))
            .build()
            .await?;
        let mut inhibitor = Self {
            connection,
            lease: None,
        };
        inhibitor.update(enabled).await?;
        Ok(inhibitor)
    }
    async fn update(&mut self, enabled: bool) -> anyhow::Result<()> {
        if enabled && self.lease.is_none() {
            let manager = zbus::Proxy::new(
                &self.connection,
                "org.freedesktop.login1",
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
            )
            .await?;
            self.lease = Some(
                manager
                    .call(
                        "Inhibit",
                        &(
                            "handle-lid-switch",
                            "RixDE",
                            "Keep the session on the external display",
                            "block",
                        ),
                    )
                    .await?,
            );
            tracing::info!("External display connected: lid switch inhibited");
        } else if !enabled && self.lease.take().is_some() {
            tracing::info!("Lid switch returned to logind");
        }
        Ok(())
    }
    pub async fn watch(
        mut self,
        mut desired: tokio::sync::watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        loop {
            let enabled = *desired.borrow_and_update();
            self.update(enabled).await?;
            if desired.changed().await.is_err() {
                return Ok(());
            }
        }
    }
}

pub fn is_external_connector(name: &str) -> bool {
    !["eDP-", "LVDS-", "DSI-"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

pub struct LidPolicy {
    inhibited: bool,
}

impl LidPolicy {
    pub fn new(inhibited: bool) -> Self {
        Self { inhibited }
    }

    pub fn update<S: AsRef<str>>(
        &mut self,
        enabled: bool,
        outputs: impl IntoIterator<Item = S>,
    ) -> bool {
        let mut has_output = false;
        let mut has_external = false;
        for name in outputs {
            has_output = true;
            has_external |= is_external_connector(name.as_ref());
        }
        self.inhibited = enabled && (has_external || (!has_output && self.inhibited));
        self.inhibited
    }
}

pub fn external_display_connected() -> bool {
    external_display_connected_in(std::path::Path::new("/sys/class/drm"))
}
fn external_display_connected_in(root: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let Some((card, connector)) = name.to_str().and_then(|name| name.split_once('-')) else {
            return false;
        };
        card.strip_prefix("card")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
            && is_external_connector(connector)
            && std::fs::read_to_string(entry.path().join("status"))
                .is_ok_and(|status| status.trim() == "connected")
    })
}

pub async fn request(action: &str) -> anyhow::Result<()> {
    if action == "logout" {
        let mut client = rix_ipc::IpcClient::connect().await?;
        anyhow::ensure!(
            client.send(rix_ipc::IpcRequest::LogoutSession).await? == rix_ipc::IpcResponse::Ok,
            "The compositor did not confirm the session logout"
        );
        return Ok(());
    }
    anyhow::ensure!(
        std::env::var_os("RIX_NATIVE_SESSION").is_some(),
        "Session actions require a dedicated RixDE login"
    );
    let conn = zbus::connection::Builder::system()?
        .method_timeout(std::time::Duration::from_secs(30))
        .build()
        .await?;
    let manager = zbus::Proxy::new(
        &conn,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await?;
    match action {
        "suspend" | "hibernate" | "reboot" | "poweroff" => {
            let method = match action {
                "suspend" => "Suspend",
                "hibernate" => "Hibernate",
                "reboot" => "Reboot",
                _ => "PowerOff",
            };
            manager.call::<_, _, ()>(method, &(false)).await?;
        }
        _ => anyhow::bail!("Unknown session action"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::net::UnixStream,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::Duration,
    };
    use tokio::{io::AsyncReadExt, sync::mpsc, time::timeout};

    #[derive(Debug, zbus::DBusError)]
    #[zbus(prefix = "org.freedesktop.login1")]
    enum LoginError {
        OperationInProgress(String),
    }

    struct Manager {
        leases: mpsc::UnboundedSender<UnixStream>,
        sleeping: Arc<AtomicBool>,
        expected_inhibitor: (&'static str, &'static str),
    }
    #[zbus::interface(name = "org.freedesktop.login1.Manager")]
    impl Manager {
        #[zbus(name = "GetSessionByPID")]
        fn get_session_by_pid(&self, _pid: u32) -> zbus::zvariant::OwnedObjectPath {
            "/test/session".try_into().unwrap()
        }
        fn inhibit(
            &self,
            what: &str,
            _who: &str,
            _why: &str,
            mode: &str,
        ) -> Result<zbus::zvariant::OwnedFd, LoginError> {
            assert_eq!((what, mode), self.expected_inhibitor);
            if what == "sleep" && self.sleeping.load(Ordering::SeqCst) {
                return Err(LoginError::OperationInProgress("Already suspending".into()));
            }
            let (reader, lease) = UnixStream::pair().unwrap();
            reader.set_nonblocking(true).unwrap();
            self.leases.send(reader).unwrap();
            let fd: std::os::fd::OwnedFd = lease.into();
            Ok(fd.into())
        }
        #[zbus(property(emits_changed_signal = "false"))]
        fn preparing_for_sleep(&self) -> bool {
            self.sleeping.load(Ordering::SeqCst)
        }
    }
    struct Session(mpsc::UnboundedSender<bool>);
    #[zbus::interface(name = "org.freedesktop.login1.Session")]
    impl Session {
        fn set_locked_hint(&self, locked: bool) {
            self.0.send(locked).unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "requires isolated dbus-run-session; never use the system bus"]
    async fn private_bus_sleep_waits_for_lock_and_publishes_lock_state() {
        let (leases_tx, mut leases) = mpsc::unbounded_channel();
        let (hints_tx, mut hints) = mpsc::unbounded_channel();
        let connection = zbus::connection::Builder::session()
            .unwrap()
            .name("org.freedesktop.login1")
            .unwrap()
            .serve_at(
                "/org/freedesktop/login1",
                Manager {
                    leases: leases_tx,
                    sleeping: Arc::new(AtomicBool::new(false)),
                    expected_inhibitor: ("sleep", "delay"),
                },
            )
            .unwrap()
            .serve_at("/test/session", Session(hints_tx))
            .unwrap()
            .build()
            .await
            .unwrap();
        let (locked_tx, locked_rx) = tokio::sync::watch::channel(false);
        let (requests_tx, mut requests) = mpsc::unbounded_channel();
        let worker_connection = connection.clone();
        let worker = tokio::spawn(async move {
            watch_lock_requests_on(
                &worker_connection,
                move || {
                    let (reply, confirmation) = tokio::sync::oneshot::channel::<()>();
                    requests_tx.send(reply).unwrap();
                    async move {
                        confirmation.await?;
                        Ok(())
                    }
                },
                locked_rx,
                || {},
            )
            .await
        });
        let bound = Duration::from_secs(3);
        assert_eq!(timeout(bound, hints.recv()).await.unwrap(), Some(false));
        let lease = timeout(bound, leases.recv()).await.unwrap().unwrap();
        let mut lease = tokio::net::UnixStream::from_std(lease).unwrap();
        connection
            .emit_signal(
                None::<&str>,
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
                "PrepareForSleep",
                &(true,),
            )
            .await
            .unwrap();
        let confirmation = timeout(bound, requests.recv()).await.unwrap().unwrap();
        let mut byte = [0];
        assert!(
            timeout(Duration::from_millis(40), lease.read(&mut byte))
                .await
                .is_err(),
            "The delay inhibitor must remain open until the lock frame is confirmed"
        );
        locked_tx.send_replace(true);
        confirmation.send(()).unwrap();
        assert_eq!(
            timeout(bound, lease.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert_eq!(timeout(bound, hints.recv()).await.unwrap(), Some(true));
        connection
            .emit_signal(
                None::<&str>,
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
                "PrepareForSleep",
                &(false,),
            )
            .await
            .unwrap();
        let _next_lease = timeout(bound, leases.recv()).await.unwrap().unwrap();
        locked_tx.send_replace(false);
        assert_eq!(timeout(bound, hints.recv()).await.unwrap(), Some(false));
        connection
            .emit_signal(
                None::<&str>,
                "/test/session",
                "org.freedesktop.login1.Session",
                "Lock",
                &(),
            )
            .await
            .unwrap();
        timeout(bound, requests.recv())
            .await
            .unwrap()
            .unwrap()
            .send(())
            .unwrap();
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    #[ignore = "requires isolated dbus-run-session; never use the system bus"]
    async fn private_bus_starting_during_sleep_keeps_wake_and_lock_subscriptions() {

        for wake_during_lock in [false, true] {
            let sleeping = Arc::new(AtomicBool::new(true));
            let (leases_tx, mut leases) = mpsc::unbounded_channel();
            let (hints_tx, mut hints) = mpsc::unbounded_channel();
            let connection = zbus::connection::Builder::session()
                .unwrap()
                .name("org.freedesktop.login1")
                .unwrap()
                .serve_at(
                    "/org/freedesktop/login1",
                    Manager {
                        leases: leases_tx,
                        sleeping: sleeping.clone(),
                        expected_inhibitor: ("sleep", "delay"),
                    },
                )
                .unwrap()
                .serve_at("/test/session", Session(hints_tx))
                .unwrap()
                .build()
                .await
                .unwrap();
            let (_locked_tx, locked_rx) = tokio::sync::watch::channel(false);
            let (requests_tx, mut requests) = mpsc::unbounded_channel();
            let (wakes_tx, mut wakes) = mpsc::unbounded_channel();
            let worker_connection = connection.clone();
            let worker = tokio::spawn(async move {
                watch_lock_requests_on(
                    &worker_connection,
                    move || {
                        let (reply, confirmation) = tokio::sync::oneshot::channel::<()>();
                        requests_tx.send(reply).unwrap();
                        async move {
                            confirmation.await?;
                            Ok(())
                        }
                    },
                    locked_rx,
                    move || {
                        wakes_tx.send(()).unwrap();
                    },
                )
                .await
            });
            let bound = Duration::from_secs(3);
            assert_eq!(timeout(bound, hints.recv()).await.unwrap(), Some(false));
            let confirmation = timeout(bound, requests.recv()).await.unwrap().unwrap();
            assert!(
                leases.try_recv().is_err(),
                "Sleep already in progress cannot issue a delay lease"
            );
            if wake_during_lock {
                sleeping.store(false, Ordering::SeqCst);
            }
            confirmation.send(()).unwrap();
            if wake_during_lock {

                timeout(bound, wakes.recv()).await.unwrap().unwrap();
            } else {
                assert!(timeout(Duration::from_millis(40), wakes.recv())
                    .await
                    .is_err());
                sleeping.store(false, Ordering::SeqCst);
                connection
                    .emit_signal(
                        None::<&str>,
                        "/org/freedesktop/login1",
                        "org.freedesktop.login1.Manager",
                        "PrepareForSleep",
                        &(false,),
                    )
                    .await
                    .unwrap();
                timeout(bound, wakes.recv()).await.unwrap().unwrap();
            }
            let _lease = timeout(bound, leases.recv()).await.unwrap().unwrap();
            connection
                .emit_signal(
                    None::<&str>,
                    "/test/session",
                    "org.freedesktop.login1.Session",
                    "Lock",
                    &(),
                )
                .await
                .unwrap();
            timeout(bound, requests.recv())
                .await
                .unwrap()
                .unwrap()
                .send(())
                .unwrap();
            worker.abort();
            assert!(worker.await.unwrap_err().is_cancelled());
            connection
                .release_name("org.freedesktop.login1")
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "requires isolated dbus-run-session; never use the system bus"]
    async fn private_bus_lid_policy_releases_and_reacquires_its_lease() {
        let (leases_tx, mut leases) = mpsc::unbounded_channel();
        let connection = zbus::connection::Builder::session()
            .unwrap()
            .name("org.freedesktop.login1")
            .unwrap()
            .serve_at(
                "/org/freedesktop/login1",
                Manager {
                    leases: leases_tx,
                    sleeping: Arc::new(AtomicBool::new(false)),
                    expected_inhibitor: ("handle-lid-switch", "block"),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let guard = LidInhibitor {
            connection,
            lease: None,
        };
        let (desired, receiver) = tokio::sync::watch::channel(true);
        let worker = tokio::spawn(guard.watch(receiver));
        let bound = Duration::from_secs(3);
        let lease = timeout(bound, leases.recv()).await.unwrap().unwrap();
        let mut lease = tokio::net::UnixStream::from_std(lease).unwrap();
        let mut byte = [0];
        desired.send_replace(true);
        assert!(
            timeout(Duration::from_millis(40), leases.recv())
                .await
                .is_err(),
            "An unchanged policy must not duplicate the lease"
        );
        assert!(timeout(Duration::from_millis(40), lease.read(&mut byte))
            .await
            .is_err());
        desired.send_replace(false);
        assert_eq!(
            timeout(bound, lease.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        desired.send_replace(true);
        let lease = timeout(bound, leases.recv()).await.unwrap().unwrap();
        let mut lease = tokio::net::UnixStream::from_std(lease).unwrap();
        drop(desired);
        timeout(bound, worker).await.unwrap().unwrap().unwrap();
        assert_eq!(
            timeout(bound, lease.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0,
            "Exiting the session must return lid handling to logind"
        );
    }

    #[test]
    fn bootstrap_lid_policy_requires_a_connected_external_display() {
        let root = tempfile::tempdir().unwrap();
        let connector = |name: &str, status: &str| {
            let path = root.path().join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("status"), status).unwrap();
        };
        connector("card0-eDP-1", "connected\n");
        connector("card1-LVDS-1", "connected\n");
        connector("card1-DSI-1", "connected\n");
        connector("garbage-HDMI-A-1", "connected\n");
        connector("card1-DP-1", "disconnected\n");
        assert!(!external_display_connected_in(root.path()));
        connector("card1-DP-1", "connected\n");
        assert!(external_display_connected_in(root.path()));
        connector("card1-DP-1", "disconnected\n");
        assert!(!external_display_connected_in(root.path()));
        connector("card0-HDMI-A-1", "connected\n");
        assert!(external_display_connected_in(root.path()));
    }

    #[test]
    fn docked_lid_policy_survives_disconnect_and_reconnect_without_outputs() {
        let mut policy = LidPolicy::new(true);
        assert!(policy.update(true, ["DP-1"]));
        for _ in 0..3 {
            assert!(policy.update(true, std::iter::empty::<&str>()));
        }
        assert!(policy.update(true, ["HDMI-A-1"]));
        assert!(policy.update(true, ["HDMI-A-1", "eDP-1"]));
        assert!(!policy.update(true, ["eDP-1"]));
        assert!(!policy.update(true, std::iter::empty::<&str>()));
    }

    #[test]
    fn headless_startup_does_not_inhibit_lid_without_a_docked_session() {
        let mut policy = LidPolicy::new(false);
        assert!(!policy.update(true, std::iter::empty::<&str>()));
        assert!(!policy.update(true, ["eDP-1", "LVDS-1", "DSI-1"]));
        assert!(policy.update(true, ["DP-1"]));
    }

    #[test]
    fn disabling_docked_lid_policy_releases_the_lease_even_without_outputs() {
        let mut policy = LidPolicy::new(true);
        assert!(!policy.update(false, std::iter::empty::<&str>()));
        assert!(!policy.update(false, ["DP-1"]));
        assert!(!policy.update(true, std::iter::empty::<&str>()));
        assert!(policy.update(true, ["DP-1"]));
    }
}
