pub const COMMAND_QUEUE_CAPACITY: usize = 64;
pub const COMMANDS_PER_TICK: usize = 32;

pub const CLIPBOARD_PNG_MAX_BYTES: usize = 16 * 1024 * 1024;
pub const CLIPBOARD_PNG_MAX_PIXELS: u64 = 33_177_600;
pub(crate) const CLIPBOARD_PNG_ENCODED_BYTES: usize = CLIPBOARD_PNG_MAX_BYTES.div_ceil(3) * 4;

pub const CURRENT_PROTOCOL_VERSION: u32 = 2;

pub const LEGACY_PROTOCOL_VERSION: u32 = 1;

pub const IPC_MAX_LINE_BYTES: usize = 32 * 1024 * 1024;

pub const IPC_MAX_REQUEST_BYTES: usize = 1024 * 1024;

pub const IPC_MAX_LARGE_READS: usize = 4;

fn default_protocol_version() -> u32 {
    LEGACY_PROTOCOL_VERSION
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcRequestEnvelope {
    #[serde(default = "default_protocol_version")]
    pub protocol: u32,
    #[serde(flatten)]
    pub request: IpcRequest,
}

impl IpcRequestEnvelope {

    pub fn new(request: IpcRequest) -> Self {
        Self {
            protocol: CURRENT_PROTOCOL_VERSION,
            request,
        }
    }

    pub fn is_compatible(&self) -> bool {
        (LEGACY_PROTOCOL_VERSION..=CURRENT_PROTOCOL_VERSION).contains(&self.protocol)
    }
}

mod png_bytes {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(data: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        if data.len() > super::CLIPBOARD_PNG_MAX_BYTES {
            return Err(serde::ser::Error::custom("PNG exceeds 16 MiB"));
        }
        serializer.serialize_str(&STANDARD.encode(data))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        if encoded.len() > super::CLIPBOARD_PNG_ENCODED_BYTES {
            return Err(serde::de::Error::custom("PNG exceeds 16 MiB"));
        }
        let data = STANDARD.decode(encoded).map_err(serde::de::Error::custom)?;
        if data.len() > super::CLIPBOARD_PNG_MAX_BYTES {
            return Err(serde::de::Error::custom("PNG exceeds 16 MiB"));
        }
        Ok(data)
    }
}

use rix_config::{DesktopSettings, DisplaySettings, LayoutConfig, PanelConfig, ThemeSettings};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: u64,
    pub title: String,
    pub app_id: String,
    #[serde(default)]
    pub outputs: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_outputs: Option<Vec<String>>,
    pub is_active: bool,
    pub is_minimized: bool,
    pub is_maximized: bool,
    pub is_fullscreen: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]

#[allow(clippy::large_enum_variant)]
pub enum IpcRequest {
    ApplyThemePackage {
        id: String,
        include_layout: bool,
    },
    GetPowerState,
    LockSession,
    LogoutSession,
    Ping,
    GetLayout,
    SetLayout(LayoutConfig),
    EditLayout {
        base: LayoutConfig,
        updated: LayoutConfig,
    },
    UpdatePanel(PanelConfig),
    ToggleEditMode,
    SetEditMode(bool),
    GetEditMode,
    GetWindowPreview {
        id: u64,
        width: u32,
        height: u32,
    },
    GetClipboard,
    ClipboardSelect(u64),
    ClipboardDelete(u64),
    ClipboardPin {
        id: u64,
        pinned: bool,
    },
    ClipboardClear,
    ClipboardSetPng {
        #[serde(with = "png_bytes")]
        data: Vec<u8>,
    },
    GetPlugins,
    GetCaptureStatus,
    StopScreenCapture,
    GetSwitcher,
    ChooseSwitcher(Option<u64>),
    GetKeyboard,
    SetKeyboardGroup(u32),
    ListWindows,
    ShowDesktop,
    FocusWindow(u64),
    CloseWindow(u64),
    MinimizeWindow(u64),
    MaximizeWindow(u64),
    Screenshot {
        path: Option<String>,
    },
    ScreenshotOutput {
        output: String,
        path: Option<String>,
    },
    ToggleLauncher,
    SetLauncher(bool),
    PowerAction(String),
    FlipOutput,
    GetTheme,
    SetTheme(ThemeSettings),
    EditTheme {
        base: ThemeSettings,
        updated: ThemeSettings,
    },
    GetDesktop,
    SetDesktop(DesktopSettings),
    EditDesktop {
        base: DesktopSettings,
        updated: DesktopSettings,
    },
    GetOutputs,
    TestOutput {
        output: String,
        profile: rix_config::OutputProfile,
    },
    ConfirmOutput(u64),
    RevertOutput(u64),
    GetDisplay,
    SetDisplay(DisplaySettings),
    SetDisplayMode {
        width: u32,
        height: u32,
        refresh_hz: u32,
    },
    SetIconTheme(String),
    SetCursorTheme {
        name: String,
        size: u32,
    },
    SubscribeEvents,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "data", rename_all = "snake_case")]
