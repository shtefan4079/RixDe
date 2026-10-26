pub(crate) use crate::protocol_handlers::get_toplevel_info;
use smithay::utils::{IsAlive, Logical, Point, Rectangle};
use smithay::{
    backend::input::{KeyState, Keycode},
    desktop::{layer_map_for_output, LayerSurface as DesktopLayerSurface, WindowSurfaceType},
    desktop::{PopupGrab, PopupManager, Space, Window},
    input::{
        keyboard::{keysyms, FilterResult},
        pointer::CursorImageStatus,
        Seat, SeatState,
    },
    output::Output,
    reexports::wayland_server::{
        backend::ClientData, protocol::wl_surface::WlSurface, Client, Display, DisplayHandle,
        Resource,
    },
    wayland::{
        compositor::CompositorState,
        dmabuf::DmabufState,
        output::OutputManagerState,
        selection::data_device::DataDeviceState,
        shell::{
            wlr_layer::{Layer, WlrLayerShellState},
            xdg::XdgShellState,
        },
        shm::ShmState,
    },
};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

#[derive(Default)]
pub struct ClientState {
    pub allow_capture: bool,
    pub allow_remote_input: bool,
    pub disconnected: std::sync::atomic::AtomicBool,
    pub compositor_state: smithay::wayland::compositor::CompositorClientState,
}

impl ClientData for ClientState {
    fn disconnected(
        &self,
        client: smithay::reexports::wayland_server::backend::ClientId,
        reason: smithay::reexports::wayland_server::backend::DisconnectReason,
    ) {
        self.disconnected
            .store(true, std::sync::atomic::Ordering::Release);
        tracing::debug!(?client, ?reason, "Wayland client disconnected");
    }
}

fn native_vt_for_key(raw: u32, ctrl: bool, alt: bool) -> Option<i32> {
    if (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12).contains(&raw) {
        Some((raw - keysyms::KEY_XF86Switch_VT_1 + 1) as i32)
    } else if ctrl && alt && (keysyms::KEY_F1..=keysyms::KEY_F12).contains(&raw) {
        Some((raw - keysyms::KEY_F1 + 1) as i32)
    } else {
        None
    }
}

const TITLEBAR_DOUBLE_CLICK_INTERVAL_MS: u32 = 400;
const TITLEBAR_DOUBLE_CLICK_DISTANCE: f64 = 5.0;

#[derive(Default)]
struct TitlebarClickTracker {
    previous: Option<TitlebarClick>,
}

#[derive(Clone, Copy)]
struct TitlebarClick {
    window_id: u64,
    time_msec: u32,
    position: Point<f64, Logical>,
}

impl TitlebarClickTracker {
    fn press(&mut self, window_id: u64, time_msec: u32, position: Point<f64, Logical>) -> bool {
        let previous = self.previous.take();
        if previous.is_some_and(|previous| {
            let delta = time_msec.wrapping_sub(previous.time_msec);
            let movement = position - previous.position;
            previous.window_id == window_id
                && delta <= TITLEBAR_DOUBLE_CLICK_INTERVAL_MS
                && movement.x.abs() <= TITLEBAR_DOUBLE_CLICK_DISTANCE
                && movement.y.abs() <= TITLEBAR_DOUBLE_CLICK_DISTANCE
        }) {
            true
        } else {
            self.previous = Some(TitlebarClick {
                window_id,
                time_msec,
                position,
            });
            false
        }
    }

    fn reset(&mut self) {
        self.previous = None;
    }
}

pub struct RixState {

    pub(crate) greeter_mode: bool,
    pub(crate) idle: crate::idle::Idle,
    pub(crate) lock: crate::session_lock::Lock,
    pub(crate) session_lock_state: smithay::wayland::session_lock::SessionLockManagerState,
    pub selection_start: Option<Point<f64, Logical>>,
    pub selection_color: [f32; 4],
    pub snap_preview: Option<WindowSnapPreview>,
    pub window_snap_settings: rix_config::WindowSnapSettings,
    pub animation_duration: u32,
    pub clipboard: crate::clipboard::Clipboard,
    pub(crate) previews: crate::preview::Queue,
    pub screencopy: crate::screencopy::Screencopy,
    pub window_capture: crate::window_capture::WindowCaptureState,
    pub(crate) remote_input: crate::remote_input::RemoteInputState,
    pub toplevel_observers:
        Vec<rix_protocols::observer_server::rix_toplevel_observer_v1::RixToplevelObserverV1>,
    pub attached_overlays: Vec<WlSurface>,
    pub(crate) screenshot_writer: crate::screenshot::Writer,
    pub(crate) capture_app: crate::capture_app::Launcher,
    pub plugins: crate::plugins::PluginManager,
    pub display_handle: DisplayHandle,
    pub space: Space<Window>,
    pub(crate) windows: crate::window_manager::WindowManager,
    pub popups: PopupManager,
    pub(crate) active_popup_grab: Option<PopupGrab<RixState>>,
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub(crate) xdg_foreign_state: smithay::wayland::xdg_foreign::XdgForeignState,
    pub(crate) _xdg_dialog: Option<smithay::wayland::shell::xdg::dialog::XdgDialogState>,
    pub _xdg_decoration: smithay::wayland::shell::xdg::decoration::XdgDecorationState,
    pub(crate) kde_decoration: smithay::wayland::shell::kde::decoration::KdeDecorationState,
    pub(crate) kde_decorations: HashMap<WlSurface, (
        wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration::OrgKdeKwinServerDecoration,
        wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration::Mode,
    )>,
    pub decoration_theme: crate::decoration::DecorationTheme,
    decoration_settings: rix_config::ThemeSettings,
    window_appearance: rix_config::WindowAppearanceSettings,
    decoration_overrides: HashMap<String, crate::decoration::DecorationTheme>,
    pub decoration_generation: u64,
    pub server_side_decorations: bool,
    pub decoration_preferences: HashMap<
        WlSurface,
        wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
    >,
    pub layer_shell_state: WlrLayerShellState,
    pub shm_state: ShmState,
    pub dmabuf_state: DmabufState,
    pub(crate) dmabuf_imports: crate::dmabuf::PendingImports,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub(crate) primary_selection:
        smithay::wayland::selection::primary_selection::PrimarySelectionState,
    pub(crate) data_control:
        Option<smithay::wayland::selection::ext_data_control::DataControlState>,
    pub _relative_pointer_manager: smithay::wayland::relative_pointer::RelativePointerManagerState,
    pub _pointer_constraints: smithay::wayland::pointer_constraints::PointerConstraintsState,

