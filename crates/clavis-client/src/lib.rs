use std::collections::BTreeMap;

pub use oo7;
use oo7::dbus::{Collection, Service};
use serde::Serialize;

pub const MAX_SECRET_BYTES: usize = 1024 * 1024;
pub type Attributes = BTreeMap<String, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interaction {
    Never,
    Allow,
}

#[derive(thiserror::Error)]
pub enum Error {
    #[error("The vault is locked")]
    Locked,
    #[error("No default vault exists; open Clavis to create one")]
    NoCollection,
    #[error("More than one credential matches these attributes")]
    Ambiguous,
    #[error("Credential verification failed")]
    Verification,
    #[error("Invalid credential metadata or excessive size")]
    InvalidInput,
    #[error("Secret Service could not complete the operation")]
    Service(oo7::dbus::Error),
    #[error("Could not communicate with Secret Service")]
    Transport(zbus::Error),
}

impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
impl From<oo7::dbus::Error> for Error {
    fn from(error: oo7::dbus::Error) -> Self {
        match error {
            oo7::dbus::Error::Service(oo7::dbus::ServiceError::IsLocked(_)) => Self::Locked,
            error => Self::Service(error),
        }
    }
}
impl From<zbus::Error> for Error {
    fn from(error: zbus::Error) -> Self {
        Self::Transport(error)
    }
}

pub struct Secret(oo7::Secret);
impl Secret {
    pub fn text(value: String) -> Self {
        Self(oo7::Secret::Text(value))
    }
    pub fn blob(value: Vec<u8>) -> Self {
        Self(oo7::Secret::Blob(value))
    }
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
    pub fn as_str(&self) -> Option<&str> {
        self.0.as_str()
    }
    pub fn into_inner(self) -> oo7::Secret {
        self.0
    }
}
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CollectionInfo {
    pub path: String,
    pub label: String,
    pub locked: bool,
    pub is_default: bool,
}
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ItemInfo {
    pub path: String,
    pub collection: String,
    pub label: String,
    pub attributes: Attributes,
    pub modified: u64,
}

pub struct Client {
    service: Service,
    connection: zbus::Connection,
    interaction: Interaction,
}
impl Client {
    pub async fn connect(interaction: Interaction) -> Result<Self, Error> {
        let connection = zbus::connection::Builder::session()?
            .method_timeout(std::time::Duration::from_secs(30))
            .build()
            .await?;
        Self::with_connection(connection, interaction).await
    }
    pub async fn with_connection(
        connection: zbus::Connection,
        interaction: Interaction,
    ) -> Result<Self, Error> {

        let service = Service::encrypted_with_connection(&connection).await?;
        Ok(Self {
            service,
            connection,
            interaction,
        })
    }
    pub fn connection(&self) -> &zbus::Connection {
        &self.connection
    }
    pub fn service(&self) -> &Service {
        &self.service
    }