pub enum IpcResponse {
    Appearance(rix_config::themes::AppearanceSnapshot),
    Power(PowerState),
    Ok,
    Pong,
    Layout(LayoutConfig),
    Theme(ThemeSettings),
    Desktop(DesktopSettings),
    Display(DisplaySettings),
    Outputs(OutputState),
    OutputConfirmed {
        output: String,
        profile: rix_config::OutputProfile,
    },
    EditMode(bool),
    LauncherState(bool),
    OutputFlipped(bool),
    Windows(Vec<WindowInfo>),
    Clipboard {
        entries: Vec<ClipboardEntry>,
        error: String,
    },
    Plugins(Vec<PluginStatus>),
    CaptureStatus(bool),
    Switcher(SwitcherState),
    Keyboard(KeyboardState),
    WindowPreview {
        id: u64,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    ScreenshotSaved(String),
    Error(String),

    UnsupportedProtocol {

        supported: u32,

        received: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum IpcEvent {
    PowerChanged(PowerState),
    Shell {
        target: rix_config::PanelTarget,
        event: ShellEvent,
    },
    DesktopChanged(DesktopSettings),
    LayoutChanged(LayoutConfig),
    ThemeChanged(ThemeSettings),
    DisplayChanged(DisplaySettings),
    IconThemeChanged(String),
    EditModeChanged(bool),
    LauncherToggleRequested,
    LauncherToggled(bool),
    ShellPopupsDismissed,
    DesktopContextMenu {
        x: f64,
        y: f64,
    },
    WindowOpened(WindowInfo),
    WindowClosed(u64),
    WindowFocused(u64),
    WindowsChanged,
    ShowDesktopChanged(bool),
    SwitcherChanged(SwitcherState),
    KeyboardChanged(KeyboardState),
    CaptureChanged(bool),
    ClipboardChanged,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum ShellEvent {
    LauncherToggle,
    LauncherSet(bool),
    DesktopContextMenu { x: f64, y: f64 },
    SwitcherChanged(SwitcherState),
    PowerAction(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyboardState {
    pub active: u32,
    pub layouts: Vec<rix_config::KeyboardLayout>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitcherState {
    #[serde(default)]
    pub target: Option<rix_config::PanelTarget>,
    pub generation: u64,
    pub selected: u32,
    pub windows: Vec<WindowInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginStatus {
    pub id: String,
    pub running: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardEntry {
    pub id: u64,
    pub label: String,
    pub mime: String,
    pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputState {
    pub native: bool,
    pub outputs: Vec<OutputInfo>,
    pub pending: Option<OutputPreview>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputInfo {
    pub name: String,
    #[serde(default)]
    pub identity: String,
    #[serde(default)]
    pub description: String,
    pub current: rix_config::OutputProfile,
    pub preferred: Option<rix_config::OutputMode>,
    pub modes: Vec<rix_config::OutputMode>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputPreview {
    pub ready: bool,
    pub id: u64,
    pub output: String,
    pub remaining_seconds: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PowerState {
    pub native: bool,
    pub on_battery: Option<bool>,
    pub available_profiles: Vec<String>,
    pub active_profile: String,
    pub performance_degraded: String,
    pub can_suspend: bool,
    pub can_hibernate: bool,
    pub sleep_inhibited: bool,
    pub idle_inhibited: bool,
    pub error: String,
}