    pointer_constraint_focus_dirty: bool,
    pointer_constraint_keyboard_client: Option<Option<smithay::reexports::wayland_server::backend::ClientId>>,
    pub output_manager_state: OutputManagerState,
    pub seat: Seat<Self>,
    pub running: Arc<AtomicBool>,
    pub socket_name: String,
    pub(crate) shell_targets: Vec<rix_config::PanelTarget>,
    pub(crate) launcher_panels: HashSet<String>,
    pub clients: Vec<Client>,
    pub start_time: Instant,
    pub screenshot_sender: std::sync::mpsc::SyncSender<rix_ipc::ScreenshotCommand>,
    pub screenshot_rx: std::sync::mpsc::Receiver<rix_ipc::ScreenshotCommand>,
    pub(crate) screenshot_pending: Vec<rix_ipc::ScreenshotCommand>,
    pub flip_sender: std::sync::mpsc::SyncSender<rix_ipc::FlipCommand>,
    pub flip_rx: std::sync::mpsc::Receiver<rix_ipc::FlipCommand>,
    pub display_mode_sender: std::sync::mpsc::SyncSender<rix_ipc::DisplayModeCommand>,
    pub display_mode_rx: std::sync::mpsc::Receiver<rix_ipc::DisplayModeCommand>,
    pub(crate) output_commands: std::collections::VecDeque<rix_ipc::DesktopCommand>,
    pub pending_display_mode: Option<(rix_ipc::DisplayModeCommand, Instant)>,
    pub window_sender: std::sync::mpsc::SyncSender<rix_ipc::WindowCommand>,
    pub window_rx: std::sync::mpsc::Receiver<rix_ipc::WindowCommand>,
    pub ipc_event_sender: Option<tokio::sync::broadcast::Sender<rix_ipc::IpcEvent>>,
    pub switcher: crate::switcher::Switcher,
    pub switcher_settings: rix_config::SwitcherSettings,
    pub native_session: bool,
    pub vt_switch: Option<i32>,
    pub(crate) pressed_buttons: HashSet<u32>,
    pub(crate) decoration_button_press: Option<(u64, crate::decoration::DecorationHit)>,
    titlebar_clicks: TitlebarClickTracker,
    pub layer_surfaces: Vec<DesktopLayerSurface>,

    pub(crate) layer_outputs: HashMap<WlSurface, String>,
    pub(crate) pending_layers: Vec<DesktopLayerSurface>,

    pub(crate) space_dirty: bool,
    pub cursor_status: CursorImageStatus,
    pub(crate) drag: crate::drag_drop::DragState,
    pub resize_cursor: Option<smithay::input::pointer::CursorIcon>,
    pub _cursor_shape: smithay::wayland::cursor_shape::CursorShapeManagerState,
    intercepted_keys: HashSet<u32>,
    pub(crate) logo_tap: Option<u32>,
    pub desktop_sender: std::sync::mpsc::SyncSender<rix_ipc::DesktopCommand>,
    pub desktop_rx: std::sync::mpsc::Receiver<rix_ipc::DesktopCommand>,
    pub(crate) lock_sender: std::sync::mpsc::SyncSender<rix_ipc::DesktopCommand>,
    pub(crate) lock_rx: std::sync::mpsc::Receiver<rix_ipc::DesktopCommand>,
    pub(crate) audio_hotkeys: tokio::sync::mpsc::Sender<rix_services::audio::Hotkey>,
    pub(crate) audio_hotkey_rx: Option<tokio::sync::mpsc::Receiver<rix_services::audio::Hotkey>>,
    pub keyboard_config: rix_config::KeyboardConfig,
    pub keyboard_group: u32,
}

pub struct WindowSnapPreview {
    pub output_name: String,
    pub geometry: Rectangle<i32, Logical>,
    pub visible: bool,
    pub transition_started: Instant,
}

impl RixState {
    pub(crate) fn clear_intercepted_keys(&mut self) {
        self.intercepted_keys.clear();
    }

    pub(crate) fn tick(&mut self) {
        self.prune_disconnected_clients();
        self.prune_popup_grab();
        self.prune_expired_placements(std::time::Duration::from_secs(10));
        if std::mem::take(&mut self.space_dirty) {
            self.space.refresh();
        }
    }

    pub(crate) fn prune_disconnected_clients(&mut self) {
        use std::sync::atomic::Ordering;
        self.clients.retain(|client| {
            client
                .get_data::<ClientState>()
                .is_none_or(|state| !state.disconnected.load(Ordering::Acquire))
        });
    }
    pub fn new(display: &mut Display<Self>, socket_name: String) -> Self {
        Self::new_with_mode(display, socket_name, false)
    }

    pub(crate) fn new_with_mode(
        display: &mut Display<Self>,
        socket_name: String,
        greeter_mode: bool,
    ) -> Self {
        let display_handle = display.handle();
        crate::blur::install(&display_handle);
        crate::surface_effects::install(&display_handle);
        if !greeter_mode {
            crate::capture_policy::install(&display_handle);
            crate::toplevel_observer::install(&display_handle);
            crate::screencopy::install(&display_handle);
            crate::window_capture::install(&display_handle);
            crate::remote_input::install(&display_handle);
        }
        smithay::wayland::viewporter::ViewporterState::new::<Self>(&display_handle);
        smithay::wayland::fractional_scale::FractionalScaleManagerState::new::<Self>(
            &display_handle,
        );
        let window_capture = if greeter_mode {
            Default::default()
        } else {
            crate::window_capture::WindowCaptureState::new::<Self>(&display_handle)
        };
        if !greeter_mode {
            smithay::wayland::idle_inhibit::IdleInhibitManagerState::new::<Self>(&display_handle);
        }

        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let xdg_foreign_state =
            smithay::wayland::xdg_foreign::XdgForeignState::new::<Self>(&display_handle);
        if greeter_mode {

            display_handle.remove_global::<Self>(xdg_foreign_state.exporter_global());
            display_handle.remove_global::<Self>(xdg_foreign_state.importer_global());
        }
        let xdg_dialog = (!greeter_mode).then(|| {
            smithay::wayland::shell::xdg::dialog::XdgDialogState::new::<Self>(&display_handle)
        });
        let layer_shell_state = WlrLayerShellState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(
            &display_handle,
            vec![
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Argb8888,
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Xrgb8888,
            ],
        );
        let dmabuf_state = DmabufState::new();
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        let mut seat_state = SeatState::new();
        let seat = seat_state.new_wl_seat(&display_handle, "rix-seat-0");
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);
        if greeter_mode {

            display_handle.remove_global::<Self>(data_device_state.global());
        }
        let relative_pointer_manager =
            smithay::wayland::relative_pointer::RelativePointerManagerState::new::<Self>(
                &display_handle,
            );
        let pointer_constraints =
            smithay::wayland::pointer_constraints::PointerConstraintsState::new::<Self>(
                &display_handle,
            );
        let primary_selection =
            smithay::wayland::selection::primary_selection::PrimarySelectionState::new_with_filter::<
                Self,
                _,
            >(&display_handle, move |_| !greeter_mode);

