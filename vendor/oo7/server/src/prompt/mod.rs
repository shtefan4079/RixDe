// org.freedesktop.Secret.Prompt

use std::{
    future::Future,
    os::fd::AsFd,
    pin::Pin,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use oo7::{Secret, dbus::ServiceError};
use tokio::{
    io::AsyncReadExt,
    sync::{Mutex, OnceCell},
};
use zbus::{
    interface,
    object_server::SignalEmitter,
    zvariant::{ObjectPath, Optional, OwnedObjectPath, OwnedValue},
};

#[cfg(any(feature = "gnome_native_crypto", feature = "gnome_openssl_crypto"))]
use crate::gnome::prompter::{GNOMEPrompterCallback, GNOMEPrompterProxy};
#[cfg(any(feature = "plasma_native_crypto", feature = "plasma_openssl_crypto"))]
use crate::plasma::prompter::PlasmaPrompterCallback;
use crate::{
    error::custom_service_error,
    service::{PrompterType, Service},
};

#[zbus::proxy(
    interface = "org.freedesktop.secrets.CliPrompter",
    default_service = "org.freedesktop.secrets.CliPrompter",
    default_path = "/org/freedesktop/secrets/CliPrompter"
)]
trait CliPrompter {
    async fn prompt(
        &self,
        label: &str,
        description: &str,
    ) -> zbus::Result<(zbus::zvariant::OwnedFd, bool)>;
}

