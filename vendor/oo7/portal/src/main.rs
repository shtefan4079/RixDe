use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
mod error;
use ashpd::{
    PortalError,
    zbus::{
        self,
        zvariant::{OwnedFd, OwnedObjectPath, OwnedValue},
    },
};
use clap::Parser;
pub use error::Result;
use oo7::dbus::Service;
use tokio::sync::{Mutex, Notify};

const PORTAL_NAME: &str = "org.freedesktop.impl.portal.desktop.clavis";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";

async fn require_portal_owner(
    connection: &zbus::Connection,
    sender: Option<&zbus::names::UniqueName<'_>>,
) -> zbus::fdo::Result<()> {
    let sender = sender.ok_or_else(|| {
        zbus::fdo::Error::AccessDenied("A portal frontend sender is required".into())
    })?;
    let bus = zbus::fdo::DBusProxy::new(connection).await?;
    let owner = bus
        .get_name_owner("org.freedesktop.portal.Desktop".try_into().unwrap())
        .await
        .map_err(|_| {
            zbus::fdo::Error::AccessDenied("The public desktop portal is unavailable".into())
        })?;
    if owner.as_str() != sender.as_str() {
        return Err(zbus::fdo::Error::AccessDenied(
            "Only the public desktop portal may supply application identities".into(),
        ));
    }
    Ok(())
}

#[derive(Default)]
struct Secret {
    active: Arc<Mutex<HashSet<OwnedObjectPath>>>,
    creation: Arc<Mutex<()>>,
}
#[derive(Clone)]
struct Request {
    owner: zbus::names::OwnedUniqueName,
    cancel: Arc<Notify>,
}
#[zbus::interface(name = "org.freedesktop.impl.portal.Request")]
impl Request {
    async fn close(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        require_portal_owner(connection, header.sender()).await?;
        if header
            .sender()
            .is_none_or(|sender| sender.as_str() != self.owner.as_str())
        {
            return Err(zbus::fdo::Error::AccessDenied(
                "Request belongs to a different portal frontend".into(),
            ));
        }
        self.cancel.notify_one();
        Ok(())
    }
}
#[zbus::interface(name = "org.freedesktop.impl.portal.Secret")]
impl Secret {
    #[zbus(property(emits_changed_signal = "const"), name = "version")]
    fn version(&self) -> u32 {
        1
    }

    #[zbus(out_args("response", "results"))]
    async fn retrieve_secret(
        &self,
        handle: OwnedObjectPath,
        app_id: &str,
        fd: OwnedFd,
        _options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        require_portal_owner(connection, header.sender()).await?;
        if !handle
            .as_str()
            .starts_with("/org/freedesktop/portal/desktop/request/")
            || ashpd::AppID::try_from(app_id).is_err()
        {
            return Err(zbus::fdo::Error::InvalidArgs(
                "Valid request path and application ID are required".into(),
            ));
        }
        if !self.active.lock().await.insert(handle.clone()) {
            return Err(zbus::fdo::Error::InvalidArgs(
                "Request handle is already active".into(),
            ));
        }
        let cancel = Arc::new(Notify::new());
        let request = Request {
            owner: header.sender().unwrap().to_owned().into(),
            cancel: cancel.clone(),
        };
        if let Err(error) = connection.object_server().at(&handle, request).await {
            self.active.lock().await.remove(&handle);
            return Err(error.into());
        }
        let operation = async {
            // Prevent simultaneous first requests for an app from creating
            // different master tokens, one of which would immediately be lost.
            let _creation = self.creation.lock().await;
            send_secret_to_app(app_id, fd.into()).await
        };
        let response = tokio::select! {
            _ = cancel.notified() => 1,
            result = tokio::time::timeout(std::time::Duration::from_secs(300), operation) => match result {
                Ok(Ok(())) => 0,
                Ok(Err(error::Error::Oo7(oo7::dbus::Error::Dismissed))) => 1,
                Ok(Err(error)) => { tracing::warn!("Clavis secret portal request failed: {error}"); 2 },
                Err(_) => 2,
            }
        };
        self.active.lock().await.remove(&handle);
        let _ = connection
            .object_server()
            .remove::<Request, _>(&handle)
            .await;
        Ok((response, HashMap::new()))
    }
}

