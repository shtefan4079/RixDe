//! Clavis lock policy and a small management API. Secrets stay in Secret Service.
use std::collections::HashMap;

use oo7::dbus::ServiceError;
use tokio_stream::StreamExt;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

use crate::{Service, error::custom_service_error};

#[derive(Clone)]
pub struct Control(pub Service);

#[zbus::interface(name = "org.clavis.Control")]
impl Control {
    async fn lock_all(&self) -> Result<(), ServiceError> {
        self.0.cancel_prompts_and_lock().await
    }

    #[cfg(any(feature = "gnome_native_crypto", feature = "gnome_openssl_crypto"))]
    async fn change_password(
        &self,
        collection: ObjectPath<'_>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<OwnedObjectPath, ServiceError> {
        crate::gnome::internal::InternalInterface::new(self.0.clone())
            .change_with_prompt(collection, header)
            .await
    }

    #[zbus(property)]
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }
}

pub fn spawn(service: Service) {
    tokio::spawn(async move {
        loop {
            if let Err(error) = watch_logind(&service).await {
                tracing::warn!("Clavis logind lock watcher unavailable: {error}");
            }
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }
    });
}

async fn watch_logind(service: &Service) -> zbus::Result<()> {
    let bus = zbus::Connection::system().await?;
    watch_on(service, &bus).await
}

pub(crate) async fn watch_on(service: &Service, bus: &zbus::Connection) -> zbus::Result<()> {
    let manager = zbus::Proxy::new(
        bus,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await?;
    let mut sleeps = manager.receive_signal("PrepareForSleep").await?;
    let path = match manager
        .call::<_, _, OwnedObjectPath>("GetSessionByPID", &(std::process::id(),))
        .await
    {
        Ok(path) => path,
        Err(error) => match std::env::var("XDG_SESSION_ID") {
            Ok(id) if !id.is_empty() => manager.call("GetSession", &(id,)).await?,
            _ => return Err(error),
        },
    };
    let session = zbus::Proxy::new(
        bus,
        "org.freedesktop.login1",
        path.as_str(),
        "org.freedesktop.login1.Session",
    )
    .await?;
    let mut locks = session.receive_signal("Lock").await?;
    let properties = zbus::Proxy::new(
        bus,
        "org.freedesktop.login1",
        path.as_str(),
        "org.freedesktop.DBus.Properties",
    )
    .await?;
    let mut changes = properties.receive_signal("PropertiesChanged").await?;
    if session
        .get_property::<bool>("LockedHint")
        .await
        .unwrap_or(false)
        || manager
            .get_property::<bool>("PreparingForSleep")
            .await
            .unwrap_or(false)
    {
        service
            .cancel_prompts_and_lock()
            .await
            .map_err(|e| zbus::Error::Failure(e.to_string()))?;
    }
    loop {
        let should_lock = tokio::select! {
            signal = sleeps.next() => signal.ok_or_else(|| zbus::Error::Failure("logind sleep stream closed".into()))?.body().deserialize::<(bool,)>()?.0,
            signal = locks.next() => { signal.ok_or_else(|| zbus::Error::Failure("logind lock stream closed".into()))?; true },
            signal = changes.next() => {
                let signal = signal.ok_or_else(|| zbus::Error::Failure("logind property stream closed".into()))?;
                let (interface, values, invalidated): (String, HashMap<String, OwnedValue>, Vec<String>) = signal.body().deserialize()?;
                if interface != "org.freedesktop.login1.Session" { false }
                else if let Some(value) = values.get("LockedHint") { bool::try_from(value).unwrap_or(false) }
                else if invalidated.iter().any(|name| name == "LockedHint") { session.get_property::<bool>("LockedHint").await.unwrap_or(false) }
                else { false }
            }
        };
        if should_lock {
            service
                .cancel_prompts_and_lock()
                .await
                .map_err(|e| zbus::Error::Failure(e.to_string()))?;
            tracing::info!("Clavis locked for session lock or sleep");
        }
    }
}

impl Service {
    pub(crate) async fn cancel_prompts_and_lock(&self) -> Result<(), ServiceError> {
        let prompts = self.prompts_snapshot().await;
        for prompt in prompts {
            prompt.cancel().await?;
        }
        let collections = self
            .collections
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut objects = Vec::new();
        for collection in collections {
            if collection.name() == "session" {
                collection.clear_session_secrets().await?;
            } else {
                objects.push(collection.path().to_owned().into());
            }
        }
        let (_, rejected) = self.set_locked(true, &objects).await?;
        if rejected.is_empty() {
            Ok(())
        } else {
            Err(custom_service_error("Some collections could not be locked"))
        }
    }
}