#[zbus::proxy(
    interface = "org.clavis.Prompter",
    default_service = "org.freedesktop.secrets.CliPrompter",
    default_path = "/org/clavis/Prompter"
)]
trait ClavisPrompter {
    #[zbus(no_autostart)]
    async fn cancel(&self) -> zbus::Result<()>;
    async fn prompt(
        &self,
        role: &str,
        label: &str,
        description: &str,
    ) -> zbus::Result<(zbus::zvariant::OwnedFd, bool)>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptRole {
    Unlock,
    CreateCollection,
    ChangePassword,
}

/// The result of the originating D-Bus operation, independent of the prompt UI.
/// Unlocking for CreateItem, for example, returns one path rather than an array.
#[derive(Debug, Clone, Copy)]
pub enum PromptResultType {
    ObjectPath,
    ObjectPaths,
}

/// A boxed future that represents the action to be taken when a prompt
/// completes
pub type PromptActionFuture =
    Pin<Box<dyn Future<Output = Result<OwnedValue, ServiceError>> + Send + 'static>>;

/// Represents the action to be taken when a prompt completes
pub struct PromptAction {
    /// The async function to execute when the prompt is accepted
    action: Box<dyn FnOnce(Option<Secret>) -> PromptActionFuture + Send>,
}

impl PromptAction {
    /// Create a new prompt action from a closure that takes an optional secret
    /// and returns a future
    pub fn new<F, Fut>(f: F) -> Self
    where
        F: FnOnce(Option<Secret>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<OwnedValue, ServiceError>> + Send + 'static,
    {
        Self {
            action: Box::new(move |secret| Box::pin(f(secret))),
        }
    }

    /// Execute the action with the provided secret
    pub async fn execute(self, secret: Option<Secret>) -> Result<OwnedValue, ServiceError> {
        (self.action)(secret).await
    }
}

#[derive(Clone)]
pub struct Prompt {
    service: Service,
    role: PromptRole,
    result_type: PromptResultType,
    path: OwnedObjectPath,
    /// The label of the collection/keyring being prompted for
    label: String,
    /// The collection for Unlock prompts (needed for secret validation)
    collection: Option<crate::collection::Collection>,
    /// GNOME Specific
    #[cfg(any(feature = "gnome_native_crypto", feature = "gnome_openssl_crypto"))]
    gnome_callback: Arc<OnceCell<GNOMEPrompterCallback>>,
    /// KDE Plasma Specific
    #[cfg(any(feature = "plasma_native_crypto", feature = "plasma_openssl_crypto"))]
    plasma_callback: Arc<OnceCell<PlasmaPrompterCallback>>,
    /// The action to execute when the prompt completes
    action: Arc<Mutex<Option<PromptAction>>>,
    owner: Arc<Mutex<Option<zbus::names::OwnedUniqueName>>>,
    started: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    task: Arc<Mutex<Option<tokio::task::AbortHandle>>>,
}

#[cfg(any(
    feature = "gnome_openssl_crypto",
    feature = "gnome_native_crypto",
    feature = "plasma_native_crypto",
    feature = "plasma_openssl_crypto"
))] // User has to enable at least one prompt backend
#[interface(name = "org.freedesktop.Secret.Prompt")]
impl Prompt {
    pub async fn prompt(
        &self,
        window_id: Optional<&str>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<(), ServiceError> {
        self.check_owner(header.sender()).await?;
        if self.started.swap(true, Ordering::AcqRel) {
            return Err(custom_service_error(
                "A prompt callback is ongoing already.",
            ));
        }
        let window_id = (*window_id).and_then(|w| ashpd::WindowIdentifierType::from_str(w).ok());
        let peer_info = match header.sender() {
            Some(sender) => self
                .service
                .session_from_sender(sender)
                .await
                .and_then(|s| s.peer_info().cloned()),
            None => None,
        };

        match self.service.prompter_type(peer_info.as_ref()).await {
            #[cfg(any(feature = "plasma_native_crypto", feature = "plasma_openssl_crypto"))]
            PrompterType::Plasma => self.prompt_plasma(window_id).await,
            #[cfg(any(feature = "gnome_native_crypto", feature = "gnome_openssl_crypto"))]
            PrompterType::GNOME => self.prompt_gnome(window_id).await,
            PrompterType::Cli => {
                let prompt = self.clone();
                let task = tokio::spawn(async move {
                    if let Err(error) = prompt.prompt_cli().await {
                        tracing::warn!("Clavis prompt failed: {error}");
                        let _ = prompt.finish_dismissed().await;
                    }
                });
                *self.task.lock().await = Some(task.abort_handle());
                Ok(())
            }
            #[allow(unreachable_patterns)]
            _ => Err(custom_service_error(
                "No prompt backend available in the current environment.",
            )),
        }
    }

    pub async fn dismiss(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<(), ServiceError> {
        self.check_owner(header.sender()).await?;
        if let Some(task) = self.task.lock().await.take() {
            task.abort();
        }
        self.cancel_ui().await;
        self.finish_dismissed().await?;
        #[cfg(any(feature = "plasma_native_crypto", feature = "plasma_openssl_crypto"))]
        if let Some(callback) = self.plasma_callback.get() {
            let emitter = SignalEmitter::from_parts(
                self.service.connection().clone(),
                callback.path().clone(),
            );
            PlasmaPrompterCallback::dismiss(&emitter).await?;
        }

        #[cfg(any(feature = "gnome_native_crypto", feature = "gnome_openssl_crypto"))]
        if let Some(_callback) = self.gnome_callback.get() {
            // TODO: figure out if we should destroy the un-export the callback
            // here?
        }

        Ok(())
    }

    #[zbus(signal, name = "Completed")]
    pub async fn completed(
        signal_emitter: &SignalEmitter<'_>,
        dismissed: bool,
        result: OwnedValue,
    ) -> zbus::Result<()>;
}

impl Prompt {
    pub async fn new(
        service: Service,
        role: PromptRole,
        result_type: PromptResultType,
        label: String,
        collection: Option<crate::collection::Collection>,
    ) -> Self {
        let index = service.prompt_index();
        Self {
            path: OwnedObjectPath::try_from(format!("/org/freedesktop/secrets/prompt/p{index}"))
                .unwrap(),
            service,
            role,
            result_type,
            label,
            collection,
            #[cfg(any(feature = "gnome_native_crypto", feature = "gnome_openssl_crypto"))]
            gnome_callback: Default::default(),
            #[cfg(any(feature = "plasma_native_crypto", feature = "plasma_openssl_crypto"))]
            plasma_callback: Default::default(),
            action: Arc::new(Mutex::new(None)),
            owner: Default::default(),
            started: Default::default(),
            finished: Default::default(),
            task: Default::default(),
        }
    }

    pub async fn set_owner(&self, sender: Option<&zbus::names::UniqueName<'_>>) {
        if let Some(sender) = sender {
            *self.owner.lock().await = Some(sender.to_owned().into());
        }
    }

    async fn check_owner(
        &self,
        sender: Option<&zbus::names::UniqueName<'_>>,
    ) -> Result<(), ServiceError> {
        let owner = self.owner.lock().await;
        if owner
            .as_ref()
            .is_some_and(|value| sender.is_none_or(|sender| value.as_str() != sender.as_str()))
        {
            return Err(custom_service_error(
                "Prompt belongs to a different D-Bus client",
            ));
        }
        Ok(())
    }

    async fn finish(&self, dismissed: bool, result: OwnedValue) -> Result<(), ServiceError> {
        if self.finished.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let emitter = self.service.signal_emitter(self.path().to_owned())?;
        Prompt::completed(&emitter, dismissed, result).await?;
        self.service.remove_prompt(self.path()).await;
        // Removing from the object server during Dismiss would wait for the active
        // method guard. Let it complete first; completed makes subsequent calls inert.
        let service = self.service.clone();
        let path = self.path.clone();
        tokio::spawn(async move {
            let _ = service.object_server().remove::<Prompt, _>(&path).await;
        });
        Ok(())
    }

    pub(crate) async fn cancel(&self) -> Result<(), ServiceError> {
        if let Some(task) = self.task.lock().await.take() {
            task.abort();
        }
        self.cancel_ui().await;
        self.finish_dismissed().await
    }

    async fn cancel_ui(&self) {
        let connection = self.service.connection();
        if let Ok(proxy) = ClavisPrompterProxy::new(&connection).await {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), proxy.cancel()).await;
        }
    }

