use crate::{apply_prompt_theme, PasswordWindow};
use anyhow::Result;
use futures_util::StreamExt;
use slint::ComponentHandle;
use std::{
    io::Write,
    os::{fd::OwnedFd, unix::fs::MetadataExt},
    path::{Component, Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::oneshot;
use zbus::{fdo, message::Header};
use zeroize::Zeroizing;

fn trusted_daemon_paths(development: bool) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = option_env!("CLAVIS_DAEMON_PATH") {
        paths.push(PathBuf::from(path));
    }
    paths.push(PathBuf::from("/usr/libexec/rixde/clavis-daemon"));
    paths.push(PathBuf::from("/usr/local/libexec/rixde/clavis-daemon"));
    if !development {
        return paths;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(directory) = exe.parent() {
            paths.push(directory.join("clavis-daemon"));

            if let (Some(root), Some(profile)) = (exe.ancestors().nth(3), directory.file_name()) {
                paths.push(
                    root.join("vendor/oo7/target")
                        .join(profile)
                        .join("clavis-daemon"),
                );
            }
        }
    }
    paths
}

fn development_paths(default: bool, value: Option<&std::ffi::OsStr>) -> bool {
    default || value.is_some_and(|value| value == "1")
}

fn development_paths_enabled() -> bool {
    development_paths(
        cfg!(debug_assertions),
        std::env::var_os("RIX_CLAVIS_TRUST_DEV_PATHS").as_deref(),
    )
}

fn protected_executable(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let uid = unsafe { libc::geteuid() };
    let mut current = PathBuf::from("/");
    for component in path.components().skip(1) {
        if !matches!(component, Component::Normal(_)) {
            return false;
        }
        current.push(component);
        let Ok(metadata) = current.symlink_metadata() else {
            return false;
        };
        if metadata.file_type().is_symlink()
            || (metadata.uid() != 0 && metadata.uid() != uid)
            || metadata.mode() & 0o022 != 0
            || (current != path && !metadata.is_dir())
        {
            return false;
        }
    }
    path.metadata().is_ok_and(|metadata| {
        metadata.is_file() && metadata.mode() & 0o111 != 0 && metadata.mode() & 0o6000 == 0
    })
}

type Reply = Option<Zeroizing<String>>;
struct Pending {
    id: u64,
    reply: oneshot::Sender<Reply>,
}
#[derive(Default)]
struct State {
    current: Option<Pending>,
    owner: Option<String>,
    epoch: u64,
    next_id: u64,
}
#[derive(Clone)]
struct Broker {
    window: slint::Weak<PasswordWindow>,
    state: Arc<Mutex<State>>,
    displayed: Arc<AtomicU64>,
    gate: Arc<tokio::sync::Mutex<()>>,
    trusted: Arc<Vec<PathBuf>>,
}
impl Broker {
    fn new(ui: &PasswordWindow) -> Self {
        Self::with_trusted(ui, trusted_daemon_paths(development_paths_enabled()))
    }

    fn with_trusted(ui: &PasswordWindow, trusted: Vec<PathBuf>) -> Self {
        Self {
            window: ui.as_weak(),
            state: Default::default(),
            displayed: Default::default(),
            gate: Arc::new(tokio::sync::Mutex::new(())),
            trusted: Arc::new(trusted),
        }
    }

    async fn owner(connection: &zbus::Connection) -> fdo::Result<String> {
        let dbus = fdo::DBusProxy::new(connection).await?;
        Ok(dbus
            .get_name_owner("org.freedesktop.secrets".try_into().unwrap())
            .await?
            .to_string())
    }

    async fn verify_daemon(
        connection: &zbus::Connection,
        sender: zbus::names::UniqueName<'_>,
        trusted: &[PathBuf],
    ) -> fdo::Result<()> {
        let dbus = fdo::DBusProxy::new(connection).await?;
        let denied = |error: fdo::Error| {
            fdo::Error::AccessDenied(format!(
                "Could not verify the Secret Service process: {error}"
            ))
        };
        let pid = dbus
            .get_connection_unix_process_id(sender.clone().into())
            .await
            .map_err(denied)?;
        let uid = dbus
            .get_connection_unix_user(sender.into())
            .await
            .map_err(denied)?;
        if uid != unsafe { libc::geteuid() } {
            return Err(fdo::Error::AccessDenied(
                "The Secret Service must run as the current user".into(),
            ));
        }
        let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) else {
            return Err(fdo::Error::AccessDenied(
                "The Secret Service process identity is unavailable".into(),
            ));
        };
        if !trusted.iter().any(|path| path == &exe) || !protected_executable(&exe) {
            return Err(fdo::Error::AccessDenied(
                "Only the protected Clavis daemon may request a vault password".into(),
            ));
        }
        Ok(())
    }

    async fn authorize(
        header: Header<'_>,
        connection: &zbus::Connection,
        trusted: &[PathBuf],
    ) -> fdo::Result<String> {
        let sender = header
            .sender()
            .ok_or_else(|| fdo::Error::AccessDenied("Missing caller identity".into()))?
            .clone();
        let owner = Self::owner(connection).await?;
        if sender.to_string() != owner {
            return Err(fdo::Error::AccessDenied(
                "Only the Secret Service can request a vault password".into(),
            ));
        }
        Self::verify_daemon(connection, sender, trusted).await?;
        Ok(owner)
    }
    fn hide(&self, id: u64) {
        let displayed = self.displayed.clone();
        let _ = self.window.upgrade_in_event_loop(move |ui| {
            if displayed
                .compare_exchange(id, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                clear_window(&ui);
            }
        });
    }
    fn update_owner(&self, owner: Option<String>) {
        let id = {
            let mut state = self.state.lock().unwrap();
            if state.owner == owner {
                return;
            }
            state.owner = owner;
            state.epoch = state.epoch.wrapping_add(1);
            state.current.take().map(|pending| {
                let _ = pending.reply.send(None);
                pending.id
            })
        };
        if let Some(id) = id {
            self.hide(id);
        }
    }
    fn cancel_pending(&self) {
        let id = {
            let mut state = self.state.lock().unwrap();

            state.epoch = state.epoch.wrapping_add(1);
            state.current.take().map(|pending| {
                let _ = pending.reply.send(None);
                pending.id
            })
        };
        if let Some(id) = id {
            self.hide(id);
        }
    }
    async fn request(
        &self,
        role: &str,
        label: &str,
        description: &str,
        header: Header<'_>,
        connection: &zbus::Connection,
    ) -> fdo::Result<(zbus::zvariant::OwnedFd, bool)> {
        let owner = Self::authorize(header, connection, &self.trusted).await?;
        if !matches!(role, "unlock" | "create" | "change")
            || label.len() > 256
            || description.len() > 8192
            || label.chars().any(char::is_control)
        {
            return Err(fdo::Error::InvalidArgs("Invalid password prompt".into()));
        }
        self.update_owner(Some(owner.clone()));
        let queued_epoch = self.state.lock().unwrap().epoch;
        let _gate = tokio::time::timeout(Duration::from_secs(300), self.gate.lock())
            .await
            .map_err(|_| fdo::Error::Failed("Prompt queue expired".into()))?;

        if Self::owner(connection).await.ok().as_deref() != Some(&owner) {
            return Err(fdo::Error::AccessDenied(
                "The Secret Service owner changed".into(),
            ));
        }
        let (send, receive) = oneshot::channel();
        let id = {
            let mut state = self.state.lock().unwrap();
            if state.epoch != queued_epoch {
                return private_reply(None);
            }
            state.next_id = state.next_id.wrapping_add(1).max(1);
            let id = state.next_id;
            state.current = Some(Pending { id, reply: send });
            id
        };

        let _cleanup = PendingGuard {
            broker: self.clone(),
            id,
        };
        let confirm = role != "unlock";
        let heading = match role {
            "create" => format!("Password for \"{label}\""),
            "change" => format!("New password for \"{label}\""),
            _ => format!("Unlock \"{label}\""),
        };
        let incorrect = description.starts_with("The unlock password was incorrect.");
        let state = self.state.clone();
        let displayed = self.displayed.clone();
        self.window.upgrade_in_event_loop(move |ui| {
            let active = state.lock().unwrap().current.as_ref().is_some_and(|p| p.id == id && !p.reply.is_closed());
            if !active { return; }
            clear_window(&ui);
            displayed.store(id, Ordering::SeqCst);
            ui.set_heading(heading.into());
            ui.set_description(if confirm { "This password protects the vault. Save it: secrets cannot be recovered without it." } else { "The application needs access to your secret vault." }.into());
            ui.set_confirm_required(confirm); ui.set_incorrect(incorrect);
            let _ = ui.show(); ui.invoke_focus_password();
        }).map_err(|_| fdo::Error::Failed("Prompt UI is unavailable".into()))?;
        let response = tokio::time::timeout(Duration::from_secs(300), receive)
            .await
            .ok()
            .and_then(Result::ok)
            .flatten();

        if Self::owner(connection).await.ok().as_deref() != Some(&owner) {
            return private_reply(None);
        }
        private_reply(response)
    }
}
struct PendingGuard {
    broker: Broker,
    id: u64,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        let mut state = self.broker.state.lock().unwrap();
        if state.current.as_ref().is_some_and(|p| p.id == self.id) {
            state.current.take();
        }
        drop(state);
        self.broker.hide(self.id);
    }
}
fn private_reply(response: Reply) -> fdo::Result<(zbus::zvariant::OwnedFd, bool)> {
    let (reader, mut writer) = std::os::unix::net::UnixStream::pair()
        .map_err(|_| fdo::Error::Failed("Could not allocate a private secret channel".into()))?;
    writer
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|_| fdo::Error::Failed("Could not configure secret channel".into()))?;
    let accepted = if let Some(secret) = response {
        if secret.is_empty() || secret.len() > 4096 {
            false
        } else {
            writer
                .write_all(secret.as_bytes())
                .map_err(|_| fdo::Error::Failed("Could not send secret".into()))?;
            true
        }
    } else {
        false
    };
    drop(writer);
    Ok((
        zbus::zvariant::OwnedFd::from(OwnedFd::from(reader)),
        accepted,
    ))
}
fn clear_window(ui: &PasswordWindow) {
    ui.set_password("".into());
    ui.set_confirmation("".into());
    let _ = ui.hide();
}
struct ClavisPrompter(Broker);
#[zbus::interface(name = "org.clavis.Prompter")]
impl ClavisPrompter {
    async fn prompt(
        &self,
        role: &str,
        label: &str,
        description: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<(zbus::zvariant::OwnedFd, bool)> {
        self.0
            .request(role, label, description, header, connection)
            .await
    }
    async fn cancel(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<()> {
        Broker::authorize(header, connection, &self.0.trusted).await?;
        self.0.cancel_pending();
        Ok(())
    }
}
struct LegacyPrompter(Broker);
#[zbus::interface(name = "org.freedesktop.secrets.CliPrompter")]
impl LegacyPrompter {
    async fn prompt(
        &self,
        label: &str,
        description: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<(zbus::zvariant::OwnedFd, bool)> {
        let role = if description.contains("create a new") {
            "create"
        } else if description.contains("change the password") {
            "change"
        } else {
            "unlock"
        };
        self.0
            .request(role, label, description, header, connection)
            .await
    }
}
fn install_ui_callbacks(ui: &PasswordWindow, broker: &Broker) -> slint::Timer {
    let weak = ui.as_weak();
    let state = broker.state.clone();
    let displayed = broker.displayed.clone();
    ui.on_submit(move || {
        let Some(ui) = weak.upgrade() else { return };
        let value = Zeroizing::new(ui.get_password().to_string());
        let confirmation = Zeroizing::new(ui.get_confirmation().to_string());
        if value.is_empty()
            || value.len() > 4096
            || (ui.get_confirm_required() && *value != *confirmation)
        {
            return;
        }
        let id = displayed.load(Ordering::SeqCst);
        let request = {
            let mut state = state.lock().unwrap();
            if state.current.as_ref().is_some_and(|p| p.id == id) {
                state.current.take()
            } else {
                None
            }
        };
        if let Some(request) = request {
            let _ = request.reply.send(Some(value));
        }
        displayed.store(0, Ordering::SeqCst);
        clear_window(&ui);
    });
    let cancel = {
        let broker = broker.clone();
        let weak = ui.as_weak();
        move || {
            broker.cancel_pending();

            broker.displayed.store(0, Ordering::SeqCst);
            if let Some(ui) = weak.upgrade() {
                clear_window(&ui);
            }
        }
    };
    let cancel = Rc::new(cancel);
    let cancelled = cancel.clone();
    ui.on_cancel(move || cancelled());
    ui.window().on_close_requested(move || {
        cancel();
        slint::CloseRequestResponse::KeepWindowShown
    });
    let tick = slint::Timer::default();
    let broker = broker.clone();
    let weak = ui.as_weak();
    tick.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(200),
        move || {
            let closed = broker
                .state
                .lock()
                .unwrap()
                .current
                .as_ref()
                .is_some_and(|p| p.reply.is_closed());
            if closed {
                broker.cancel_pending();
            }
            if broker.displayed.load(Ordering::SeqCst) == 0 {
                if let Some(ui) = weak.upgrade() {
                    ui.set_password("".into());
                    ui.set_confirmation("".into());
                }
            }
        },
    );
    tick
}
fn watch_owner(
    runtime: &tokio::runtime::Runtime,
    broker: Broker,
    connection: zbus::Connection,
) -> tokio::task::JoinHandle<()> {
    runtime.spawn(async move {
        let Ok(dbus) = fdo::DBusProxy::new(&connection).await else {
            broker.cancel_pending();
            return;
        };
        let Ok(mut changes) = dbus.receive_name_owner_changed().await else {
            broker.cancel_pending();
            return;
        };

        broker.update_owner(Broker::owner(&connection).await.ok());
        while let Some(change) = changes.next().await {
            if change
                .args()
                .is_ok_and(|args| args.name().as_str() == "org.freedesktop.secrets")
            {
                broker.update_owner(Broker::owner(&connection).await.ok());
            }
        }
        broker.update_owner(None);
    })
}
pub fn run_service() -> Result<()> {
    super::ui::select_backend()?;
    let ui = PasswordWindow::new()?;
    apply_prompt_theme(&ui, &super::ui::load_theme());
    super::ui::watch_prompt_theme(ui.as_weak());
    let broker = Broker::new(&ui);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let connection = runtime.block_on(async {
        zbus::connection::Builder::session()?
            .name("org.freedesktop.secrets.CliPrompter")?
            .serve_at("/org/clavis/Prompter", ClavisPrompter(broker.clone()))?
            .serve_at(
                "/org/freedesktop/secrets/CliPrompter",
                LegacyPrompter(broker.clone()),
            )?
            .build()
            .await
    })?;
    let watcher = watch_owner(&runtime, broker.clone(), connection.clone());
    let _tick = install_ui_callbacks(&ui, &broker);
    slint::run_event_loop_until_quit()?;
    broker.cancel_pending();
    clear_window(&ui);
    watcher.abort();
    drop(connection);
    drop(runtime);
    Ok(())
}

#[cfg(test)]
#[path = "prompt_tests.rs"]
mod tests;