        let data_control = (!greeter_mode).then(|| {
            smithay::wayland::selection::ext_data_control::DataControlState::new::<Self, _>(
                &display_handle,
                Some(&primary_selection),
                |_| true,
            )
        });
        let (screenshot_sender, screenshot_rx) =
            std::sync::mpsc::sync_channel(rix_ipc::COMMAND_QUEUE_CAPACITY);
        let (flip_sender, flip_rx) = std::sync::mpsc::sync_channel(rix_ipc::COMMAND_QUEUE_CAPACITY);
        let (display_mode_sender, display_mode_rx) =
            std::sync::mpsc::sync_channel(rix_ipc::COMMAND_QUEUE_CAPACITY);
        let (window_sender, window_rx) =
            std::sync::mpsc::sync_channel(rix_ipc::COMMAND_QUEUE_CAPACITY);
        let (desktop_sender, desktop_rx) =
            std::sync::mpsc::sync_channel(rix_ipc::COMMAND_QUEUE_CAPACITY);
        let (lock_sender, lock_rx) = std::sync::mpsc::sync_channel(8);
        let (audio_hotkeys, audio_hotkey_rx) = tokio::sync::mpsc::channel(64);

        let mut state = Self {
            greeter_mode,
            idle: Default::default(),
            lock: Default::default(),
            session_lock_state: smithay::wayland::session_lock::SessionLockManagerState::new::<
                Self,
                _,
            >(&display.handle(), move |_| !greeter_mode),
            selection_start: None,
            selection_color: [0.; 4],
            snap_preview: None,
            window_snap_settings: rix_config::WindowSnapSettings::default(),
            animation_duration: 0,
            clipboard: Default::default(),
            previews: Default::default(),
            plugins: Default::default(),
            screencopy: Default::default(),
            window_capture,
            remote_input: Default::default(),
            toplevel_observers: Vec::new(),
            attached_overlays: Vec::new(),
            screenshot_writer: Default::default(),
            capture_app: Default::default(),
            display_handle,
            space: Space::default(),
            windows: Default::default(),
            popups: PopupManager::default(),
            active_popup_grab: None,
            compositor_state,
            xdg_shell_state,
            xdg_foreign_state,
            _xdg_dialog: xdg_dialog,
            _xdg_decoration: smithay::wayland::shell::xdg::decoration::XdgDecorationState::new::<
                Self,
            >(&display.handle()),
            kde_decoration: smithay::wayland::shell::kde::decoration::KdeDecorationState::new::<Self>(
                &display.handle(),
                wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::Mode::Server,
            ),
            kde_decorations: HashMap::new(),
            decoration_theme: crate::decoration::DecorationTheme::load_with_material(
                &rix_config::ThemeSettings::default(), true,
            ),
            decoration_settings: Default::default(),
            window_appearance: Default::default(),
            decoration_overrides: Default::default(),
            decoration_generation: 0,
            server_side_decorations: true,
            decoration_preferences: HashMap::new(),
            layer_shell_state,
            shm_state,
            dmabuf_state,
            dmabuf_imports: Default::default(),
            seat_state,
            data_device_state,
            primary_selection,
            data_control,
            _relative_pointer_manager: relative_pointer_manager,
            _pointer_constraints: pointer_constraints,
            pointer_constraint_focus_dirty: false,
            pointer_constraint_keyboard_client: None,
            output_manager_state,
            seat,
            running: Arc::new(AtomicBool::new(true)),
            socket_name,
            shell_targets: Vec::new(),
            launcher_panels: HashSet::new(),
            clients: Vec::new(),
            start_time: Instant::now(),
            screenshot_sender,
            screenshot_rx,
            screenshot_pending: Vec::new(),
            flip_sender,
            flip_rx,
            display_mode_sender,
            display_mode_rx,
            output_commands: Default::default(),
            pending_display_mode: None,
            window_sender,
            window_rx,
            ipc_event_sender: None,
            switcher: Default::default(),
            switcher_settings: Default::default(),
            native_session: false,
            vt_switch: None,
            pressed_buttons: HashSet::new(),
            decoration_button_press: None,
            titlebar_clicks: TitlebarClickTracker::default(),
            layer_surfaces: Vec::new(),
            layer_outputs: HashMap::new(),
            pending_layers: Vec::new(),
            space_dirty: false,
            cursor_status: CursorImageStatus::default_named(),
            drag: Default::default(),
            resize_cursor: None,
            _cursor_shape: smithay::wayland::cursor_shape::CursorShapeManagerState::new::<Self>(
                &display.handle(),
            ),
            intercepted_keys: HashSet::new(),
            logo_tap: None,
            desktop_sender,
            desktop_rx,
            lock_sender,
            lock_rx,
            audio_hotkeys,
            audio_hotkey_rx: Some(audio_hotkey_rx),
            keyboard_config: Default::default(),
            keyboard_group: 0,
        };

