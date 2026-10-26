use crate::protocol::*;
use rix_config::ConfigManager;
use std::os::{
    fd::AsRawFd,
    unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tracing::{error, info, warn};

pub const EVENT_CHANNEL_CAPACITY: usize = 128;

pub const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(5);
pub const WINDOW_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

fn enqueue<T>(sender: &std::sync::mpsc::SyncSender<T>, command: T) -> Result<(), &'static str> {
    sender.try_send(command).map_err(|error| match error {
        std::sync::mpsc::TrySendError::Full(_) => "Compositor busy; retry shortly",
        std::sync::mpsc::TrySendError::Disconnected(_) => "Compositor unavailable",
    })
}

async fn config_write(
    config: Arc<ConfigManager>,
    workers: &Arc<tokio::sync::Semaphore>,
    work: impl FnOnce(&ConfigManager) -> Result<IpcResponse, rix_config::ConfigError> + Send + 'static,
) -> IpcResponse {
    let Ok(permit) = workers.clone().try_acquire_owned() else {
        return IpcResponse::Error("Config writer busy; retry shortly".into());
    };
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work(&config)
    })
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => IpcResponse::Error(error.to_string()),
        Err(error) => IpcResponse::Error(format!("Config worker failed: {error}")),
    }
}

pub fn default_socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("RIX_IPC_SOCKET") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|path| !path.is_empty()) {
        PathBuf::from(runtime_dir).join("rix-ipc.sock")
    } else {
        let uid = unsafe { libc::geteuid() };
        PathBuf::from(format!("/tmp/rix-ipc-{}/rix-ipc.sock", uid))
    }
}

fn prepare_socket_parent(path: &Path, is_default: bool) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let custom_socket = std::env::var_os("RIX_IPC_SOCKET").is_some_and(|path| !path.is_empty());
    if !is_default || custom_socket {
        return Ok(());
    }

    let private_fallback = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|path| !path.is_empty())
        .is_none();
    if private_fallback {
        match std::fs::DirBuilder::new().mode(0o700).create(parent) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent)?;
    let metadata = directory.metadata()?;
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "IPC socket directory belongs to another user",
        ));
    }
    if private_fallback {
        if metadata.mode() & 0o077 != 0
            && unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
    } else if metadata.mode() & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "XDG_RUNTIME_DIR must not be accessible by other users",
        ));
    }
    Ok(())
}

#[derive(Debug)]
pub struct ScreenshotCommand {
    pub output: Option<String>,
    pub path: Option<String>,
    pub responder: tokio::sync::oneshot::Sender<Result<String, String>>,
}

#[derive(Debug, Clone, Copy)]
pub enum WindowAction {
    List,
    ShowDesktop,
    Focus(u64),
    Close(u64),
    Minimize(u64),
    Maximize(u64),
}

#[derive(Debug)]
pub struct WindowCommand {
    pub action: WindowAction,
    pub responder: tokio::sync::oneshot::Sender<Result<Vec<WindowInfo>, String>>,
}

#[derive(Debug)]
pub struct FlipCommand {
    pub responder: tokio::sync::oneshot::Sender<Result<(), String>>,
}

#[derive(Debug)]
pub struct DisplayModeCommand {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub responder: tokio::sync::oneshot::Sender<Result<(), String>>,
}

#[derive(Debug)]
pub enum DesktopAction {
    Output(IpcRequest),
    LockSession,
    LogoutSession,
    Launcher(Option<bool>),
    PowerAction(String),
    Keyboard(Option<u32>),
    Clipboard(IpcRequest),
    GetPlugins,
    GetCaptureStatus,
    StopScreenCapture,
    GetSwitcher,
    ChooseSwitcher(Option<u64>),
    Preview { id: u64, width: u32, height: u32 },
}
#[derive(Debug)]
pub struct DesktopCommand {
    pub action: DesktopAction,
    pub responder: tokio::sync::oneshot::Sender<Result<IpcResponse, String>>,
}