    async fn ready(&self, collection: Collection) -> Result<Collection, Error> {
        if collection.is_locked().await? {
            if self.interaction == Interaction::Never {
                return Err(Error::Locked);
            }
            collection.unlock(None).await?;
            if collection.is_locked().await? {
                return Err(Error::Locked);
            }
        }
        Ok(collection)
    }
    async fn default(&self) -> Result<Collection, Error> {
        let collection = self
            .service
            .with_alias("default")
            .await?
            .ok_or(Error::NoCollection)?;
        self.ready(collection).await
    }
    async fn collection(&self, path: &str) -> Result<Collection, Error> {
        self.service
            .collections()
            .await?
            .into_iter()
            .find(|c| c.path().as_str() == path)
            .ok_or(Error::NoCollection)
    }
    pub async fn collections(&self) -> Result<Vec<CollectionInfo>, Error> {
        let default = self
            .service
            .with_alias("default")
            .await?
            .map(|c| c.path().to_string());
        let mut result = Vec::new();
        for c in self.service.collections().await? {
            result.push(CollectionInfo {
                path: c.path().to_string(),
                label: c.label().await?,
                locked: c.is_locked().await?,
                is_default: default.as_deref() == Some(c.path().as_str()),
            });
        }
        result.sort_by(|a, b| b.is_default.cmp(&a.is_default).then(a.label.cmp(&b.label)));
        Ok(result)
    }
    pub async fn items(&self, path: &str) -> Result<Vec<ItemInfo>, Error> {
        let c = self.ready(self.collection(path).await?).await?;
        let mut rows = Vec::new();
        for item in c.items().await? {
            rows.push(ItemInfo {
                path: item.path().to_string(),
                collection: path.to_owned(),
                label: item.label().await?,
                attributes: item.attributes().await?.into_iter().collect(),
                modified: item.modified().await?.as_secs(),
            });
        }
        rows.sort_by(|a, b| a.label.cmp(&b.label).then(a.path.cmp(&b.path)));
        Ok(rows)
    }
    pub async fn get(&self, attributes: &Attributes) -> Result<Option<Secret>, Error> {
        validate_attributes(attributes)?;
        let c = self.default().await?;
        let mut items = c.search_items(attributes).await?;
        if items.len() > 1 {
            return Err(Error::Ambiguous);
        }
        match items.pop() {
            Some(item) => Ok(Some(Secret(item.secret().await?))),
            None => Ok(None),
        }
    }
    pub async fn set(
        &self,
        label: &str,
        attributes: &Attributes,
        secret: Secret,
    ) -> Result<(), Error> {
        self.set_in(None, label, attributes, secret).await
    }
    pub async fn set_in(
        &self,
        collection: Option<&str>,
        label: &str,
        attributes: &Attributes,
        secret: Secret,
    ) -> Result<(), Error> {
        validate_input(label, attributes, secret.as_bytes())?;
        let c = match collection {
            Some(path) => self.ready(self.collection(path).await?).await?,
            None => self.default().await?,
        };
        if c.search_items(attributes).await?.len() > 1 {
            return Err(Error::Ambiguous);
        }

        let item = c
            .create_item(label, attributes, secret.0.clone(), true, None)
            .await?;
        let stored = item.secret().await?;
        if stored.as_bytes() != secret.as_bytes() {
            return Err(Error::Verification);
        }
        Ok(())
    }
    pub async fn delete(&self, attributes: &Attributes) -> Result<usize, Error> {
        validate_attributes(attributes)?;
        let c = self.default().await?;
        let items = c.search_items(attributes).await?;
        let count = items.len();
        for item in items {
            item.delete(None).await?;
        }
        Ok(count)
    }
    pub async fn read_item(&self, collection: &str, path: &str) -> Result<Option<Secret>, Error> {
        let c = self.ready(self.collection(collection).await?).await?;
        for item in c.items().await? {
            if item.path().as_str() == path {
                return Ok(Some(Secret(item.secret().await?)));
            }
        }
        Ok(None)
    }
    pub async fn delete_item(&self, collection: &str, path: &str) -> Result<bool, Error> {
        let c = self.ready(self.collection(collection).await?).await?;
        for item in c.items().await? {
            if item.path().as_str() == path {
                item.delete(None).await?;
                return Ok(true);
            }
        }
        Ok(false)
    }
    pub async fn lock(&self, path: &str) -> Result<(), Error> {
        self.collection(path).await?.lock(None).await?;
        Ok(())
    }
    pub async fn lock_all(&self) -> Result<(), Error> {

        let control = zbus::Proxy::new(
            &self.connection,
            "org.freedesktop.secrets",
            "/org/clavis/Control",
            "org.clavis.Control",
        )
        .await?;
        match control.call::<_, _, ()>("LockAll", &()).await {
            Ok(()) => return Ok(()),
            Err(zbus::Error::MethodError(name, _, _))
                if matches!(
                    name.as_str(),
                    "org.freedesktop.DBus.Error.UnknownMethod"
                        | "org.freedesktop.DBus.Error.UnknownInterface"
                        | "org.freedesktop.DBus.Error.UnknownObject"
                ) => {}
            Err(error) => return Err(error.into()),
        }
        for c in self.service.collections().await? {
            c.lock(None).await?;
        }
        Ok(())
    }
    pub async fn unlock(&self, path: &str) -> Result<(), Error> {
        if self.interaction == Interaction::Never {
            return Err(Error::Locked);
        }
        self.ready(self.collection(path).await?).await?;
        Ok(())
    }
    pub async fn create(&self, label: &str, make_default: bool) -> Result<String, Error> {
        if self.interaction == Interaction::Never || label.trim().is_empty() || label.len() > 256 {
            return Err(Error::InvalidInput);
        }
        let c = self
            .service
            .create_collection(label, make_default.then_some("default"), None)
            .await?;
        Ok(c.path().to_string())
    }
    pub async fn change_password(&self, path: &str) -> Result<(), Error> {
        if self.interaction == Interaction::Never {
            return Err(Error::Locked);
        }
        self.ready(self.collection(path).await?).await?;
        let control = zbus::Proxy::new(
            &self.connection,
            "org.freedesktop.secrets",
            "/org/clavis/Control",
            "org.clavis.Control",
        )
        .await?;
        let path: zbus::zvariant::OwnedObjectPath = path.try_into().map_err(zbus::Error::from)?;
        let prompt: zbus::zvariant::OwnedObjectPath =
            control.call("ChangePassword", &(path,)).await?;
        if let Some(prompt) = oo7::dbus::api::Prompt::new(&self.connection, prompt).await? {
            prompt.receive_completed(None).await?;
        }
        Ok(())
    }
}