    pub(crate) async fn finish_dismissed(&self) -> Result<(), ServiceError> {
        // libsecret validates the result type even when dismissed is true.
        let result = match self.result_type {
            PromptResultType::ObjectPath => OwnedValue::from(ObjectPath::default()),
            PromptResultType::ObjectPaths => {
                zbus::zvariant::Value::new(Vec::<OwnedObjectPath>::new())
                    .try_into_owned()
                    .unwrap()
            }
        };
        self.finish(true, result).await
    }

    pub fn path(&self) -> &ObjectPath<'_> {
        &self.path
    }

    pub fn role(&self) -> PromptRole {
        self.role
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    fn collection(&self) -> Option<&crate::collection::Collection> {
        self.collection.as_ref()
    }

    /// Set the action to execute when the prompt completes
    pub async fn set_action(&self, action: PromptAction) {
        *self.action.lock().await = Some(action);
    }

    /// Take the action, consuming it so it can only be executed once
    async fn take_action(&self) -> Option<PromptAction> {
        self.action.lock().await.take()
    }

    pub async fn on_unlock_collection(&self, secret: Option<Secret>) -> Result<bool, ServiceError> {
        debug_assert_eq!(self.role, PromptRole::Unlock);

        // Get the collection to validate the secret
        let collection = self.collection().expect("Unlock requires a collection");
        let label = self.label();

        let is_valid = if let Some(ref secret) = secret {
            // Validate the secret using the already-open keyring
            let keyring_guard = collection.keyring.read().await;
            let valid = keyring_guard
                .as_ref()
                .unwrap()
                .validate_secret(secret)
                .await
                .map_err(|err| {
                    custom_service_error(&format!(
                        "Failed to validate secret for {label} keyring: {err}."
                    ))
                })?;
            drop(keyring_guard);
            valid
        } else {
            // No secret means unencrypted -> validate items are plaintext
            let keyring_guard = collection.keyring.read().await;
            let valid = keyring_guard
                .as_ref()
                .unwrap()
                .validate_unencrypted()
                .await
                .map_err(|err| {
                    custom_service_error(&format!(
                        "Failed to validate unencrypted keyring {label}: {err}."
                    ))
                })?;
            drop(keyring_guard);
            valid
        };

        if is_valid {
            tracing::debug!("Keyring secret matches for {label}.");

            let Some(action) = self.take_action().await else {
                return Err(custom_service_error(
                    "Prompt action was already executed or not set",
                ));
            };

            // Execute the unlock action after successful validation
            let result_value = action.execute(secret).await?;

            self.finish(false, result_value).await?;
            Ok(true)
        } else {
            tracing::error!("Keyring {label} failed to unlock, incorrect secret.");

            Ok(false)
        }
    }

    pub async fn on_create_collection(&self, secret: Option<Secret>) -> Result<(), ServiceError> {
        if secret.as_ref().is_none_or(|secret| secret.is_empty()) {
            return Err(custom_service_error(
                "Clavis requires a nonempty encryption password",
            ));
        }
        debug_assert_eq!(self.role, PromptRole::CreateCollection);

        let Some(action) = self.take_action().await else {
            return Err(custom_service_error(
                "Prompt action was already executed or not set",
            ));
        };

        // Execute the collection creation action with the secret
        match action.execute(secret).await {
            Ok(collection_path_value) => {
                tracing::info!("CreateCollection action completed successfully");

                self.finish(false, collection_path_value).await?;
                Ok(())
            }
            Err(err) => Err(custom_service_error(&format!(
                "Failed to create collection: {err}."
            ))),
        }
    }

    pub async fn on_change_password(&self, secret: Option<Secret>) -> Result<(), ServiceError> {
        if secret.as_ref().is_none_or(|secret| secret.is_empty()) {
            return Err(custom_service_error(
                "Clavis requires a nonempty encryption password",
            ));
        }
        debug_assert_eq!(self.role, PromptRole::ChangePassword);

        let Some(action) = self.take_action().await else {
            return Err(custom_service_error(
                "Prompt action was already executed or not set",
            ));
        };

        // Execute the change password action with the new secret
        match action.execute(secret).await {
            Ok(result) => {
                tracing::info!("ChangePassword action completed successfully");

                self.finish(false, result).await?;
                Ok(())
            }
            Err(err) => Err(custom_service_error(&format!(
                "Failed to change password: {err}."
            ))),
        }
    }

    #[cfg(any(feature = "plasma_native_crypto", feature = "plasma_openssl_crypto"))]
    async fn prompt_plasma(
        &self,
        window_id: Option<ashpd::WindowIdentifierType>,
    ) -> Result<(), ServiceError> {
        if self.plasma_callback.get().is_some() {
            return Err(custom_service_error(
                "A prompt callback is ongoing already.",
            ));
        }

        let callback = PlasmaPrompterCallback::new(self.service.clone(), self.path.clone()).await;
        let path = OwnedObjectPath::from(callback.path().clone());

        let _ = self.plasma_callback.set(callback.clone());
        self.service
            .object_server()
            .at(&path, callback.clone())
            .await?;
        tracing::debug!("Prompt `{}` created.", self.path);

        callback.start(&self.role, window_id, &self.label).await
    }

    #[cfg(any(feature = "gnome_native_crypto", feature = "gnome_openssl_crypto"))]
    async fn prompt_gnome(
        &self,
        window_id: Option<ashpd::WindowIdentifierType>,
    ) -> Result<(), ServiceError> {
        if self.gnome_callback.get().is_some() {
            return Err(custom_service_error(
                "A GNOME prompt callback is ongoing already.",
            ));
        };

        let callback =
            GNOMEPrompterCallback::new(window_id, self.service.clone(), self.path.clone())
                .await
                .map_err(|err| {
                    custom_service_error(&format!("Failed to create GNOMEPrompterCallback {err}."))
                })?;

        let path = OwnedObjectPath::from(callback.path().clone());

        let _ = self.gnome_callback.set(callback.clone());
        self.service.object_server().at(&path, callback).await?;
        tracing::debug!("Prompt `{}` created.", self.path);

        let prompter = GNOMEPrompterProxy::new(&self.service.connection()).await?;
        tokio::spawn(async move { prompter.begin_prompting(&path).await });

        Ok(())
    }

    async fn prompt_cli(&self) -> Result<(), ServiceError> {
        let connection = self.service.connection();
        let native = ClavisPrompterProxy::new(&connection).await?;
        let legacy = CliPrompterProxy::new(&connection).await?;
        let role = match self.role {
            PromptRole::Unlock => "unlock",
            PromptRole::CreateCollection => "create",
            PromptRole::ChangePassword => "change",
        };
        let base_description = match self.role {
            PromptRole::Unlock => format!(
                "An application wants access to the locked collection ‘{}’.",
                self.label
            ),
            PromptRole::CreateCollection => format!(
                "Choose an encryption password for the new collection ‘{}’.",
                self.label
            ),
            PromptRole::ChangePassword => {
                format!("Choose a new encryption password for ‘{}’.", self.label)
            }
        };
        let mut description = base_description.clone();
        for _ in 0..64 {
            let response = tokio::time::timeout(std::time::Duration::from_secs(300), async {
                match native.prompt(role, &self.label, &description).await {
                    Err(zbus::Error::MethodError(name, _, _))
                        if matches!(
                            name.as_str(),
                            "org.freedesktop.DBus.Error.UnknownMethod"
                                | "org.freedesktop.DBus.Error.UnknownInterface"
                        ) =>
                    {
                        legacy.prompt(&self.label, &description).await
                    }
                    response => response,
                }
            })
            .await
            .map_err(|_| custom_service_error("Prompt timed out"))??;
            if !response.1 {
                return self.finish_dismissed().await;
            }
            let std_stream = std::os::unix::net::UnixStream::from(
                response.0.as_fd().try_clone_to_owned().map_err(|e| {
                    custom_service_error(&format!("Invalid prompt descriptor: {e}"))
                })?,
            );
            std_stream
                .set_nonblocking(true)
                .map_err(|e| custom_service_error(&format!("Invalid prompt socket: {e}")))?;
            let stream = tokio::net::UnixStream::from_std(std_stream)
                .map_err(|e| custom_service_error(&format!("Invalid prompt socket: {e}")))?;
            let mut bytes = zeroize::Zeroizing::new(Vec::new());
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                stream.take(65_537).read_to_end(&mut bytes),
            )
            .await
            .map_err(|_| custom_service_error("Prompt secret transfer timed out"))?
            .map_err(|e| custom_service_error(&format!("Prompt secret transfer failed: {e}")))?;
            if bytes.len() > 65_536 {
                return Err(custom_service_error("Prompt secret exceeds maximum length"));
            }
            if bytes.is_empty() {
                description = format!("An encryption password is required.\n\n{base_description}");
                continue;
            }
            let secret = Some(Secret::from(std::mem::take(&mut *bytes)));
            match self.role {
                PromptRole::Unlock => {
                    if self.on_unlock_collection(secret).await? {
                        return Ok(());
                    }
                    description =
                        format!("The unlock password was incorrect.\n\n{base_description}");
                }
                PromptRole::CreateCollection => {
                    self.on_create_collection(secret).await?;
                    return Ok(());
                }
                PromptRole::ChangePassword => {
                    self.on_change_password(secret).await?;
                    return Ok(());
                }
            }
        }
        self.finish_dismissed().await
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod clavis_tests;