pub struct IpcServer {
    socket_path: PathBuf,
    config: Arc<ConfigManager>,
    edit_mode: Arc<AtomicBool>,
    event_sender: broadcast::Sender<IpcEvent>,
    screenshot_sender:
        Arc<std::sync::Mutex<Option<std::sync::mpsc::SyncSender<ScreenshotCommand>>>>,
    flip_sender: Arc<std::sync::Mutex<Option<std::sync::mpsc::SyncSender<FlipCommand>>>>,
    display_mode_sender:
        Arc<std::sync::Mutex<Option<std::sync::mpsc::SyncSender<DisplayModeCommand>>>>,
    window_sender: Arc<std::sync::Mutex<Option<std::sync::mpsc::SyncSender<WindowCommand>>>>,
    desktop_sender: Arc<std::sync::Mutex<Option<std::sync::mpsc::SyncSender<DesktopCommand>>>>,
    power_state: Arc<std::sync::Mutex<PowerState>>,
    large_reads: Arc<tokio::sync::Semaphore>,
}

struct ConnectionContext {
    config: Arc<ConfigManager>,
    config_workers: Arc<tokio::sync::Semaphore>,
    edit_mode: Arc<AtomicBool>,
    event_tx: broadcast::Sender<IpcEvent>,
    screenshot_sender:
        Arc<std::sync::Mutex<Option<std::sync::mpsc::SyncSender<ScreenshotCommand>>>>,
    flip_sender: Arc<std::sync::Mutex<Option<std::sync::mpsc::SyncSender<FlipCommand>>>>,
    display_mode_sender:
        Arc<std::sync::Mutex<Option<std::sync::mpsc::SyncSender<DisplayModeCommand>>>>,
    window_sender: Arc<std::sync::Mutex<Option<std::sync::mpsc::SyncSender<WindowCommand>>>>,
    desktop_sender: Arc<std::sync::Mutex<Option<std::sync::mpsc::SyncSender<DesktopCommand>>>>,
    power_state: Arc<std::sync::Mutex<PowerState>>,
    large_reads: Arc<tokio::sync::Semaphore>,
}

struct SocketCleanup {
    path: PathBuf,
    inode: u64,
}

