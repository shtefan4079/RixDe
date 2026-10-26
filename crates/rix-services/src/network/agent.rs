use super::{model::CredentialRequest, profiles, vault, Settings};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::{oneshot, watch};
use zbus::{
    zvariant::{OwnedObjectPath, Value},
    Connection,
};

#[derive(zbus::DBusError, Debug)]
#[zbus(prefix = "org.freedesktop.NetworkManager.SecretAgent")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    NoSecrets(String),
    UserCanceled(String),
    InternalError(String),
    PermissionDenied(String),
}
struct Pending {
    request: CredentialRequest,
    visible: bool,
    reply: oneshot::Sender<Option<Settings>>,
}
struct Inner {
    pending: Mutex<HashMap<u64, Pending>>,
    cache: Mutex<HashMap<String, Settings>>,
    next: AtomicU64,
    updates: watch::Sender<Vec<CredentialRequest>>,
}
#[derive(Clone)]
pub(super) struct Control {
    inner: Arc<Inner>,
    pub updates: watch::Receiver<Vec<CredentialRequest>>,
}
impl Control {
    pub fn new() -> Self {
        let (updates, receiver) = watch::channel(Vec::new());
        Self {
            inner: Arc::new(Inner {
                pending: Mutex::new(HashMap::new()),
                cache: Mutex::new(HashMap::new()),
                next: AtomicU64::new(1),
                updates,
            }),
            updates: receiver,
        }
    }
    fn publish(&self) {
        let mut requests = self
            .inner
            .pending
            .lock()
            .unwrap()
            .values()
            .filter(|p| p.visible)
            .map(|p| p.request.clone())
            .collect::<Vec<_>>();
        requests.sort_by_key(|p| p.id);
        self.inner.updates.send_replace(requests);
    }
    pub fn clear(&self) {
        self.inner.pending.lock().unwrap().clear();
        self.inner.cache.lock().unwrap().clear();
        self.publish();
    }
    pub fn reply(
        &self,
        id: u64,
        username: String,
        password: String,
        key_password: String,
        extra: String,
        cancel: bool,
    ) -> anyhow::Result<()> {
        let request = self
            .inner
            .pending
            .lock()
            .unwrap()
            .get(&id)
            .map(|p| p.request.clone())
            .ok_or_else(|| anyhow::anyhow!("This password request has expired"))?;
        let answer = if cancel {
            None
        } else {
            let mut secret = HashMap::new();
            if request.setting_name == "vpn" {
                let mut map = profiles::map_data(&extra)?;
                if !password.is_empty() {
                    let key = request
                        .hints
                        .iter()
                        .find(|h| !h.starts_with("x-vpn-") && h.as_str() != "username")
                        .map(String::as_str)
                        .unwrap_or("password");
                    map.insert(key.into(), password);
                }
                if !username.is_empty() {
                    map.insert("username".into(), username);
                }
                if map.is_empty() {
                    anyhow::bail!("Enter the requested VPN credentials");
                }
                secret.insert("secrets".into(), Value::from(map).try_to_owned()?);
            } else {
                if !username.is_empty() {
                    secret.insert(
                        if request.setting_name == "802-1x" {
                            "identity"
                        } else {
                            "username"
                        }
                        .into(),
                        profiles::string(&username),
                    );
                }
                if !password.is_empty() {
                    let key = if request.setting_name == "802-11-wireless-security" {
                        request
                            .hints
                            .iter()
                            .find(|v| {
                                matches!(
                                    v.as_str(),
                                    "psk"
                                        | "wep-key0"
                                        | "wep-key1"
                                        | "wep-key2"
                                        | "wep-key3"
                                        | "leap-password"
                                )
                            })
                            .map(String::as_str)
                            .unwrap_or("psk")
                    } else if request.setting_name == "wireguard" {
                        "private-key"
                    } else if request.hints.iter().any(|h| h == "pin") {
                        "pin"
                    } else {
                        "password"
                    };
                    secret.insert(key.into(), profiles::string(&password));
                }
                if !key_password.is_empty() {
                    secret.insert(
                        "private-key-password".into(),
                        profiles::string(&key_password),
                    );
                }
                if secret.is_empty() {
                    anyhow::bail!("Enter the requested network credentials");
                }
            }
            Some(HashMap::from([(request.setting_name.clone(), secret)]))
        };
        let pending = self
            .inner
            .pending
            .lock()
            .unwrap()
            .remove(&id)
            .ok_or_else(|| anyhow::anyhow!("This password request has expired"))?;
        let _ = pending.reply.send(answer);
        self.publish();
        Ok(())
    }
    pub async fn export(&self, connection: &Connection) -> anyhow::Result<()> {
        connection
            .object_server()
            .at(
                "/org/freedesktop/NetworkManager/SecretAgent",
                Agent(self.clone()),
            )
            .await?;
        Ok(())
    }
    pub async fn register(&self, connection: &Connection) -> anyhow::Result<()> {
        super::proxy(
            connection,
            "/org/freedesktop/NetworkManager/AgentManager",
            "org.freedesktop.NetworkManager.AgentManager",
        )
        .await?
        .call::<_, _, ()>(
            "RegisterWithCapabilities",
            &(
                format!("org.rixde.NetworkAgent.p{}", std::process::id()),
                1u32,
            ),
        )
        .await?;
        Ok(())
    }
}
struct Agent(Control);
async fn authorize(bus: &Connection, header: &zbus::message::Header<'_>) -> Result<(), AgentError> {
    let dbus = zbus::Proxy::new(
        bus,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await?;
    let owner: String = dbus.call("GetNameOwner", &(super::NM,)).await?;
    if header.sender().map(|s| s.as_str()) != Some(owner.as_str()) {
        return Err(AgentError::PermissionDenied(
            "Only NetworkManager may request network credentials".into(),
        ));
    }
    Ok(())
}
#[zbus::interface(name = "org.freedesktop.NetworkManager.SecretAgent")]
impl Agent {

    #[allow(clippy::too_many_arguments)]
    async fn get_secrets(
        &self,
        connection: Settings,
        connection_path: OwnedObjectPath,
        setting_name: String,
        hints: Vec<String>,
        flags: u32,
        #[zbus(connection)] bus: &Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<Settings, AgentError> {
        authorize(bus, &header).await?;
        let required_hints = hints.clone();
        let needs_identity = setting_name == "802-1x"
            && profiles::str_setting(&connection, "802-1x", "identity").is_none();
        if flags & 2 != 0 {
            self.0
                .inner
                .cache
                .lock()
                .unwrap()
                .remove(connection_path.as_str());
        }
        if flags & 2 == 0 {
            if let Some(cached) = self
                .0
                .inner
                .cache
                .lock()
                .unwrap()
                .get(connection_path.as_str())
                .and_then(|s| s.get(&setting_name))
            {
                let answer = HashMap::from([(
                    setting_name.clone(),
                    cached
                        .iter()
                        .filter_map(|(k, v)| v.try_clone().ok().map(|v| (k.clone(), v)))
                        .collect(),
                )]);
                let remembered = vault::cached(&connection, &answer);
                if !remembered.is_empty()
                    && (!needs_identity || remembered[&setting_name].contains_key("identity"))
                    && vault::satisfies(&remembered, &setting_name, &required_hints)
                {
                    return Ok(remembered);
                }
            }
        }
        let id = self.0.inner.next.fetch_add(1, Ordering::Relaxed);
        let (replier, mut answer) = oneshot::channel();
        let is_key = hints.iter().any(|v| v == "private-key-password");
        let request = CredentialRequest {
            id,
            profile_path: connection_path.to_string(),
            profile_name: profiles::str_setting(&connection, "connection", "id")
                .unwrap_or("Network")
                .into(),
            setting_name: setting_name.clone(),
            hints: hints
                .into_iter()
                .take(32)
                .map(|s| s.chars().take(256).collect())
                .collect(),
            message: if flags & 2 != 0 {
                "The saved credentials were rejected. Enter new credentials"
            } else {
                "This network needs credentials to connect"
            }
            .into(),
            username: setting_name == "802-1x"
                && profiles::str_setting(&connection, "802-1x", "identity").is_none(),
            password: !is_key || setting_name == "vpn",
            private_key_password: is_key,
            retry: flags & 2 != 0,
        };
        {
            let mut pending = self.0.inner.pending.lock().unwrap();
            if pending.len() >= 8 {
                return Err(AgentError::InternalError(
                    "Too many pending credential requests".into(),
                ));
            }
            pending.insert(
                id,
                Pending {
                    request,
                    visible: false,
                    reply: replier,
                },
            );
        }

        if flags & 2 == 0 && !needs_identity {
            let stored = tokio::select! {
                stored = vault::load(&connection, &setting_name) => stored,
                _ = &mut answer => {
                    self.0.inner.pending.lock().unwrap().remove(&id);
                    return Err(AgentError::UserCanceled("The credential request was cancelled".into()));
                }
            };
            if let Some(stored) =
                stored.filter(|stored| vault::satisfies(stored, &setting_name, &required_hints))
            {
                if self.0.inner.pending.lock().unwrap().remove(&id).is_none() {
                    return Err(AgentError::UserCanceled(
                        "The credential request was cancelled".into(),
                    ));
                }
                self.0.publish();
                return Ok(stored);
            }
        }
        if flags & 3 == 0 {
            self.0.inner.pending.lock().unwrap().remove(&id);
            return Err(AgentError::NoSecrets(
                "No unlocked saved credentials are available".into(),
            ));
        }
        if let Some(pending) = self.0.inner.pending.lock().unwrap().get_mut(&id) {
            pending.visible = true;
        } else {
            return Err(AgentError::UserCanceled(
                "The credential request was cancelled".into(),
            ));
        }
        self.0.publish();
        let result = tokio::time::timeout(std::time::Duration::from_secs(120), answer).await;
        self.0.inner.pending.lock().unwrap().remove(&id);
        self.0.publish();
        match result {
            Ok(Ok(Some(settings))) => {

                let cached = vault::cached(&connection, &settings);
                if !cached.is_empty() {
                    let mut cache = self.0.inner.cache.lock().unwrap();
                    if cache.len() >= 32 {
                        cache.clear();
                    }
                    cache.insert(connection_path.to_string(), cached);
                }
                vault::save(&connection, &settings).await;
                Ok(settings)
            }
            _ => Err(AgentError::UserCanceled(
                "The credential request was cancelled".into(),
            )),
        }
    }
    async fn cancel_get_secrets(
        &self,
        connection_path: OwnedObjectPath,
        setting_name: String,
        #[zbus(connection)] bus: &Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<(), AgentError> {
        authorize(bus, &header).await?;
        self.0.inner.pending.lock().unwrap().retain(|_, p| {
            !(p.request.profile_path == connection_path.as_str()
                && p.request.setting_name == setting_name)
        });
        self.0.publish();
        Ok(())
    }
    async fn save_secrets(
        &self,
        connection: Settings,
        connection_path: OwnedObjectPath,
        #[zbus(connection)] bus: &Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<(), AgentError> {
        authorize(bus, &header).await?;

        let secrets = vault::cached(&connection, &connection);
        {
            let mut cache = self.0.inner.cache.lock().unwrap();
            if cache.len() >= 32 {
                cache.clear();
            }
            if secrets.is_empty() {
                cache.remove(connection_path.as_str());
            } else {
                cache.insert(connection_path.to_string(), secrets);
            }
        }
        vault::save(&connection, &connection).await;
        Ok(())
    }
    async fn delete_secrets(
        &self,
        connection: Settings,
        connection_path: OwnedObjectPath,
        #[zbus(connection)] bus: &Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<(), AgentError> {
        authorize(bus, &header).await?;
        self.0
            .inner
            .cache
            .lock()
            .unwrap()
            .remove(connection_path.as_str());
        vault::delete(&connection).await;
        Ok(())
    }
}