pub fn credential_attributes(application: &str, key: &str) -> Result<Attributes, Error> {
    let values = BTreeMap::from([
        ("application".into(), application.into()),
        ("clavis.key".into(), key.into()),
    ]);
    validate_attributes(&values)?;
    Ok(values)
}
fn validate_attributes(a: &Attributes) -> Result<(), Error> {
    if a.is_empty()
        || a.len() > 32
        || a.iter().any(|(k, v)| {
            k.is_empty()
                || k.len() > 256
                || v.is_empty()
                || v.len() > 4096
                || k.contains('\0')
                || v.contains('\0')
        })
    {
        return Err(Error::InvalidInput);
    }
    Ok(())
}
fn validate_input(label: &str, a: &Attributes, value: &[u8]) -> Result<(), Error> {
    validate_attributes(a)?;
    if label.trim().is_empty()
        || label.len() > 256
        || label.contains('\0')
        || value.len() > MAX_SECRET_BYTES
    {
        return Err(Error::InvalidInput);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secret_diagnostics_never_contain_value() {
        let secret = Secret::text("fixture-private-value".into());
        assert_eq!(format!("{secret:?}"), "Secret([REDACTED])");
    }
    #[test]
    fn empty_match_cannot_delete_entire_vault() {
        assert!(matches!(
            validate_attributes(&Attributes::new()),
            Err(Error::InvalidInput)
        ));
    }
    #[test]
    fn credential_names_are_scoped() {
        assert_ne!(
            credential_attributes("browser", "profile").unwrap(),
            credential_attributes("steam", "profile").unwrap()
        );
        assert!(credential_attributes("", "profile").is_err());
    }
    #[test]
    fn malicious_or_excessive_input_is_rejected() {
        assert!(credential_attributes("app\0id", "key").is_err());
        let attrs = credential_attributes("app", "key").unwrap();
        assert!(validate_input("label", &attrs, &vec![0; MAX_SECRET_BYTES + 1]).is_err());
        assert!(validate_input("", &attrs, b"secret").is_err());
    }
    #[test]
    fn remote_errors_cannot_leak_through_display_debug_or_error_source() {
        let error = Error::from(oo7::dbus::Error::NotFound("dummy-bearer-never-log".into()));
        assert!(!format!("{error} {error:?}").contains("dummy-bearer"));
        assert!(std::error::Error::source(&error).is_none());
    }
}
