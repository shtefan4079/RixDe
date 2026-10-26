use crate::{
    apply_manager_theme, apply_prompt_theme, CredentialRow, PasswordWindow, VaultRow, VaultWindow,
};
use anyhow::Result;
use clavis_client::{credential_attributes, Client, CollectionInfo, Interaction, ItemInfo, Secret};
use rix_ipc::{IpcClient, IpcEvent, IpcRequest, IpcResponse};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::{
    path::PathBuf,
    rc::Rc,
    sync::mpsc,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

pub fn select_backend() -> Result<()> {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .select()?;
    slint::set_xdg_app_id("org.clavis.Clavis")?;
    Ok(())
}
pub fn load_theme() -> rix_config::ThemeSettings {
    let path = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    path.and_then(|p| std::fs::read(p.join("rix/theme.json")).ok())
        .and_then(|b| serde_json::from_slice::<rix_config::ThemeSettings>(&b).ok())
        .filter(|t| t.validate().is_ok())
        .unwrap_or_default()
}
fn theme_stream(mut apply: impl FnMut(rix_config::ThemeSettings) -> bool + Send + 'static) {
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        runtime.block_on(async move {
            loop {
                if let Ok(mut subscription) = IpcClient::subscribe().await {
                    if let Ok(mut client) = IpcClient::connect().await {
                        if let Ok(IpcResponse::Theme(theme)) =
                            client.send(IpcRequest::GetTheme).await
                        {
                            if !apply(theme) {
                                return;
                            }
                        }
                    }
                    while let Ok(event) = subscription.next_event().await {
                        if let IpcEvent::ThemeChanged(theme) = event {
                            if !apply(theme) {
                                return;
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        });
    });
}
pub fn watch_prompt_theme(weak: slint::Weak<PasswordWindow>) {
    theme_stream(move |theme| {
        weak.upgrade_in_event_loop(move |ui| apply_prompt_theme(&ui, &theme))
            .is_ok()
    });
}

enum Command {
    Refresh(String),
    SelectVault(String),
    SelectItem(String),
    Lock,
    Unlock,
    ChangePassword,
    Create(String),
    Save {
        label: String,
        application: String,
        key: String,
        secret: Secret,
    },
    Reveal,
    Delete,
    Backup,
}
struct State {
    vault: Option<String>,
    item: Option<String>,
    search: String,
    notice: String,
}
struct Snapshot {
    vaults: Vec<CollectionInfo>,
    items: Vec<ItemInfo>,
    vault: Option<String>,
    item: Option<String>,
    locked: bool,
    notice: String,
}
impl State {
    async fn snapshot(&mut self, client: &Client) -> Result<Snapshot> {
        let vaults = client
            .collections()
            .await?
            .into_iter()
            .filter(|c| c.path != "/org/freedesktop/secrets/collection/session")
            .collect::<Vec<_>>();
        if !vaults
            .iter()
            .any(|v| self.vault.as_deref() == Some(&v.path))
        {
            self.vault = vaults
                .iter()
                .find(|v| v.is_default)
                .or(vaults.first())
                .map(|v| v.path.clone());
            self.item = None;
        }
        let locked = vaults
            .iter()
            .find(|v| self.vault.as_deref() == Some(&v.path))
            .is_none_or(|v| v.locked);
        let mut items = if !locked {
            client.items(self.vault.as_deref().unwrap()).await?
        } else {
            Vec::new()
        };
        let query = self.search.to_lowercase();
        items.retain(|i| {
            query.is_empty()
                || i.label.to_lowercase().contains(&query)
                || i.attributes.iter().any(|(k, v)| {
                    k.to_lowercase().contains(&query) || v.to_lowercase().contains(&query)
                })
        });
        if !items.iter().any(|i| self.item.as_deref() == Some(&i.path)) {
            self.item = None;
        }
        Ok(Snapshot {
            vaults,
            items,
            vault: self.vault.clone(),
            item: self.item.clone(),
            locked,
            notice: self.notice.clone(),
        })
    }
}
fn apply_snapshot(ui: &VaultWindow, snapshot: Snapshot) {
    let old_vault = usize::try_from(ui.get_selected_vault())
        .ok()
        .and_then(|index| ui.get_vaults().row_data(index))
        .map(|row| row.path.to_string());
    let old_item = usize::try_from(ui.get_selected_item())
        .ok()
        .and_then(|index| ui.get_credentials().row_data(index))
        .map(|row| row.path.to_string());
    if snapshot.locked || old_vault != snapshot.vault || old_item != snapshot.item {
        ui.invoke_clear_secret();
    }
    let selected_vault = snapshot
        .vaults
        .iter()
        .position(|v| Some(&v.path) == snapshot.vault.as_ref())
        .map_or(-1, |i| i as i32);
    let selected_item = snapshot
        .items
        .iter()
        .position(|v| Some(&v.path) == snapshot.item.as_ref())
        .map_or(-1, |i| i as i32);
    ui.set_vaults(ModelRc::from(Rc::new(VecModel::from(
        snapshot
            .vaults
            .into_iter()
            .map(|v| VaultRow {
                path: v.path.into(),
                label: v.label.into(),
                state: format!(
                    "{}{}",
                    if v.locked {
                        "Locked"
                    } else {
                        "Unlocked"
                    },
                    if v.is_default {
                        " · default"
                    } else {
                        ""
                    }
                )
                .into(),
                locked: v.locked,
                default_vault: v.is_default,
            })
            .collect::<Vec<_>>(),
    ))));
    ui.set_credentials(ModelRc::from(Rc::new(VecModel::from(
        snapshot
            .items
            .into_iter()
            .map(|i| CredentialRow {
                path: i.path.into(),
                label: i.label.into(),
                application: i
                    .attributes
                    .get("application")
                    .or_else(|| i.attributes.get("xdg:schema"))
                    .cloned()
                    .unwrap_or_else(|| "Application".into())
                    .into(),
                attributes: i
                    .attributes
                    .into_iter()
                    .map(|(k, v)| format!("{k}: {v}"))
                    .collect::<Vec<_>>()
                    .join("\n")
                    .into(),
                modified: i.modified.to_string().into(),
            })
            .collect::<Vec<_>>(),
    ))));
    ui.set_selected_vault(selected_vault);
    ui.set_selected_item(selected_item);
    ui.set_locked(snapshot.locked);
    if snapshot.locked {
        ui.invoke_clear_secret();
    }
    ui.set_status(snapshot.notice.into());
    ui.set_busy(false);
}
fn worker(receiver: mpsc::Receiver<Command>, weak: slint::Weak<VaultWindow>) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    let mut state = State {
        vault: None,
        item: None,
        search: String::new(),
        notice: "Clavis · compatible Secret Service".into(),
    };
    while let Ok(command) = receiver.recv() {
        let result = runtime.block_on(async {
            let client = Client::connect(Interaction::Never).await?;

            let control = zbus::Proxy::new(client.connection(), "org.freedesktop.secrets", "/org/clavis/Control", "org.clavis.Control").await.map_err(clavis_client::Error::from)?;
            let version = control.get_property::<String>("Version").await;
            anyhow::ensure!(version.is_ok(), "Clavis is not selected for the current session. Select it for the next login to RixDE; previous storage is preserved.");
            match command {
                Command::Refresh(query) => state.search = query,
                Command::SelectVault(path) => {
                    state.vault = Some(path);
                    state.item = None;
                }
                Command::SelectItem(path) => state.item = Some(path),
                Command::Lock => {
                    client.lock_all().await?;
                    state.notice = "All vaults locked".into();
                }
                Command::Unlock => {
                    let client = Client::connect(Interaction::Allow).await?;
                    client
                        .unlock(
                            state
                                .vault
                                .as_deref()
                                .ok_or(clavis_client::Error::NoCollection)?,
                        )
                        .await?;
                    state.notice = "Vault unlocked".into();
                }
                Command::Create(label) => {
                    let make_default = !client.collections().await?.iter().any(|c| c.is_default);
                    let interactive = Client::connect(Interaction::Allow).await?;
                    state.vault = Some(interactive.create(&label, make_default).await?);
                    state.item = None;
                    state.notice = "Encrypted vault created".into();
                }
                Command::ChangePassword => {
                    let client = Client::connect(Interaction::Allow).await?;
                    client
                        .change_password(
                            state
                                .vault
                                .as_deref()
                                .ok_or(clavis_client::Error::NoCollection)?,
                        )
                        .await?;
                    state.notice = "Vault password changed".into();
                }
                Command::Save {
                    label,
                    application,
                    key,
                    secret,
                } => {
                    let attrs = credential_attributes(&application, &key)?;
                    client
                        .set_in(state.vault.as_deref(), &label, &attrs, secret)
                        .await?;
                    state.notice = "Secret saved and verified".into();
                }
                Command::Reveal => {
                    let vault = state
                        .vault
                        .as_deref()
                        .ok_or(clavis_client::Error::NoCollection)?;
                    let item = state
                        .item
                        .clone()
                        .ok_or(clavis_client::Error::InvalidInput)?;
                    if let Some(secret) = client.read_item(vault, &item).await? {

                        let text =
                            Zeroizing::new(secret.as_str().map(str::to_owned).unwrap_or_else(
                                || format!("Binary secret · {} bytes", secret.as_bytes().len()),
                            ));
                        let _ = weak.upgrade_in_event_loop(move |ui| {
                            let index = ui.get_selected_item();
                            if !ui.get_locked()
                                && index >= 0
                                && ui
                                    .get_credentials()
                                    .row_data(index as usize)
                                    .is_some_and(|r| r.path.as_str() == item)
                            {
                                ui.set_revealed(slint::SharedString::from(text.as_str()));
                            }
                        });
                    }
                }
                Command::Delete => {
                    client
                        .delete_item(
                            state
                                .vault
                                .as_deref()
                                .ok_or(clavis_client::Error::NoCollection)?,
                            state
                                .item
                                .as_deref()
                                .ok_or(clavis_client::Error::InvalidInput)?,
                        )
                        .await?;
                    state.item = None;
                    state.notice = "Item deleted".into();
                }
                Command::Backup => {
                    client.lock_all().await?;
                    let data = std::env::var_os("XDG_STATE_HOME")
                        .map(PathBuf::from)
                        .filter(|p| p.is_absolute())
                        .or_else(|| {
                            std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state"))
                        })
                        .ok_or(clavis_client::Error::InvalidInput)?;
                    let folder = data.join("clavis/backups");
                    super::backup::prepare_directory(&folder)?;
                    let stamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_nanos();
                    let path = folder.join(format!("clavis-{stamp}.backup"));
                    super::backup::save(&path)?;
                    state.notice = format!("Encrypted backup: {}", path.display());
                }
            }
            state.snapshot(&client).await
        });
        match result {
            Ok(snapshot) => {
                if weak
                    .upgrade_in_event_loop(move |ui| apply_snapshot(&ui, snapshot))
                    .is_err()
                {
                    return;
                }
            }
            Err(error) => {
                let text = error.to_string();
                if weak
                    .upgrade_in_event_loop(move |ui| {
                        ui.set_status(text.into());
                        ui.set_locked(true);
                        ui.set_busy(false);
                        ui.invoke_clear_secret();
                    })
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}
pub fn run_manager() -> Result<()> {
    select_backend()?;
    let ui = VaultWindow::new()?;
    apply_manager_theme(&ui, &load_theme());
    let weak = ui.as_weak();
    theme_stream(move |theme| {
        weak.upgrade_in_event_loop(move |ui| apply_manager_theme(&ui, &theme))
            .is_ok()
    });
    let (send, receive) = mpsc::sync_channel(16);
    let weak = ui.as_weak();
    std::thread::spawn(move || worker(receive, weak));
    let bind =
        |command: Command, weak: &slint::Weak<VaultWindow>, send: &mpsc::SyncSender<Command>| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_busy() {
                return;
            }
            let keep_revealed = matches!(&command, Command::Reveal | Command::Refresh(_));
            if send.try_send(command).is_ok() {
                if !keep_revealed {
                    ui.invoke_clear_secret();
                }
                ui.set_busy(true);
            }
        };
    macro_rules! on_action {
        ($method:ident, $command:expr) => {{
            let send = send.clone();
            let weak = ui.as_weak();
            ui.$method(move || bind($command, &weak, &send));
        }};
    }
    on_action!(on_lock_all, Command::Lock);
    on_action!(on_unlock, Command::Unlock);
    on_action!(on_change_password, Command::ChangePassword);
    on_action!(on_reveal, Command::Reveal);
    on_action!(on_delete_item, Command::Delete);
    on_action!(on_backup, Command::Backup);
    let sender = send.clone();
    let weak = ui.as_weak();
    ui.on_refresh(move || {
        if let Some(ui) = weak.upgrade() {
            bind(
                Command::Refresh(ui.get_search().to_string()),
                &weak,
                &sender,
            );
        }
    });
    let sender = send.clone();
    let weak = ui.as_weak();
    ui.on_select_vault(move |i| {
        if let Some(ui) = weak.upgrade() {
            if let Some(row) = ui.get_vaults().row_data(i.max(0) as usize) {
                bind(Command::SelectVault(row.path.into()), &weak, &sender);
            }
        }
    });
    let sender = send.clone();
    let weak = ui.as_weak();
    ui.on_select_item(move |i| {
        if let Some(ui) = weak.upgrade() {
            if let Some(row) = ui.get_credentials().row_data(i.max(0) as usize) {
                bind(Command::SelectItem(row.path.into()), &weak, &sender);
            }
        }
    });
    let sender = send.clone();
    let weak = ui.as_weak();
    ui.on_create_vault(move |label| bind(Command::Create(label.into()), &weak, &sender));
    let sender = send.clone();
    let weak = ui.as_weak();
    ui.on_save_item(move |label, application, key, secret| {
        bind(
            Command::Save {
                label: label.into(),
                application: application.into(),
                key: key.into(),
                secret: Secret::text(secret.to_string()),
            },
            &weak,
            &sender,
        )
    });
    let timer = slint::Timer::default();
    let weak = ui.as_weak();
    let mut revealed_at = None::<Instant>;
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(200),
        move || {
            if let Some(ui) = weak.upgrade() {
                if ui.get_revealed().is_empty() {
                    revealed_at = None;
                } else if let Some(start) = revealed_at {
                    if start.elapsed() >= Duration::from_secs(20) {
                        ui.set_revealed("".into());
                        revealed_at = None;
                    }
                } else {
                    revealed_at = Some(Instant::now());
                }
            }
        },
    );
    let refresh_timer = slint::Timer::default();
    let weak = ui.as_weak();
    let sender = send.clone();
    refresh_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(3),
        move || {
            if let Some(ui) = weak.upgrade() {
                if !ui.get_busy()
                    && sender
                        .try_send(Command::Refresh(ui.get_search().to_string()))
                        .is_ok()
                {
                    ui.set_busy(true);
                }
            }
        },
    );
    let _ = send.try_send(Command::Refresh(String::new()));
    ui.set_busy(true);
    ui.run()?;
    ui.invoke_clear_secret();
    Ok(())
}