impl Drop for SocketCleanup {
    fn drop(&mut self) {

        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.file_type().is_socket() && metadata.ino() == self.inode)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl IpcServer {
    pub fn new(config: Arc<ConfigManager>) -> Self {
        Self::new_with_path(config, default_socket_path())
    }

    pub fn new_with_path(config: Arc<ConfigManager>, socket_path: PathBuf) -> Self {
        let (event_sender, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            socket_path,
            config,
            edit_mode: Arc::new(AtomicBool::new(false)),
            event_sender,
            screenshot_sender: Arc::new(std::sync::Mutex::new(None)),
            flip_sender: Arc::new(std::sync::Mutex::new(None)),
            display_mode_sender: Arc::new(std::sync::Mutex::new(None)),
            window_sender: Arc::new(std::sync::Mutex::new(None)),
            desktop_sender: Arc::new(std::sync::Mutex::new(None)),
            power_state: Arc::new(std::sync::Mutex::new(PowerState::default())),
            large_reads: Arc::new(tokio::sync::Semaphore::new(IPC_MAX_LARGE_READS)),
        }
    }

    pub fn publish_power_state(&self, state: PowerState) {
        let Ok(mut current) = self.power_state.lock() else {
            warn!("Power state lock poisoned; dropping update");
            return;
        };
        if *current != state {
            *current = state.clone();
            let _ = self.event_sender.send(IpcEvent::PowerChanged(state));
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn set_screenshot_sender(&self, sender: std::sync::mpsc::SyncSender<ScreenshotCommand>) {
        if let Ok(mut lock) = self.screenshot_sender.lock() {
            *lock = Some(sender);
        }
    }

    pub fn set_flip_sender(&self, sender: std::sync::mpsc::SyncSender<FlipCommand>) {
        if let Ok(mut lock) = self.flip_sender.lock() {
            *lock = Some(sender);
        }
    }

    pub fn set_display_mode_sender(&self, sender: std::sync::mpsc::SyncSender<DisplayModeCommand>) {
        if let Ok(mut lock) = self.display_mode_sender.lock() {
            *lock = Some(sender);
        }
    }

    pub fn set_desktop_sender(&self, sender: std::sync::mpsc::SyncSender<DesktopCommand>) {
        if let Ok(mut lock) = self.desktop_sender.lock() {
            *lock = Some(sender);
        }
    }

    pub fn set_window_sender(&self, sender: std::sync::mpsc::SyncSender<WindowCommand>) {
        if let Ok(mut lock) = self.window_sender.lock() {
            *lock = Some(sender);
        }
    }

    pub fn event_sender(&self) -> broadcast::Sender<IpcEvent> {
        self.event_sender.clone()
    }

    pub fn is_edit_mode(&self) -> bool {
        self.edit_mode.load(Ordering::SeqCst)
    }

    pub async fn run(&self) -> std::io::Result<()> {
        self.run_with_ready(|| {}).await
    }

    pub async fn run_with_ready(&self, ready: impl FnOnce()) -> std::io::Result<()> {
        prepare_socket_parent(&self.socket_path, self.socket_path == default_socket_path())?;
        if self.socket_path.exists() {
            let metadata = std::fs::symlink_metadata(&self.socket_path)?;
            if metadata.uid() != unsafe { libc::geteuid() } {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "IPC socket belongs to another user",
                ));
            }
            if !metadata.file_type().is_socket() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("IPC path is not a socket: {}", self.socket_path.display()),
                ));
            }
            if std::os::unix::net::UnixStream::connect(&self.socket_path).is_ok() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!(
                        "IPC server already running at {}",
                        self.socket_path.display()
                    ),
                ));
            }
            std::fs::remove_file(&self.socket_path)?;
        }

        let listener = UnixListener::bind(&self.socket_path)?;
        std::fs::set_permissions(&self.socket_path, std::fs::Permissions::from_mode(0o600))?;
        let cleanup = SocketCleanup {
            path: self.socket_path.clone(),
            inode: std::fs::symlink_metadata(&self.socket_path)?.ino(),
        };
        info!("Rix IPC server listening on {:?}", self.socket_path);
        ready();

        let mut config_rx = self.config.subscribe();
        let event_tx_config = self.event_sender.clone();
        tokio::spawn(async move {
            loop {
                match config_rx.recv().await {
                    Ok(layout) => {
                        let _ = event_tx_config.send(IpcEvent::LayoutChanged(layout));
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(
                            "IPC config forwarder lagged, skipped {} layout updates",
                            skipped
                        );
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        let mut desktop_rx = self.config.subscribe_desktop();
        let event_tx_desktop = self.event_sender.clone();
        tokio::spawn(async move {
            loop {
                match desktop_rx.recv().await {
                    Ok(desktop) => {
                        let _ = event_tx_desktop.send(IpcEvent::DesktopChanged(desktop));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        let mut theme_rx = self.config.subscribe_theme();
        let event_tx_theme = self.event_sender.clone();
        tokio::spawn(async move {
            loop {
                match theme_rx.recv().await {
                    Ok(theme) => {
                        let _ = event_tx_theme.send(IpcEvent::ThemeChanged(theme));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        let mut display_rx = self.config.subscribe_display();
        let event_tx_display = self.event_sender.clone();
        tokio::spawn(async move {
            loop {
                match display_rx.recv().await {
                    Ok(display) => {
                        let _ = event_tx_display.send(IpcEvent::DisplayChanged(display));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let _cleanup = cleanup;
        let connections = Arc::new(tokio::sync::Semaphore::new(64));
        let config_workers = Arc::new(tokio::sync::Semaphore::new(8));
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    if peer_uid(stream.peer_cred()) != Some(unsafe { libc::geteuid() }) {
                        continue;
                    }
                    let Ok(permit) = connections.clone().try_acquire_owned() else {
                        continue;
                    };
                    let context = ConnectionContext {
                        config: self.config.clone(),
                        config_workers: config_workers.clone(),
                        edit_mode: self.edit_mode.clone(),
                        event_tx: self.event_sender.clone(),
                        screenshot_sender: self.screenshot_sender.clone(),
                        flip_sender: self.flip_sender.clone(),
                        display_mode_sender: self.display_mode_sender.clone(),
                        window_sender: self.window_sender.clone(),
                        desktop_sender: self.desktop_sender.clone(),
                        power_state: self.power_state.clone(),
                        large_reads: self.large_reads.clone(),
                    };
                    let event_rx = self.event_sender.subscribe();

                    tokio::spawn(async move {
                        let _permit = permit;
                        if let Err(e) = handle_connection(stream, context, event_rx).await {
                            warn!("Connection closed: {}", e);
                        }
                    });
                }
                Err(e) => {
                    error!("Error accepting IPC connection: {}", e);
                }
            }
        }
    }
}

pub(crate) fn peer_uid(cred: std::io::Result<tokio::net::unix::UCred>) -> Option<u32> {
    match cred {
        Ok(cred) => Some(cred.uid()),
        Err(error) => {
            warn!("Could not verify IPC peer credentials: {}", error);
            None
        }
    }
}

async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &IpcResponse,
) -> std::io::Result<()> {
    let mut res_str =
        serde_json::to_string(response).unwrap_or_else(|_| "{\"status\":\"error\"}".to_string());
    res_str.push('\n');
    writer.write_all(res_str.as_bytes()).await
}

async fn handle_connection(
    stream: UnixStream,
    context: ConnectionContext,
    mut _event_rx: broadcast::Receiver<IpcEvent>,
) -> std::io::Result<()> {
    let ConnectionContext {
        config,
        config_workers,
        edit_mode,
        event_tx,
        screenshot_sender,
        flip_sender,
        display_mode_sender,
        window_sender,
        desktop_sender,
        power_state,
        large_reads,
    } = context;
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();

    loop {
        line.clear();

        let read = (&mut reader)
            .take((IPC_MAX_REQUEST_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)
            .await?;
        if read == 0 {
            break;
        }

        let mut _large_slot = None;
        let mut large_line: Option<Vec<u8>> = None;
        if !line.ends_with(b"\n") {
            if read <= IPC_MAX_REQUEST_BYTES {

                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "IPC request line ended before its newline",
                ));
            }
            let Ok(slot) = large_reads.clone().try_acquire_owned() else {
                write_response(
                    &mut writer,
                    &IpcResponse::Error(
                        "IPC server is busy with other large transfers; retry shortly".into(),
                    ),
                )
                .await?;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "large IPC transfer rejected: the buffer budget is exhausted",
                ));
            };
            _large_slot = Some(slot);

            let mut buffer = std::mem::take(&mut line);
            let remaining = (IPC_MAX_LINE_BYTES + 1 - buffer.len()) as u64;
            (&mut reader)
                .take(remaining)
                .read_until(b'\n', &mut buffer)
                .await?;
            if buffer.len() > IPC_MAX_LINE_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "IPC request exceeds the {} MiB line limit",
                        IPC_MAX_LINE_BYTES / (1024 * 1024)
                    ),
                ));
            }
            if !buffer.ends_with(b"\n") {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "IPC request line ended before its newline",
                ));
            }
            large_line = Some(buffer);
        }
        let bytes: &[u8] = large_line.as_deref().unwrap_or(&line);
        if bytes.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let oversized = large_line.is_some();

        let parsed: Result<IpcRequestEnvelope, _> = if oversized {
            let bytes = large_line.take().expect("oversized line buffer");
            tokio::task::spawn_blocking(move || serde_json::from_slice(&bytes))
                .await
                .map_err(std::io::Error::other)?
        } else {
            serde_json::from_slice(bytes)
        };
        let request: Result<IpcRequest, _> = match parsed {
            Ok(envelope) if !envelope.is_compatible() => {
                write_response(
                    &mut writer,
                    &IpcResponse::UnsupportedProtocol {
                        supported: CURRENT_PROTOCOL_VERSION,
                        received: envelope.protocol,
                    },
                )
                .await?;
                continue;
            }
            Ok(envelope) => {
                let request = envelope.request;
                if oversized && !matches!(request, IpcRequest::ClipboardSetPng { .. }) {
                    write_response(
                        &mut writer,
                        &IpcResponse::Error(format!(
                            "IPC request exceeds the {} KiB limit; only clipboard PNG transfers may be larger",
                            IPC_MAX_REQUEST_BYTES / 1024
                        )),
                    )
                    .await?;
                    continue;
                }
                Ok(request)
            }
            Err(error) => Err(error),
        };
        let response = match request {
            Ok(IpcRequest::Ping) => IpcResponse::Pong,
            Ok(IpcRequest::GetLayout) => IpcResponse::Layout(config.get_layout()),
            Ok(IpcRequest::EditLayout { base, updated }) => {
                config_write(config.clone(), &config_workers, move |config| {
                    config.edit_layout(&base, &updated).map(IpcResponse::Layout)
                })
                .await
            }
            Ok(IpcRequest::SetLayout(new_layout)) => {
                config_write(config.clone(), &config_workers, move |config| {
                    config.save_layout(new_layout).map(|()| IpcResponse::Ok)
                })
                .await
            }
            Ok(IpcRequest::UpdatePanel(updated_panel)) => {
                let base = config.get_layout();
                let mut layout = base.clone();
                if let Some(panel) = layout.panels.iter_mut().find(|p| p.id == updated_panel.id) {
                    *panel = updated_panel;
                } else {
                    layout.panels.push(updated_panel);
                }
                config_write(config.clone(), &config_workers, move |config| {
                    config.edit_layout(&base, &layout).map(|_| IpcResponse::Ok)
                })
                .await
            }
            Ok(IpcRequest::ToggleEditMode) => {
                let current = edit_mode.fetch_xor(true, Ordering::SeqCst);
                let new_state = !current;
                let _ = event_tx.send(IpcEvent::EditModeChanged(new_state));
                IpcResponse::EditMode(new_state)
            }
            Ok(IpcRequest::SetEditMode(val)) => {
                edit_mode.store(val, Ordering::SeqCst);
                let _ = event_tx.send(IpcEvent::EditModeChanged(val));
                IpcResponse::EditMode(val)
            }
            Ok(IpcRequest::GetEditMode) => IpcResponse::EditMode(edit_mode.load(Ordering::SeqCst)),
            Ok(IpcRequest::ToggleLauncher) if desktop_sender.lock().is_ok_and(|s| s.is_none()) => {
                let _ = event_tx.send(IpcEvent::LauncherToggleRequested);
                IpcResponse::Ok
            }
            Ok(IpcRequest::SetLauncher(val))
                if desktop_sender.lock().is_ok_and(|s| s.is_none()) =>
            {
                let _ = event_tx.send(IpcEvent::LauncherToggled(val));
                IpcResponse::Ok
            }
            Ok(
                request @ (IpcRequest::ToggleLauncher
                | IpcRequest::SetLauncher(_)
                | IpcRequest::PowerAction(_)
                | IpcRequest::GetOutputs
                | IpcRequest::TestOutput { .. }
                | IpcRequest::ConfirmOutput(_)
                | IpcRequest::RevertOutput(_)
                | IpcRequest::LockSession
                | IpcRequest::LogoutSession
                | IpcRequest::GetKeyboard
                | IpcRequest::SetKeyboardGroup(_)
                | IpcRequest::GetPlugins
                | IpcRequest::GetCaptureStatus
                | IpcRequest::StopScreenCapture
                | IpcRequest::GetClipboard
                | IpcRequest::ClipboardSelect(_)
                | IpcRequest::ClipboardDelete(_)
                | IpcRequest::ClipboardPin { .. }
                | IpcRequest::ClipboardClear
                | IpcRequest::ClipboardSetPng { .. }
                | IpcRequest::GetSwitcher
                | IpcRequest::ChooseSwitcher(_)
                | IpcRequest::GetWindowPreview { .. }),
            ) => {
                let sender = desktop_sender.lock().ok().and_then(|sender| sender.clone());
                if let Some(sender) = sender {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let action = match request {
                        request @ (IpcRequest::GetOutputs
                        | IpcRequest::TestOutput { .. }
                        | IpcRequest::ConfirmOutput(_)
                        | IpcRequest::RevertOutput(_)) => DesktopAction::Output(request),
                        IpcRequest::ToggleLauncher => DesktopAction::Launcher(None),
                        IpcRequest::SetLauncher(open) => DesktopAction::Launcher(Some(open)),
                        IpcRequest::PowerAction(action) => DesktopAction::PowerAction(action),
                        IpcRequest::LockSession => DesktopAction::LockSession,
                        IpcRequest::LogoutSession => DesktopAction::LogoutSession,
                        IpcRequest::GetPlugins => DesktopAction::GetPlugins,
                        IpcRequest::GetCaptureStatus => DesktopAction::GetCaptureStatus,
                        IpcRequest::StopScreenCapture => DesktopAction::StopScreenCapture,
                        request @ (IpcRequest::GetClipboard
                        | IpcRequest::ClipboardSelect(_)
                        | IpcRequest::ClipboardDelete(_)
                        | IpcRequest::ClipboardPin { .. }
                        | IpcRequest::ClipboardClear
                        | IpcRequest::ClipboardSetPng { .. }) => DesktopAction::Clipboard(request),
                        IpcRequest::GetSwitcher => DesktopAction::GetSwitcher,
                        IpcRequest::ChooseSwitcher(id) => DesktopAction::ChooseSwitcher(id),
                        IpcRequest::SetKeyboardGroup(group) => DesktopAction::Keyboard(Some(group)),
                        IpcRequest::GetWindowPreview { id, width, height } => {
                            DesktopAction::Preview { id, width, height }
                        }
                        _ => DesktopAction::Keyboard(None),
                    };
                    if let Err(error) = enqueue(
                        &sender,
                        DesktopCommand {
                            action,
                            responder: tx,
                        },
                    ) {
                        IpcResponse::Error(error.into())
                    } else {
                        match tokio::time::timeout(WINDOW_COMMAND_TIMEOUT, rx).await {
                            Ok(Ok(Ok(IpcResponse::OutputConfirmed { output, profile }))) => {
                                let config = config.clone();
                                match tokio::task::spawn_blocking(move || {
                                    config.save_output_profile(output, profile)
                                })
                                .await
                                {
                                    Ok(Ok(())) => IpcResponse::Ok,
                                    Ok(Err(error)) => IpcResponse::Error(format!(
                                        "Mode confirmed, but could not save it: {error}"
                                    )),
                                    Err(error) => IpcResponse::Error(format!(
                                        "Mode confirmed, but saving failed: {error}"
                                    )),
                                }
                            }
                            Ok(Ok(Ok(response))) => response,
                            Ok(Ok(Err(error))) => IpcResponse::Error(error),
                            _ => IpcResponse::Error("Compositor command timed out".into()),
                        }
                    }
                } else {
                    IpcResponse::Error("Desktop controls unavailable".into())
                }
            }
            Ok(IpcRequest::ListWindows) => {
                dispatch_window_command(&window_sender, WindowAction::List).await
            }
            Ok(IpcRequest::ShowDesktop) => {
                dispatch_window_command(&window_sender, WindowAction::ShowDesktop).await
            }
            Ok(IpcRequest::FocusWindow(id)) => {
                dispatch_window_command(&window_sender, WindowAction::Focus(id)).await
            }
            Ok(IpcRequest::CloseWindow(id)) => {
                dispatch_window_command(&window_sender, WindowAction::Close(id)).await
            }
            Ok(IpcRequest::MinimizeWindow(id)) => {
                dispatch_window_command(&window_sender, WindowAction::Minimize(id)).await
            }
            Ok(IpcRequest::MaximizeWindow(id)) => {
                dispatch_window_command(&window_sender, WindowAction::Maximize(id)).await
            }
            Ok(request @ (IpcRequest::Screenshot { .. } | IpcRequest::ScreenshotOutput { .. })) => {
                let (output, path) = match request {
                    IpcRequest::Screenshot { path } => (None, path),
                    IpcRequest::ScreenshotOutput { output, path } => (Some(output), path),
                    _ => unreachable!(),
                };
                let (tx, rx) = tokio::sync::oneshot::channel();
                let sender_opt = screenshot_sender
                    .lock()
                    .ok()
                    .and_then(|guard| guard.as_ref().cloned());
                if let Some(sender) = sender_opt {
                    match enqueue(
                        &sender,
                        ScreenshotCommand {
                            output,
                            path,
                            responder: tx,
                        },
                    ) {
                        Ok(()) => match tokio::time::timeout(SCREENSHOT_TIMEOUT, rx).await {
                            Ok(Ok(Ok(saved_path))) => IpcResponse::ScreenshotSaved(saved_path),
                            Ok(Ok(Err(err))) => IpcResponse::Error(err),
                            Ok(Err(_)) => {
                                IpcResponse::Error("Compositor dropped screenshot request".into())
                            }
                            Err(_) => {
                                IpcResponse::Error("Screenshot request timed out after 5s".into())
                            }
                        },
                        Err(error) => IpcResponse::Error(error.into()),
                    }
                } else {
                    IpcResponse::Error("Screenshot capability not configured in compositor".into())
                }
            }
            Ok(IpcRequest::FlipOutput) => {
                let sender_opt = flip_sender
                    .lock()
                    .ok()
                    .and_then(|guard| guard.as_ref().cloned());
                if let Some(sender) = sender_opt {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    if let Err(error) = enqueue(&sender, FlipCommand { responder: tx }) {
                        IpcResponse::Error(error.into())
                    } else {
                        match tokio::time::timeout(WINDOW_COMMAND_TIMEOUT, rx).await {
                            Ok(Ok(Ok(()))) => {

                                config_write(config.clone(), &config_workers, |config| {
                                    config
                                        .update_display(|display| {
                                            display.flipped = !display.flipped
                                        })
                                        .map(|()| IpcResponse::Ok)
                                })
                                .await
                            }
                            Ok(Ok(Err(e))) => IpcResponse::Error(e),
                            Ok(Err(_)) => {
                                IpcResponse::Error("Compositor dropped flip request".into())
                            }
                            Err(_) => IpcResponse::Error("Flip request timed out after 5s".into()),
                        }
                    }
                } else {
                    IpcResponse::Error("Compositor flip channel not available".into())
                }
            }
            Ok(IpcRequest::GetPowerState) => {

                IpcResponse::Power(power_state.lock().map(|s| s.clone()).unwrap_or_default())
            }
            Ok(IpcRequest::GetDesktop) => IpcResponse::Desktop(config.get_desktop()),
            Ok(IpcRequest::EditDesktop { base, updated }) => {
                config_write(config.clone(), &config_workers, move |config| {
                    config
                        .edit_desktop(&base, &updated)
                        .map(IpcResponse::Desktop)
                })
                .await
            }
            Ok(IpcRequest::SetDesktop(desktop)) => {
                config_write(config.clone(), &config_workers, move |config| {
                    config.save_desktop(desktop).map(|()| IpcResponse::Ok)
                })
                .await
            }
            Ok(IpcRequest::GetTheme) => IpcResponse::Theme(config.get_theme()),
            Ok(IpcRequest::ApplyThemePackage { id, include_layout }) => {
                config_write(config.clone(), &config_workers, move |config| {
                    config
                        .apply_theme_package(&id, include_layout)
                        .map(IpcResponse::Appearance)
                })
                .await
            }
            Ok(IpcRequest::EditTheme { base, updated }) => {
                config_write(config.clone(), &config_workers, move |config| {
                    config.edit_theme(&base, &updated).map(IpcResponse::Theme)
                })
                .await
            }
            Ok(IpcRequest::SetTheme(theme)) => {
                config_write(config.clone(), &config_workers, move |config| {
                    config.save_theme(theme).map(|()| IpcResponse::Ok)
                })
                .await
            }
            Ok(IpcRequest::GetDisplay) => IpcResponse::Display(config.get_display()),
            Ok(IpcRequest::SetDisplay(disp)) => {
                config_write(config.clone(), &config_workers, move |config| {
                    config.save_display(disp).map(|()| IpcResponse::Ok)
                })
                .await
            }
            Ok(IpcRequest::SetDisplayMode {
                width,
                height,
                refresh_hz,
            }) => {
                if !(640..=7680).contains(&width)
                    || !(480..=4320).contains(&height)
                    || !(30..=360).contains(&refresh_hz)
                {
                    IpcResponse::Error("Unsupported display mode values".into())
                } else if let Some(sender) = display_mode_sender
                    .lock()
                    .ok()
                    .and_then(|guard| guard.as_ref().cloned())
                {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    if let Err(error) = enqueue(
                        &sender,
                        DisplayModeCommand {
                            width,
                            height,
                            refresh_hz,
                            responder: tx,
                        },
                    ) {
                        IpcResponse::Error(error.into())
                    } else {
                        match tokio::time::timeout(WINDOW_COMMAND_TIMEOUT, rx).await {
                            Ok(Ok(Ok(()))) => {

                                config_write(config.clone(), &config_workers, move |config| {
                                    config
                                        .update_display(|display| {
                                            display.width = width;
                                            display.height = height;
                                            display.refresh_hz = refresh_hz;
                                        })
                                        .map(|()| IpcResponse::Ok)
                                })
                                .await
                            }
                            Ok(Ok(Err(e))) => IpcResponse::Error(e),
                            Ok(Err(_)) => {
                                IpcResponse::Error("Compositor dropped display request".into())
                            }
                            Err(_) => {
                                IpcResponse::Error("Display request timed out after 5s".into())
                            }
                        }
                    }
                } else {
                    IpcResponse::Error("Display mode changes unavailable in this backend".into())
                }
            }
            Ok(IpcRequest::SetIconTheme(theme_name)) => {

                let response = {
                    let saved_name = theme_name.clone();
                    config_write(config.clone(), &config_workers, move |config| {
                        let base = config.get_theme();
                        let mut updated = base.clone();
                        updated.icon_theme = saved_name;
                        config.edit_theme(&base, &updated).map(|_| IpcResponse::Ok)
                    })
                    .await
                };
                if matches!(response, IpcResponse::Ok) {
                    let _ = event_tx.send(IpcEvent::IconThemeChanged(theme_name));
                }
                response
            }
            Ok(IpcRequest::SetCursorTheme { name, size }) => {

                config_write(config.clone(), &config_workers, move |config| {
                    let base = config.get_theme();
                    let mut updated = base.clone();
                    updated.cursor_theme = name;
                    updated.cursor_size = size;
                    config.edit_theme(&base, &updated).map(|_| IpcResponse::Ok)
                })
                .await
            }
            Ok(IpcRequest::SubscribeEvents) => {
                let mut res_str = serde_json::to_string(&IpcResponse::Ok).unwrap();
                res_str.push('\n');
                let _ = writer.write_all(res_str.as_bytes()).await;

                loop {
                    match _event_rx.recv().await {
                        Ok(event) => {
                            if let Ok(mut evt_str) = serde_json::to_string(&event) {
                                evt_str.push('\n');
                                if writer.write_all(evt_str.as_bytes()).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!("IPC subscriber lagged, skipped {} events", skipped);
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                return Ok(());
            }
            Err(e) => IpcResponse::Error(format!("Invalid request JSON: {}", e)),
        };

        write_response(&mut writer, &response).await?;
    }

    Ok(())
}

async fn dispatch_window_command(
    sender_slot: &Arc<std::sync::Mutex<Option<std::sync::mpsc::SyncSender<WindowCommand>>>>,
    action: WindowAction,
) -> IpcResponse {
    let sender = sender_slot
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().cloned());
    let Some(sender) = sender else {
        return IpcResponse::Error(
            "Window control not available: compositor window provider not connected".into(),
        );
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    if let Err(error) = enqueue(
        &sender,
        WindowCommand {
            action,
            responder: tx,
        },
    ) {
        return IpcResponse::Error(error.into());
    }
    match tokio::time::timeout(WINDOW_COMMAND_TIMEOUT, rx).await {
        Ok(Ok(Ok(windows))) => match action {
            WindowAction::List => IpcResponse::Windows(windows),
            _ => IpcResponse::Ok,
        },
        Ok(Ok(Err(err))) => IpcResponse::Error(err),
        Ok(Err(_)) => IpcResponse::Error("Compositor dropped window request".into()),
        Err(_) => IpcResponse::Error("Window request timed out after 5s".into()),
    }
}