        state.restore_supervisor_lock(
            !greeter_mode && std::env::var("RIX_RECOVERY_LOCKED").as_deref() == Ok("1"),
        );
        state
    }

    pub fn set_decoration_theme(&mut self, theme: &rix_config::ThemeSettings) {
        self.decoration_settings = theme.clone();
        self.reload_decoration_themes();
    }

    pub fn set_window_appearance(&mut self, settings: rix_config::WindowAppearanceSettings) {
        if self.window_appearance == settings {
            return;
        }
        self.window_appearance = settings;
        self.reload_decoration_themes();
    }

    fn reload_decoration_themes(&mut self) {
        self.decoration_theme = crate::decoration::DecorationTheme::load_with_material(
            &self.decoration_settings,
            self.window_appearance.use_desktop_material,
        );
        self.decoration_overrides = self
            .window_appearance
            .rules
            .iter()
            .map(|rule| {
                let (theme, material) = self
                    .window_appearance
                    .effective(&rule.app_id, &self.decoration_settings);
                (
                    rule.app_id.clone(),
                    crate::decoration::DecorationTheme::load_with_material(&theme, material),
                )
            })
            .collect();
        self.decoration_generation = self.decoration_generation.wrapping_add(1);
        self.reflow_output_layout();
    }

    pub fn decoration_for_window(&self, window: &Window) -> crate::decoration::DecorationTheme {
        let app_id = window
            .toplevel()
            .and_then(|surface| get_toplevel_info(surface).1);
        let theme = self.decoration_for_app(app_id.as_deref().unwrap_or_default());
        if let Some((id, _)) = self
            .windows
            .entries
            .iter()
            .find(|(_, known)| known == window)
        {

            theme.with_maximized(
                self.windows
                    .pending_placement
                    .get(id)
                    .map_or(self.windows.maximized.contains_key(id), |pending| {
                        pending.previous_maximized
                    }),
            )
        } else {
            theme.for_window(window)
        }
    }

    pub(crate) fn decoration_for_window_layout(
        &self,
        window: &Window,
    ) -> crate::decoration::DecorationTheme {
        let app_id = window
            .toplevel()
            .and_then(|surface| get_toplevel_info(surface).1);
        let theme = self.decoration_for_app(app_id.as_deref().unwrap_or_default());
        if let Some((id, _)) = self
            .windows
            .entries
            .iter()
            .find(|(_, known)| known == window)
        {
            theme.with_maximized(self.windows.maximized.contains_key(id))
        } else {
            theme.for_window(window)
        }
    }

    fn decoration_for_app(&self, app_id: &str) -> &crate::decoration::DecorationTheme {
        self.decoration_overrides
            .get(app_id)
            .unwrap_or(&self.decoration_theme)
    }

    pub fn set_server_side_decorations(&mut self, enabled: bool) {
        if self.server_side_decorations == enabled {
            return;
        }
        self.server_side_decorations = enabled;
        for (id, window) in &self.windows.entries {
            if let Some(toplevel) = window.toplevel() {

                if self.kde_decorations.contains_key(toplevel.wl_surface()) {
                    continue;
                }
                if toplevel.with_pending_state(|state| state.decoration_mode.is_none()) {
                    continue;
                }
                let mode = crate::decoration::effective_mode(
                    enabled,
                    self.decoration_preferences
                        .get(toplevel.wl_surface())
                        .copied(),
                );
                toplevel.with_pending_state(|state| state.decoration_mode = Some(mode));
                if toplevel.is_initial_configure_sent() {
                    if let Some(serial) = toplevel.send_pending_configure() {
                        tracing::debug!(
                            window = id,
                            ?mode,
                            ?serial,
                            "Updated window decoration mode"
                        );
                    }
                }
            }
        }
    }

    pub fn handle_keyboard_key(
        &mut self,
        keycode: Keycode,
        key_state: KeyState,
        serial: smithay::utils::Serial,
        time: u32,
        screenshot_supported: bool,
    ) {
        if !self.remote_input.injecting
            && !self
                .remote_input
                .physical_key(u32::from(keycode), key_state)
        {
            return;
        }
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let mut finish_switcher = None;
        let mut cancel_drag = false;
        keyboard.input::<(), _>(
            self,
            keycode,
            key_state,
            serial,
            time,
            |state, mods, key| {
                let code = u32::from(keycode);
                let symbol = key.modified_sym();
                if state.greeter_mode {
                    if key_state == KeyState::Released && state.intercepted_keys.remove(&code) {
                        return FilterResult::Intercept(());
                    }

                    if state.native_session
                        && key_state == KeyState::Pressed
                        && mods.ctrl
                        && mods.alt
                    {
                        if let Some(vt) = native_vt_for_key(u32::from(symbol), mods.ctrl, mods.alt)
                        {
                            state.intercepted_keys.insert(code);
                            state.vt_switch = Some(vt);
                            return FilterResult::Intercept(());
                        }
                    }
                    return FilterResult::Forward;
                }
                let logo_key = matches!(
                    u32::from(symbol),
                    keysyms::KEY_Super_L
                        | keysyms::KEY_Super_R
                        | keysyms::KEY_Meta_L
                        | keysyms::KEY_Meta_R
                );
                if state.switcher.active() && key_state == KeyState::Released && !mods.alt {
                    finish_switcher = Some(true);
                }

                if key_state == KeyState::Released && logo_key {
                    let tapped = state.logo_tap.take() == Some(code);
                    if tapped && !state.lock.active {
                        state.toggle_launcher();
                    }
                }

                if key_state == KeyState::Released && state.intercepted_keys.remove(&code) {
                    return FilterResult::Intercept(());
                }
                if key_state != KeyState::Pressed {
                    return FilterResult::Forward;
                }

                if logo_key && !mods.alt && !mods.ctrl && !mods.shift && !state.lock.active {
                    state.logo_tap = state.logo_tap.is_none().then_some(code);
                } else {
                    state.logo_tap = None;
                }
                if state.native_session {
                    let vt = native_vt_for_key(u32::from(symbol), mods.ctrl, mods.alt);
                    if let Some(vt) = vt {
                        state.intercepted_keys.insert(code);
                        state.vt_switch = Some(vt);
                        return FilterResult::Intercept(());
                    }
                }
                let audio = match u32::from(symbol) {
                    keysyms::KEY_XF86AudioRaiseVolume => {
                        Some(rix_services::audio::Hotkey::VolumeUp)
                    }
                    keysyms::KEY_XF86AudioLowerVolume => {
                        Some(rix_services::audio::Hotkey::VolumeDown)
                    }
                    keysyms::KEY_XF86AudioMute => Some(rix_services::audio::Hotkey::ToggleMute),
                    keysyms::KEY_XF86AudioMicMute => {
                        Some(rix_services::audio::Hotkey::ToggleMicrophoneMute)
                    }
                    _ => None,
                };
                if let Some(audio) = audio {
                    if state.intercepted_keys.insert(code) {
                        if let Err(error) = state.audio_hotkeys.try_send(audio) {
                            tracing::warn!(%error, "Hardware audio control queue is unavailable");
                        }
                    }
                    return FilterResult::Intercept(());
                }
                if state.lock.active {
                    return FilterResult::Forward;
                }
                if state.drag.active && symbol == keysyms::KEY_Escape.into() {
                    state.intercepted_keys.insert(code);
                    cancel_drag = true;
                    return FilterResult::Intercept(());
                }
                if symbol == keysyms::KEY_Escape.into() && state.selection_start.take().is_some() {
                    state.intercepted_keys.insert(code);
                    return FilterResult::Intercept(());
                }
                let shortcut = if state.switcher.active() && symbol == keysyms::KEY_Escape.into() {
                    state.intercepted_keys.insert(code);
                    finish_switcher = Some(false);
                    true
                } else if state.switcher.active() && symbol == keysyms::KEY_Return.into() {
                    state.intercepted_keys.insert(code);
                    finish_switcher = Some(true);
                    true
                } else if state.switcher.active()
                    && (symbol == keysyms::KEY_Left.into() || symbol == keysyms::KEY_Right.into())
                {
                    if state.intercepted_keys.insert(code) {
                        state.step_switcher(symbol == keysyms::KEY_Left.into());
                    }
                    true
                } else if mods.alt
                    && (symbol == keysyms::KEY_Tab.into()
                        || symbol == keysyms::KEY_ISO_Left_Tab.into())
                {
                    if state.intercepted_keys.insert(code) {
                        state.step_switcher(mods.shift);
                    }
                    true
                } else if mods.alt && symbol == keysyms::KEY_F4.into() {
                    if state.intercepted_keys.insert(code) {
                        if let Some(id) = state.windows.active {
                            let _ = state.apply_window_action(rix_ipc::WindowAction::Close(id));
                        }
                    }
                    true
                } else if mods.ctrl
                    && mods.alt
                    && (symbol == keysyms::KEY_Delete.into()
                        || symbol == keysyms::KEY_KP_Delete.into())
                {
                    if state.intercepted_keys.insert(code) {
                        state.publish_shell(
                            state.shell_output().as_deref(),
                            rix_ipc::ShellEvent::PowerAction("security".into()),
                        );
                    }
                    true
                } else if mods.logo && symbol == keysyms::KEY_l.into() {
                    if state.intercepted_keys.insert(code) {
                        state.lock.requested = true;
                    }
                    true
                } else if mods.logo
                    && symbol == keysyms::KEY_space.into()
                    && state.keyboard_config.switch_option != "grp:win_space_toggle"
                {
                    if state.intercepted_keys.insert(code) {
                        state.toggle_launcher();
                    }
                    true
                } else if screenshot_supported && symbol == keysyms::KEY_Print.into() {
                    if state.intercepted_keys.insert(code) {
                        state
                            .capture_app
                            .request(crate::capture_app::Mode::Panel, &state.socket_name);
                    }
                    true
                } else if screenshot_supported
                    && mods.logo
                    && mods.shift
                    && !mods.ctrl
                    && !mods.alt
                    && key.raw_latin_sym_or_raw_current_sym() == Some(keysyms::KEY_s.into())
                {
                    if state.intercepted_keys.insert(code) {
                        state
                            .capture_app
                            .request(crate::capture_app::Mode::Area, &state.socket_name);
                    }
                    true
                } else if state.switcher.active() {
                    state.intercepted_keys.insert(code);
                    true
                } else {
                    false
                };
                if shortcut {
                    FilterResult::Intercept(())
                } else {
                    FilterResult::Forward
                }
            },
        );
        if cancel_drag {
            self.cancel_drag();
        } else {
            self.refresh_drag_action();
        }
        if let Some(commit) = finish_switcher {
            self.finish_switcher(commit);
        }
        self.publish_keyboard_state();
    }

    pub(crate) fn emit_window_change(&self) {
        if let Some(sender) = &self.ipc_event_sender {
            let _ = sender.send(rix_ipc::IpcEvent::WindowsChanged);
        }
    }

    pub(crate) fn release_remote_key(&mut self, code: Keycode, time: u32) {
        let intercepted = self.intercepted_keys.remove(&u32::from(code));
        if self.logo_tap == Some(u32::from(code)) {
            self.logo_tap = None;
        }
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.input::<(), _>(
                self,
                code,
                KeyState::Released,
                smithay::utils::SERIAL_COUNTER.next_serial(),
                time,
                |_, _, _| {
                    if intercepted {
                        FilterResult::Intercept(())
                    } else {
                        FilterResult::Forward
                    }
                },
            );
            if self.switcher.active() && !keyboard.modifier_state().alt {
                self.finish_switcher(false);
            }
        }
        self.refresh_drag_action();
        self.publish_keyboard_state();
    }

    pub fn desktop_bounds(&self) -> Option<(f64, f64, f64, f64)> {
        self.space
            .outputs()
            .filter_map(|output| self.space.output_geometry(output))
            .fold(None::<(f64, f64, f64, f64)>, |bounds, geometry| {
                let left = geometry.loc.x as f64;
                let top = geometry.loc.y as f64;
                let right = (geometry.loc.x + geometry.size.w) as f64;
                let bottom = (geometry.loc.y + geometry.size.h) as f64;
                Some(match bounds {
                    Some((min_x, min_y, max_x, max_y)) => (
                        min_x.min(left),
                        min_y.min(top),
                        max_x.max(right),
                        max_y.max(bottom),
                    ),
                    None => (left, top, right, bottom),
                })
            })
            .filter(|(min_x, min_y, max_x, max_y)| max_x > min_x && max_y > min_y)
    }

    pub fn effective_cursor_status(&self) -> CursorImageStatus {
        if self.pointer_is_locked() {
            return CursorImageStatus::Hidden;
        }
        if let Some(icon) = self.resize_cursor {
            return CursorImageStatus::Named(icon);
        }
        let Some(pointer) = self.seat.get_pointer() else {
            return self.cursor_status.clone();
        };
        let pos = pointer.current_location();
        if self.surface_under(pos).is_some() {
            return self.cursor_status.clone();
        }
        for window in self.space.elements().rev() {
            let Some(body) = self.space.element_geometry(window) else {
                continue;
            };
            if body.to_f64().contains(pos) {
                break;
            }
            if crate::decoration::enabled(window) {
                if let Some(hit) = self.decoration_for_window(window).hit(body, pos) {
                    return CursorImageStatus::Named(match hit {
                        crate::decoration::DecorationHit::Resize(edge) => {
                            crate::grabs::resize_cursor(edge)
                        }
                        _ => smithay::input::pointer::CursorIcon::Default,
                    });
                }
            }
        }
        CursorImageStatus::default_named()
    }

    pub fn pointer_is_locked(&self) -> bool {
        let Some(pointer) = self.seat.get_pointer() else {
            return false;
        };
        let Some(surface) = pointer.current_focus() else {
            return false;
        };
        smithay::wayland::pointer_constraints::with_pointer_constraint(
            &surface,
            &pointer,
            |constraint| {
                constraint.is_some_and(|constraint| {
                    constraint.is_active()
                        && matches!(
                            &*constraint,
                            smithay::wayland::pointer_constraints::PointerConstraint::Locked(_)
                        )
                })
            },
        )
    }

    fn pointer_has_active_constraint(&self) -> bool {
        let Some(pointer) = self.seat.get_pointer() else {
            return false;
        };
        let Some(surface) = pointer.current_focus() else {
            return false;
        };
        smithay::wayland::pointer_constraints::with_pointer_constraint(
            &surface,
            &pointer,
            |constraint| constraint.is_some_and(|constraint| constraint.is_active()),
        )
    }

    pub fn constrain_pointer_location(
        &self,
        candidate: Point<f64, Logical>,
    ) -> Point<f64, Logical> {
        let Some(pointer) = self.seat.get_pointer() else {
            return candidate;
        };
        let Some(surface) = pointer.current_focus() else {
            return candidate;
        };
        let current = pointer.current_location();
        let Some((focused_surface, origin)) = self.surface_under(current) else {
            return candidate;
        };
        if focused_surface != surface {
            return candidate;
        }

        let constraint = smithay::wayland::pointer_constraints::with_pointer_constraint(
            &surface,
            &pointer,
            |constraint| {
                constraint.and_then(|constraint| {
                    if !constraint.is_active()
                        || !matches!(
                            &*constraint,
                            smithay::wayland::pointer_constraints::PointerConstraint::Confined(_)
                        )
                    {
                        return None;
                    }
                    Some(constraint.region().cloned())
                })
            },
        );
        let Some(region) = constraint else {
            return candidate;
        };

        let local_candidate = candidate - origin;
        if let Some(region) = region {
            if region.contains(local_candidate.to_i32_round()) {
                return candidate;
            }

            let local_current = current - origin;
            let delta = local_candidate - local_current;
            let mut inside = 0.0;
            let mut outside = 1.0;
            for _ in 0..16 {
                let middle = (inside + outside) / 2.0;
                let point = Point::from((
                    local_current.x + delta.x * middle,
                    local_current.y + delta.y * middle,
                ));
                if region.contains(point.to_i32_round()) {
                    inside = middle;
                } else {
                    outside = middle;
                }
            }
            return Point::from((
                current.x + (candidate.x - current.x) * inside,
                current.y + (candidate.y - current.y) * inside,
            ));
        }

        let Some(size) =
            smithay::backend::renderer::utils::with_renderer_surface_state(&surface, |state| {
                state.surface_size()
            })
            .flatten()
        else {
            return current;
        };
        Point::from((
            (candidate.x - origin.x).clamp(0.0, (size.w - 1).max(0) as f64) + origin.x,
            (candidate.y - origin.y).clamp(0.0, (size.h - 1).max(0) as f64) + origin.y,
        ))
    }

    pub fn update_pointer_constraint(&mut self) {
        self.pointer_constraint_focus_dirty = false;
        let keyboard_client = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus())
            .and_then(|surface| surface.client())
            .map(|client| client.id());
        self.pointer_constraint_keyboard_client = Some(keyboard_client.clone());
        self.update_pointer_constraint_for_keyboard_client(keyboard_client);
    }

    fn update_pointer_constraint_for_keyboard_client(
        &mut self,
        keyboard_client: Option<smithay::reexports::wayland_server::backend::ClientId>,
    ) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let Some(surface) = pointer.current_focus() else {
            return;
        };
        let pointer_client = surface.client().map(|client| client.id());
        let keyboard_focus_matches = pointer_client.is_some() && pointer_client == keyboard_client;
        if !keyboard_focus_matches {

            smithay::wayland::pointer_constraints::with_pointer_constraint(
                &surface,
                &pointer,
                |constraint| {
                    if let Some(constraint) = constraint.filter(|constraint| constraint.is_active())
                    {
                        constraint.deactivate();
                    }
                },
            );
            return;
        }
        let position = pointer.current_location();
        let Some((focused_surface, origin)) = self.surface_under(position) else {
            return;
        };
        if focused_surface != surface {
            return;
        }
        let local = (position - origin).to_i32_round();
        smithay::wayland::pointer_constraints::with_pointer_constraint(
            &surface,
            &pointer,
            |constraint| {
                let Some(constraint) = constraint else {
                    return;
                };
                let inside = constraint
                    .region()
                    .is_none_or(|region| region.contains(local));
                if inside && keyboard_focus_matches && !constraint.is_active() {
                    constraint.activate();
                } else if (!inside || !keyboard_focus_matches) && constraint.is_active() {
                    constraint.deactivate();
                }
            },
        );
    }

    pub(crate) fn sync_pointer_constraint_for_keyboard_client(
        &mut self,
        keyboard_client: Option<smithay::reexports::wayland_server::backend::ClientId>,
    ) {
        self.pointer_constraint_focus_dirty = false;
        self.pointer_constraint_keyboard_client = Some(keyboard_client.clone());
        let had_active_constraint = self.pointer_has_active_constraint();
        self.update_pointer_constraint_for_keyboard_client(keyboard_client);
        if had_active_constraint && !self.pointer_has_active_constraint() {
            self.cursor_status = CursorImageStatus::default_named();
        }
    }

    pub(crate) fn defer_pointer_constraint_focus(&mut self) {
        self.pointer_constraint_focus_dirty = true;
    }

    pub(crate) fn apply_pending_pointer_constraint_focus(&mut self) {
        self.prune_drag();
        let keyboard_client = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus())
            .and_then(|surface| surface.client())
            .map(|client| client.id());
        if self.pointer_constraint_focus_dirty
            || self.pointer_constraint_keyboard_client.as_ref() != Some(&keyboard_client)
        {
            self.sync_pointer_constraint_for_keyboard_client(keyboard_client);
        }
    }

    pub(crate) fn keyboard_focus_lost(&mut self) {
        self.sync_pointer_constraint_for_keyboard_client(None);
    }

    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        if self.lock.active {
            return self.lock_surface_at(pos);
        }
        if self.selection_start.is_some() {
            return None;
        }
        for priority in [Layer::Overlay, Layer::Top] {
            if let Some(focus) = self.layer_surface_under(pos, priority) {
                return Some(focus);
            }
        }

        let modal = self.modal_index();
        for window in self.space.elements().rev() {
            let Some(loc) = self.space.element_location(window) else {
                continue;
            };
            let origin = loc - window.geometry().loc;
            if let Some((surface, offset)) = window.surface_under(
                pos - origin.to_f64(),
                WindowSurfaceType::POPUP | WindowSurfaceType::SUBSURFACE,
            ) {
                if self.modal_child_in(window, &modal).is_some() {
                    return None;
                }
                return Some((surface, origin.to_f64() + offset.to_f64()));
            }
            if let Some(body) = self.space.element_geometry(window) {
                if crate::decoration::enabled(window)
                    && self.decoration_for_window(window).hit(body, pos).is_some()
                {
                    return None;
                }
            }

            if let Some((surface, offset)) =
                window.surface_under(pos - origin.to_f64(), WindowSurfaceType::ALL)
            {
                if self.modal_child_in(window, &modal).is_some() {
                    return None;
                }
                return Some((surface, origin.to_f64() + offset.to_f64()));
            }
        }
        for priority in [Layer::Bottom, Layer::Background] {
            if let Some(focus) = self.layer_surface_under(pos, priority) {
                return Some(focus);
            }
        }
        None
    }

    fn layer_surface_under(
        &self,
        pos: Point<f64, Logical>,
        priority: Layer,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        for output in self.space.outputs() {
            let Some(output_geo) = self.space.output_geometry(output) else {
                continue;
            };
            if !output_geo.to_f64().contains(pos) {
                continue;
            }
            let local = pos - output_geo.loc.to_f64();
            let map = layer_map_for_output(output);
            for layer in map.layers_on(priority).rev() {
                if !crate::toplevel_observer::visible(layer.wl_surface()) {
                    continue;
                }
                let Some(layer_geo) = map.layer_geometry(layer) else {
                    continue;
                };

                let origin = layer_geo.loc - layer.bbox().loc + crate::layer_layout::offset(layer);
                if let Some((surface, surface_loc)) =
                    layer.surface_under(local - origin.to_f64(), WindowSurfaceType::ALL)
                {
                    return Some((
                        surface,
                        output_geo.loc.to_f64() + origin.to_f64() + surface_loc.to_f64(),
                    ));
                }
            }
        }
        None
    }

    pub fn focus_layer_at(
        &mut self,
        pos: Point<f64, Logical>,
        serial: smithay::utils::Serial,
    ) -> Option<bool> {
        if self.lock.active {
            self.focus_lock(self.lock_surface_at(pos).map(|(surface, _)| surface));
            return Some(true);
        }
        let (surface, _) = self.surface_under(pos)?;
        let layer = self
            .space
            .layer_for_surface(&surface, WindowSurfaceType::ALL)?;
        if layer.can_receive_keyboard_focus() {
            if let Some(keyboard) = self.seat.get_keyboard() {
                keyboard.set_focus(self, Some(layer.wl_surface().clone()), serial);
            }
        }
        Some(true)
    }

    pub fn send_frame_callbacks(&self, output: &Output, time: std::time::Duration) {
        if self.lock.active {
            for (_, surface) in self.lock.surfaces.iter().filter(|(o, _)| o == output) {
                smithay::desktop::utils::send_frames_surface_tree(
                    surface.wl_surface(),
                    output,
                    time,
                    Some(std::time::Duration::ZERO),
                    |_, _| Some(output.clone()),
                );
            }
            return;
        }
        for window in self.space.elements() {
            window.send_frame(output, time, Some(std::time::Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
        let layers: Vec<_> = layer_map_for_output(output).layers().cloned().collect();
        for layer in layers {
            layer.send_frame(output, time, Some(std::time::Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
        if self.drag.active {
            if let Some(icon) = self.drag.icon.as_ref().filter(|surface| surface.alive()) {
                smithay::desktop::utils::send_frames_surface_tree(
                    icon,
                    output,
                    time,
                    Some(std::time::Duration::ZERO),
                    |_, _| Some(output.clone()),
                );
            }
        }
        if let (CursorImageStatus::Surface(surface), Some(pointer), Some(geometry)) = (
            &self.cursor_status,
            self.seat.get_pointer(),
            self.space.output_geometry(output),
        ) {
            if surface.alive() && geometry.to_f64().contains(pointer.current_location()) {
                smithay::desktop::utils::send_frames_surface_tree(
                    surface,
                    output,
                    time,
                    Some(std::time::Duration::ZERO),
                    |_, _| Some(output.clone()),
                );
            }
        }
    }

    pub(crate) fn maximize_on_output(&mut self, id: u64, output: &Output) -> Result<(), String> {
        self.toggle_maximize(id, Some(output))
    }
}

impl RixState {
    pub fn work_area(&self, output: &Output) -> smithay::utils::Rectangle<i32, Logical> {
        let mut area = layer_map_for_output(output).non_exclusive_zone();
        if let Some(geometry) = self.space.output_geometry(output) {
            area.loc += geometry.loc;
        }
        area
    }
    pub(crate) fn sync_layer_keyboard_focus(&mut self) {
        if self.lock.active {
            return;
        }
        use smithay::wayland::shell::wlr_layer::KeyboardInteractivity;
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let exclusive = self
            .layer_surfaces
            .iter()
            .rev()
            .find(|l| l.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive)
            .map(|l| l.wl_surface().clone());
        let current = keyboard.current_focus();
        if let Some(surface) = exclusive {
            if current.as_ref() != Some(&surface) {
                keyboard.set_focus(
                    self,
                    Some(surface),
                    smithay::utils::SERIAL_COUNTER.next_serial(),
                );
            }
        } else if current.as_ref().is_some_and(|surface| {
            self.layer_surfaces
                .iter()
                .any(|l| l.wl_surface() == surface && !l.can_receive_keyboard_focus())
        }) {
            let restore = self
                .windows
                .entries
                .iter()
                .find(|(id, _)| Some(*id) == self.windows.active)
                .and_then(|(_, w)| w.toplevel().map(|t| t.wl_surface().clone()));
            keyboard.set_focus(self, restore, smithay::utils::SERIAL_COUNTER.next_serial());
        }
    }

    pub fn pointer_released(&mut self, button: u32) {
        self.pressed_buttons.remove(&button);
        if button == 0x110 {
            self.selection_start = None;
            if let Some((id, pressed)) = self.decoration_button_press.take() {
                let released = self
                    .seat
                    .get_pointer()
                    .and_then(|pointer| self.decoration_under(pointer.current_location()))
                    .and_then(|(window, hit)| {
                        self.windows
                            .entries
                            .iter()
                            .find(|(_, known)| *known == window)
                            .map(|(id, _)| (*id, hit))
                    });
                if released == Some((id, pressed)) {
                    use crate::decoration::DecorationHit;
                    let action = match pressed {
                        DecorationHit::Minimize => Some(rix_ipc::WindowAction::Minimize(id)),
                        DecorationHit::Maximize => Some(rix_ipc::WindowAction::Maximize(id)),
                        DecorationHit::Close => Some(rix_ipc::WindowAction::Close(id)),
                        _ => None,
                    };
                    if let Some(action) = action {
                        let _ = self.apply_window_action(action);
                    }
                }
            }
        }
    }

    pub(crate) fn decoration_under(
        &self,
        position: Point<f64, Logical>,
    ) -> Option<(Window, crate::decoration::DecorationHit)> {
        if self.lock.active
            || [Layer::Overlay, Layer::Top]
                .into_iter()
                .any(|layer| self.layer_surface_under(position, layer).is_some())
        {
            return None;
        }
        for window in self.space.elements().rev() {
            let Some(origin) = self
                .space
                .element_location(window)
                .map(|loc| loc - window.geometry().loc)
            else {
                continue;
            };
            if window
                .surface_under(
                    position - origin.to_f64(),
                    WindowSurfaceType::POPUP | WindowSurfaceType::SUBSURFACE,
                )
                .is_some()
            {
                return None;
            }
            if let Some(body) = self.space.element_geometry(window) {
                if crate::decoration::enabled(window) {
                    if let Some(hit) = self.decoration_for_window(window).hit(body, position) {
                        return Some((window.clone(), hit));
                    }
                }
            }
            if window
                .surface_under(position - origin.to_f64(), WindowSurfaceType::ALL)
                .is_some()
            {
                return None;
            }
        }
        None
    }

    pub fn pointer_pressed(&mut self, button: u32, serial: smithay::utils::Serial, time_msec: u32) {
        self.logo_tap = None;
        if button == 0x110 {
            self.decoration_button_press = None;
        }
        if button != 0x110 {
            self.titlebar_clicks.reset();
        }
        self.pressed_buttons.insert(button);
        if self.drag.active {
            self.titlebar_clicks.reset();
            return;
        }
        let Some(pointer) = self.seat.get_pointer() else {
            self.titlebar_clicks.reset();
            return;
        };
        let position = pointer.current_location();
        if self.focus_layer_at(position, serial).is_some() {
            self.titlebar_clicks.reset();
            return;
        }
        if let Some(dialog) = self.modal_blocked_at(position) {
            self.titlebar_clicks.reset();
            self.focus_window(&dialog);
            self.refresh_modal_pointer_focus();
            return;
        }
        if let Some(sender) = &self.ipc_event_sender {
            let _ = sender.send(rix_ipc::IpcEvent::ShellPopupsDismissed);
        }
        let decoration = self.decoration_under(position);
        if button == 0x110 && decoration.is_none() && self.surface_under(position).is_none() {
            self.titlebar_clicks.reset();
            self.selection_start = Some(position);
            return;
        }
        if button == 0x111 && decoration.is_none() && self.surface_under(position).is_none() {
            if let Some(output) = self.space.outputs().find(|o| {
                self.space
                    .output_geometry(o)
                    .is_some_and(|g| g.to_f64().contains(position))
            }) {
                if let Some(geometry) = self.space.output_geometry(output) {
                    self.publish_shell(
                        Some(&output.name()),
                        rix_ipc::ShellEvent::DesktopContextMenu {
                            x: position.x - geometry.loc.x as f64,
                            y: position.y - geometry.loc.y as f64,
                        },
                    );
                }
            }
            return;
        }
        if let Some((window, hit)) = decoration {
            self.focus_window(&window);
            if button != 0x110 {
                self.titlebar_clicks.reset();
                return;
            }
            let id = self
                .windows
                .entries
                .iter()
                .find(|(_, w)| w == &window)
                .map(|(id, _)| *id);
            use crate::decoration::DecorationHit;
            match (hit, id) {
                (
                    hit
                    @ (DecorationHit::Close | DecorationHit::Maximize | DecorationHit::Minimize),
                    Some(id),
                ) => {
                    self.titlebar_clicks.reset();
                    self.decoration_button_press = Some((id, hit));
                }
                (DecorationHit::Move, _) if !crate::window_manager::is_fullscreen(&window) => {
                    if let Some(id) = id {
                        if self.titlebar_clicks.press(id, time_msec, position) {
                            let _ = self.apply_window_action(rix_ipc::WindowAction::Maximize(id));
                            return;
                        }
                    } else {
                        self.titlebar_clicks.reset();
                    }
                    if let Some(location) = self.space.element_location(&window) {
                        pointer.set_grab(
                            self,
                            crate::grabs::MoveSurfaceGrab {
                                start_data: smithay::input::pointer::GrabStartData {
                                    focus: None,
                                    button,
                                    location: position,
                                },
                                window,
                                initial_window_location: location,
                                snap_output: None,
                            },
                            serial,
                            smithay::input::pointer::Focus::Clear,
                        );
                    }
                }
                (DecorationHit::Resize(edges), _)
                    if !crate::window_manager::is_fullscreen(&window) =>
                {
                    self.resize_cursor = Some(crate::grabs::resize_cursor(edges));
                    if let Some(location) = self.space.element_location(&window) {
                        let size = window.geometry().size;
                        pointer.set_grab(
                            self,
                            crate::grabs::ResizeSurfaceGrab {
                                start_data: smithay::input::pointer::GrabStartData {
                                    focus: None,
                                    button,
                                    location: position,
                                },
                                window,
                                edges,
                                initial_window_location: location,
                                initial_window_size: size,
                            },
                            serial,
                            smithay::input::pointer::Focus::Clear,
                        );
                    }
                }
                _ => self.titlebar_clicks.reset(),
            }
            return;
        }
        self.titlebar_clicks.reset();
        let clicked = self
            .space
            .element_under(position)
            .map(|(w, loc)| (w.clone(), loc));
        if let Some((window, location)) = clicked {
            self.focus_window(&window);
            let mods = self
                .seat
                .get_keyboard()
                .map(|k| k.modifier_state())
                .unwrap_or_default();
            if button == 0x110
                && (mods.alt || mods.logo)
                && !crate::window_manager::is_fullscreen(&window)
            {
                if let Some(start_data) = pointer.grab_start_data() {
                    pointer.set_grab(
                        self,
                        crate::grabs::MoveSurfaceGrab {
                            start_data,
                            window,
                            initial_window_location: location,
                            snap_output: None,
                        },
                        serial,
                        smithay::input::pointer::Focus::Clear,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod titlebar_click_tests {
    use super::TitlebarClickTracker;
    use smithay::utils::{Logical, Point};

    #[test]
    fn recognizes_a_second_click_on_the_same_titlebar() {
        let mut clicks = TitlebarClickTracker::default();
        assert!(!clicks.press(7, 100, Point::<f64, Logical>::from((20., 12.))));
        assert!(clicks.press(7, 420, Point::<f64, Logical>::from((24., 16.))));
    }

    #[test]
    fn starts_a_new_sequence_for_a_different_window_or_distant_click() {
        let mut clicks = TitlebarClickTracker::default();
        assert!(!clicks.press(7, 100, Point::<f64, Logical>::from((20., 12.))));
        assert!(!clicks.press(8, 200, Point::<f64, Logical>::from((20., 12.))));
        assert!(!clicks.press(8, 300, Point::<f64, Logical>::from((40., 12.))));
        assert!(clicks.press(8, 350, Point::<f64, Logical>::from((42., 14.))));
    }

    #[test]
    fn handles_timestamp_wrap_and_rejects_a_slow_second_click() {
        let mut clicks = TitlebarClickTracker::default();
        assert!(!clicks.press(7, u32::MAX - 100, Point::<f64, Logical>::from((20., 12.))));
        assert!(clicks.press(7, 100, Point::<f64, Logical>::from((20., 12.))));

        assert!(!clicks.press(7, 200, Point::<f64, Logical>::from((20., 12.))));
        assert!(!clicks.press(7, 700, Point::<f64, Logical>::from((20., 12.))));
    }
}

#[cfg(test)]
mod client_prune_tests {
    use super::*;
    use std::sync::{atomic::Ordering, Arc};

    #[test]
    fn keeps_live_clients_and_drops_disconnected_handles() {
        let mut display = wayland_server::Display::<RixState>::new().unwrap();
        let mut state = RixState::new(&mut display, "client-prune-test".into());
        let mut handle = display.handle();

        let (live_socket, _live_peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let live = handle
            .insert_client(live_socket, Arc::new(ClientState::default()))
            .unwrap();
        let (dead_socket, _dead_peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let dead_data = Arc::new(ClientState::default());
        let dead = handle
            .insert_client(dead_socket, dead_data.clone())
            .unwrap();
        dead_data.disconnected.store(true, Ordering::Release);

        state.clients.push(live.clone());
        state.clients.push(dead);
        state.prune_disconnected_clients();
        assert_eq!(state.clients.len(), 1, "a live client must survive pruning");
        assert_eq!(state.clients[0].id(), live.id());
        state.prune_disconnected_clients();
        assert_eq!(state.clients.len(), 1, "pruning must be idempotent");
    }
}

#[cfg(test)]
mod window_material_tests {
    use super::*;
    #[test]
    fn app_rules_reload_geometry_and_restore_live_desktop_theme() {
        let mut display = wayland_server::Display::<RixState>::new().unwrap();
        let mut state = RixState::new(&mut display, "material-test".into());
        let mut desktop = rix_config::ThemeSettings::default();
        desktop
            .appearance
            .metrics
            .insert("decoration_title_height".into(), 38.);
        state.set_decoration_theme(&desktop);
        let first = state.decoration_generation;
        let mut custom = desktop.clone();
        custom
            .appearance
            .metrics
            .insert("decoration_title_height".into(), 54.);
        let rules = rix_config::WindowAppearanceSettings {
            use_desktop_material: true,
            rules: vec![rix_config::WindowAppearanceRule {
                app_id: "org.example.Editor".into(),
                use_desktop_material: false,
                theme: Some(custom),
            }],
        };
        state.set_window_appearance(rules.clone());
        assert!(state.decoration_generation > first);
        assert_eq!(state.decoration_for_app("org.example.Editor").height, 54);
        assert_eq!(state.decoration_for_app("org.example.Other").height, 38);
        let body = Rectangle::from_size((400, 300).into());
        assert!(state
            .decoration_for_app("org.example.Editor")
            .backdrop(body)
            .is_none());
        assert!(state
            .decoration_for_app("org.example.Other")
            .backdrop(body)
            .is_some());
        let generation = state.decoration_generation;
        state.set_window_appearance(rules);
        assert_eq!(
            state.decoration_generation, generation,
            "unrelated desktop changes must not rerasterize every window"
        );
        desktop
            .appearance
            .metrics
            .insert("decoration_title_height".into(), 44.);
        state.set_decoration_theme(&desktop);
        assert_eq!(state.decoration_for_app("org.example.Editor").height, 54);
        assert_eq!(state.decoration_for_app("org.example.Other").height, 44);
        state.set_window_appearance(Default::default());
        assert_eq!(state.decoration_for_app("org.example.Editor").height, 44);
        assert!(state
            .decoration_for_app("org.example.Editor")
            .backdrop(body)
            .is_some());
    }
}
