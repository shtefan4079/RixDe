use crate::tests::TestServiceSetup;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use zbus::{
    object_server::SignalEmitter,
    zvariant::{OwnedObjectPath, Value},
};

#[derive(Clone)]
struct Manager;
#[zbus::interface(name = "org.freedesktop.login1.Manager")]
impl Manager {
    #[zbus(name = "GetSessionByPID")]
    fn get_session_by_pid(&self, _pid: u32) -> OwnedObjectPath {
        OwnedObjectPath::try_from("/org/freedesktop/login1/session/test").unwrap()
    }
    #[zbus(property)]
    fn preparing_for_sleep(&self) -> bool {
        false
    }
    #[zbus(signal)]
    async fn prepare_for_sleep(emitter: &SignalEmitter<'_>, value: bool) -> zbus::Result<()>;
}
#[derive(Clone, Default)]
struct LoginSession(Arc<AtomicBool>);
#[zbus::interface(name = "org.freedesktop.login1.Session")]
impl LoginSession {
    #[zbus(property)]
    fn locked_hint(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    #[zbus(signal)]
    async fn lock(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

#[tokio::test]
async fn logind_sleep_lock_and_confirmed_locked_hint_lock_collections()
-> Result<(), Box<dyn std::error::Error>> {
    for event in ["sleep", "lock", "hint"] {
        let setup = TestServiceSetup::plain_session(true).await?;
        let session = LoginSession::default();
        setup
            .client_conn
            .object_server()
            .at("/org/freedesktop/login1", Manager)
            .await?;
        setup
            .client_conn
            .object_server()
            .at("/org/freedesktop/login1/session/test", session.clone())
            .await?;
        let service = setup.server.clone();
        let connection = service.connection();
        let watcher =
            tokio::spawn(async move { super::policy::watch_on(&service, &connection).await });
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(!setup.collections[0].is_locked().await?);
        match event {
            "sleep" => {
                Manager::prepare_for_sleep(
                    &SignalEmitter::new(&setup.client_conn, "/org/freedesktop/login1")?,
                    true,
                )
                .await?
            }
            "lock" => {
                LoginSession::lock(&SignalEmitter::new(
                    &setup.client_conn,
                    "/org/freedesktop/login1/session/test",
                )?)
                .await?
            }
            _ => {
                session.0.store(true, Ordering::Release);
                let changed = HashMap::from([("LockedHint", Value::from(true))]);
                setup
                    .client_conn
                    .emit_signal(
                        None::<&str>,
                        "/org/freedesktop/login1/session/test",
                        "org.freedesktop.DBus.Properties",
                        "PropertiesChanged",
                        &(
                            "org.freedesktop.login1.Session",
                            changed,
                            Vec::<String>::new(),
                        ),
                    )
                    .await?;
            }
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !setup.collections[0].is_locked().await.unwrap() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        watcher.abort();
    }
    Ok(())
}

#[tokio::test]
async fn control_lock_all_is_available_on_the_secret_service_connection()
-> Result<(), Box<dyn std::error::Error>> {
    let setup = TestServiceSetup::plain_session(true).await?;
    let control = zbus::Proxy::new(
        &setup.client_conn,
        "org.freedesktop.secrets",
        "/org/clavis/Control",
        "org.clavis.Control",
    )
    .await?;
    assert_eq!(
        control.get_property::<String>("Version").await?,
        env!("CARGO_PKG_VERSION")
    );
    control.call::<_, _, ()>("LockAll", &()).await?;
    assert!(setup.collections[0].is_locked().await?);
    Ok(())
}

#[tokio::test]
async fn policy_lock_discards_session_secrets_and_keeps_empty_session_usable()
-> Result<(), Box<dyn std::error::Error>> {
    let setup = TestServiceSetup::plain_session(true).await?;
    let session = setup.service_api.read_alias("session").await?.unwrap();
    let secret = setup.create_dbus_secret("ephemeral-fixture-secret")?;
    session
        .create_item(
            "Ephemeral",
            &[("app", "test-session")],
            &secret,
            false,
            None,
        )
        .await?;
    assert_eq!(session.items().await?.len(), 1);
    setup.server.cancel_prompts_and_lock().await?;
    assert!(session.items().await?.is_empty());
    assert!(!session.is_locked().await?);
    session
        .create_item("New", &[("app", "test-session-new")], &secret, false, None)
        .await?;
    assert_eq!(session.items().await?.len(), 1);
    Ok(())
}