/// Generates, stores and send the secret back to the fd stream
async fn send_secret_to_app(app_id: &str, fd: std::os::fd::OwnedFd) -> Result<()> {
    let service = Service::new().await?;
    let collection = service.default_collection().await?;

    let attributes = &[
        ("app_id", app_id),
        ("xdg:schema", "org.clavis.SecretPortal"),
    ];

    if collection.is_locked().await? {
        collection.unlock(None).await?;
    }

    if let Some(item) = collection.search_items(attributes).await?.first() {
        write_secret_fd(fd, &item.secret().await?).await?;
    } else {
        tracing::debug!("Could not find secret for {app_id}, creating one");
        let secret =
            oo7::Secret::random().map_err(|e| ashpd::PortalError::Failed(e.to_string()))?;

        collection
            .create_item(
                &format!("Secret Portal token for {app_id}"),
                attributes,
                secret.clone(),
                true,
                None,
            )
            .await?;

        write_secret_fd(fd, &secret).await?;
    }

    Ok(())
}

/// Pollable pipe/socket writes remain cancellable even when an app stops reading.
async fn write_secret_fd(fd: std::os::fd::OwnedFd, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::fd::AsFd;
    let flags = rustix::fs::fcntl_getfl(&fd)?;
    rustix::fs::fcntl_setfl(&fd, flags | rustix::fs::OFlags::NONBLOCK)?;
    match tokio::io::unix::AsyncFd::new(fd.as_fd().try_clone_to_owned()?) {
        Ok(pollable) => tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut written = 0;
            while written < bytes.len() {
                let mut ready = pollable.writable().await?;
                match ready.try_io(|fd| {
                    rustix::io::write(fd.get_ref(), &bytes[written..]).map_err(std::io::Error::from)
                }) {
                    Ok(Ok(0)) => return Err(std::io::Error::from(std::io::ErrorKind::WriteZero)),
                    Ok(Ok(count)) => written += count,
                    Ok(Err(error)) => return Err(error),
                    Err(_) => {}
                }
            }
            Ok(())
        })
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Secret portal output timed out",
            )
        })?,
        Err(error) => {
            // memfd output is also supported by the portal protocol. Regular
            // descriptors cannot be registered with epoll and the token is tiny.
            let mut file = std::fs::File::from(fd);
            if !file.metadata()?.is_file() {
                return Err(error);
            }
            std::io::Write::write_all(&mut file, bytes)
        }
    }
}

#[derive(Parser)]
#[command(version, about = "Clavis backend for the Secret portal")]
struct Args {
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if rustix::process::getuid().as_raw() == 0
        || rustix::process::getuid() != rustix::process::geteuid()
    {
        return Err(
            PortalError::Failed("Clavis portal must run as the session user".into()).into(),
        );
    }
    rustix::process::setrlimit(
        rustix::process::Resource::Core,
        rustix::process::Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    )
    .map_err(|e| PortalError::Failed(e.to_string()))?;
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
        .map_err(|e| PortalError::Failed(e.to_string()))?;
    tracing_subscriber::fmt()
        .with_max_level(if args.verbose {
            tracing::Level::DEBUG
        } else {
            tracing::Level::INFO
        })
        .init();
    let connection = zbus::connection::Builder::session()?
        .method_timeout(std::time::Duration::from_secs(305))
        .name(PORTAL_NAME)?
        .serve_at(PORTAL_PATH, Secret::default())?
        .build()
        .await?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = connection.closed() => {},
        _ = tokio::signal::ctrl_c() => {},
        _ = sigterm.recv() => {},
    }
    Ok(())
}

#[cfg(test)]
mod tests;
