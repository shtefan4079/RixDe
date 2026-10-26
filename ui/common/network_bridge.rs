use super::{icon, model, reconcile};
use crate::{NetworkDeviceRow, NetworkProfileDraft, NetworkRow, SystemHost, SystemState};
use rix_services::network::{Action, ProfileDraft, Snapshot};
use slint::ComponentHandle;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    sync::Arc,
};

struct Binding {
    window: slint::Weak<SystemHost>,
    config: Arc<rix_config::ConfigManager>,
    commands: tokio::sync::mpsc::Sender<Action>,
    snapshot: RefCell<Snapshot>,
    local_error: RefCell<String>,
    local_error_operation: Cell<u64>,
    device: RefCell<String>,
    suppressed_credential: RefCell<String>,
    profile_started: Cell<Option<u64>>,
    form_started: Cell<Option<u64>>,
}
thread_local! { static BINDINGS: RefCell<HashMap<usize, Rc<Binding>>> = RefCell::new(HashMap::new()); }
fn key(window: &SystemHost) -> usize {
    window.window() as *const slint::Window as usize
}
fn binding(window: &SystemHost) -> Option<Rc<Binding>> {
    BINDINGS.with(|b| b.borrow().get(&key(window)).cloned())
}
fn text(value: &str) -> slint::SharedString {
    crate::i18n::tr(value)
}
fn report(window: &SystemHost, message: &str) {
    if let Some(binding) = binding(window) {
        *binding.local_error.borrow_mut() = message.to_owned();
        binding
            .local_error_operation
            .set(binding.snapshot.borrow().operation_id);
    }
    let global = window.global::<SystemState>();
    global.set_network_error(text(message));
    global.set_network_form_error(text(message));
    global.set_network_form_state(3);
    global.set_network_profile_busy(false);
}
fn send(window: &SystemHost, binding: &Binding, action: Action) -> bool {
    match binding.commands.try_send(action) {
        Ok(()) => true,
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            report(window, "Network operation queue is full. Try again.");
            false
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            report(window, "Network service has stopped. Reopen Settings.");
            false
        }
    }
}
fn send_credential(window: &SystemHost, binding: &Binding, action: Action) -> bool {
    match binding.commands.try_send(action) {
        Ok(()) => true,
        Err(tokio::sync::mpsc::error::TrySendError::Full(action)) => {
            let commands = binding.commands.clone();
            let weak = window.as_weak();
            tokio::spawn(async move {
                let result =
                    tokio::time::timeout(std::time::Duration::from_secs(2), commands.send(action))
                        .await;
                if !matches!(result, Ok(Ok(()))) {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            report(
                                &w,
                                "Network credential response could not be delivered. Try again.",
                            );
                            let global = w.global::<SystemState>();
                            global.set_network_credential_failed(
                                global.get_network_credential_failed().wrapping_add(1),
                            );
                        }
                    });
                }
            });
            true
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            report(window, "Network service has stopped. Reopen Settings.");
            false
        }
    }
}
fn network_icon(name: &str, theme: &str) -> slint::Image {
    for candidate in [name, name.strip_suffix("-symbolic").unwrap_or(name)] {
        let image = icon(candidate, theme);
        if image.size().width > 0 {
            return image;
        }
    }
    let svg: &[u8] = if name.contains("error") {
        include_bytes!("icons/network-error.svg")
    } else if name.contains("offline") || name.contains("disabled") {
        include_bytes!("icons/network-offline.svg")
    } else if name.contains("wired") {
        include_bytes!("icons/network-wired.svg")
    } else if name.contains("cellular") {
        include_bytes!("icons/network-cellular.svg")
    } else if name.contains("vpn") {
        include_bytes!("icons/network-vpn.svg")
    } else if name.contains("weak") {
        include_bytes!("icons/network-weak.svg")
    } else if name.contains("-ok") {
        include_bytes!("icons/network-ok.svg")
    } else if name.contains("good") {
        include_bytes!("icons/network-good.svg")
    } else {
        include_bytes!("icons/network-wireless.svg")
    };
    slint::Image::load_from_svg_data(svg).unwrap_or_default()
}
fn strength_icon(strength: u8, active: bool) -> &'static str {
    match (active, strength) {
        (true, 76..=100) => "network-wireless-signal-excellent-symbolic",
        (true, 51..=75) => "network-wireless-signal-good-symbolic",
        (true, 26..=50) => "network-wireless-signal-ok-symbolic",
        (true, _) => "network-wireless-signal-weak-symbolic",
        (false, 76..=100) => "network-wireless-signal-excellent-symbolic",
        (false, 51..=75) => "network-wireless-signal-good-symbolic",
        (false, 26..=50) => "network-wireless-signal-ok-symbolic",
        _ => "network-wireless-signal-weak-symbolic",
    }
}
pub fn refresh_icons(window: &SystemHost, theme: &str) {
    if let Some(binding) = binding(window) {
        render(window, &binding, Some(theme));
    }
}
fn validate_password(security: &str, password: &str) -> slint::SharedString {
    let valid = match security {
        "wpa-psk" | "wpa2-wpa3" => {
            password.is_ascii()
                && ((8..=63).contains(&password.len())
                    || (password.len() == 64 && password.bytes().all(|b| b.is_ascii_hexdigit())))
        }
        "open" | "owe" | "owe-transition" => true,
        _ => !password.is_empty(),
    };
    if valid {
        "".into()
    } else if matches!(security, "wpa-psk" | "wpa2-wpa3") {
        text("WPA2 passwords must contain 8 to 63 ASCII characters or 64 hexadecimal digits")
    } else {
        text("Enter the network password")
    }
}

fn row_status(n: &rix_services::network::Network, pending: &str) -> String {
    let status = if !n.error.is_empty() {
        text(&n.error).to_string()
    } else if n.active {
        if n.device_kind == 2 {
            format!("{} · {}%", text("Connected"), n.strength)
        } else {
            text("Connected").to_string()
        }
    } else if n.path == pending || (40..100).contains(&n.state) {
        text("Connecting…").to_string()
    } else if n.state == 10 {
        text("Device is unmanaged").to_string()
    } else if n.state == 20 {
        text("Device is unavailable").to_string()
    } else if n.state == 110 {
        text("Disconnecting…").to_string()
    } else if n.state == 120 {
        text("Connection failed").to_string()
    } else if !n.available {
        text("Network is currently unavailable").to_string()
    } else if n.wired {
        text("Ready to connect").to_string()
    } else {
        let security = match n.security.as_str() {
            "open" => text("Open network").to_string(),
            "enterprise" => "WPA Enterprise".into(),
            "wep" => "WEP".into(),
            "owe" => "OWE".into(),
            "sae" => "WPA3".into(),
            "wpa-psk" => "WPA/WPA2".into(),
            other => other.into(),
        };
        let saved = if n.saved.is_empty() {
            "".to_owned()
        } else {
            text(" · Saved").to_string()
        };
        format!("{security} · {}%{saved}", n.strength)
    };
    status
}
fn user_network(snapshot: &Snapshot, network: &rix_services::network::Network) -> bool {
    if network.kind == "vpn" || network.kind == "wireguard" {
        return true;
    }
    if !matches!(
        network.kind.as_str(),
        "802-3-ethernet" | "802-11-wireless" | "gsm" | "cdma"
    ) {
        return false;
    }

    if (network.wired || network.kind == "802-3-ethernet") && !network.active {
        return false;
    }
    if let Some(device) = snapshot
        .devices
        .iter()
        .find(|device| device.path == network.device)
    {
        device.user_visible()
    } else {

        !network.saved.is_empty() || network.iface.is_empty()
    }
}
fn friendly_name(snapshot: &Snapshot, network: &rix_services::network::Network) -> String {
    if network.wired && (network.name == network.iface || network.name.is_empty()) {
        snapshot
            .profiles
            .iter()
            .find(|profile| profile.path == network.profile_path)
            .map(|profile| profile.name.clone())
            .filter(|name| !name.is_empty() && name != &network.iface)
            .unwrap_or_else(|| text("Ethernet").to_string())
    } else {
        network.name.clone()
    }
}
fn rate_text(bytes_per_second: u64, _language: &str) -> String {
    let (divisor, unit) = if bytes_per_second < 1024 {
        (1., "B/s")
    } else if bytes_per_second < 1024 * 1024 {
        (1024., "KiB/s")
    } else if bytes_per_second < 1024 * 1024 * 1024 {
        (1024. * 1024., "MiB/s")
    } else {
        (1024. * 1024. * 1024., "GiB/s")
    };
    let number = if divisor == 1. {
        bytes_per_second.to_string()
    } else {
        format!("{:.1}", bytes_per_second as f64 / divisor)
    };
    format!("{number} {}", text(unit))
}
fn adapter_name(device: &rix_services::network::Device) -> slint::SharedString {
    format!(
        "{} · {}",
        text(match device.kind {
            1 => "Ethernet",
            2 => "Wi-Fi",
            8 => "Mobile network",
            16 | 29 => "VPN",
            _ => "Adapter",
        }),
        device.iface
    )
    .into()
}
fn render(window: &SystemHost, binding: &Binding, theme_override: Option<&str>) {
    let snapshot = binding.snapshot.borrow();
    let global = window.global::<SystemState>();
    let theme = theme_override
        .map(str::to_owned)
        .unwrap_or_else(|| binding.config.get_theme().icon_theme);
    global.set_network_service_ready(snapshot.available);
    let allowed = |permission: &str| {
        snapshot.available
            && !snapshot
                .permissions
                .iter()
                .any(|(key, value)| key == permission && value == "no")
    };
    let can_toggle = allowed("org.freedesktop.NetworkManager.enable-disable-wifi");
    let can_connect = allowed("org.freedesktop.NetworkManager.network-control");
    let can_edit = allowed("org.freedesktop.NetworkManager.settings.modify.own")
        || allowed("org.freedesktop.NetworkManager.settings.modify.system");
    global.set_network_can_toggle(can_toggle);
    global.set_network_can_connect(can_connect);
    global.set_network_can_edit(can_edit);
    global.set_network_can_save_own(allowed(
        "org.freedesktop.NetworkManager.settings.modify.own",
    ));
    global.set_network_can_save_shared(allowed(
        "org.freedesktop.NetworkManager.settings.modify.system",
    ));
    global.set_wifi_enabled(snapshot.wifi);
    global.set_network_has_wifi(
        snapshot
            .devices
            .iter()
            .any(|device| device.kind == 2 && device.user_visible()),
    );
    global.set_wifi_hardware(snapshot.hardware && snapshot.devices.iter().any(|d| d.kind == 2));
    global.set_networking_enabled(snapshot.networking);
    global.set_network_can_networking(allowed(
        "org.freedesktop.NetworkManager.enable-disable-network",
    ));
    global.set_network_can_scan(
        allowed("org.freedesktop.NetworkManager.wifi.scan")
            && snapshot
                .devices
                .iter()
                .any(|d| d.kind == 2 && d.available && d.managed),
    );
    global.set_network_scanning(snapshot.scanning);
    global.set_network_secret_agent(snapshot.secret_agent);
    if snapshot.operation_done && snapshot.operation_id > binding.local_error_operation.get() {
        binding.local_error.borrow_mut().clear();
    }
    let error = if snapshot.error.is_empty() {
        binding.local_error.borrow().clone()
    } else {
        snapshot.error.clone()
    };
    global.set_network_error(text(&error));
    if !snapshot.available && binding.form_started.get().is_some() {
        global.set_network_form_error(text("NetworkManager is unavailable"));
        global.set_network_form_state(3);
        binding.form_started.set(None);
    }
    if !snapshot.available && binding.profile_started.get().is_some() {
        global.set_network_profile_busy(false);
        binding.profile_started.set(None);
    }
    global.set_network_portal(snapshot.connectivity == 2);
    global.set_connectivity(text(match snapshot.connectivity {
        4 => "Connected to the internet",
        3 => "Limited connectivity",
        2 => "Sign-in required",
        1 => "No connection",
        _ => "Internet connectivity is unknown",
    }));
    let status = if !snapshot.available {
        "NetworkManager is unavailable"
    } else if !snapshot.networking {
        "Networking is disabled"
    } else if !snapshot.devices.iter().any(|d| d.kind == 2)
        && !snapshot.devices.iter().any(|d| d.physical_uplink())
    {
        "No Wi-Fi adapter is available"
    } else if !snapshot.devices.iter().any(|d| d.kind == 2) {
        ""
    } else if !snapshot.hardware {
        "Wi-Fi is blocked by hardware"
    } else if !snapshot.wifi {
        "Wi-Fi is off"
    } else if snapshot
        .devices
        .iter()
        .filter(|d| d.kind == 2)
        .all(|d| !d.managed)
    {
        "Wi-Fi adapters are unmanaged"
    } else if !snapshot.secret_agent {
        "Network password agent is unavailable"
    } else {
        ""
    };
    let mut statuses = vec![text(status).to_string()];
    if snapshot.available && !can_toggle {
        statuses.push(text("Wi-Fi switching is denied by system policy").to_string());
    }
    if snapshot.available && !can_connect {
        statuses.push(text("Connecting and disconnecting are denied by system policy").to_string());
    }
    if snapshot.available && !can_edit {
        statuses.push(text("Editing network profiles is denied by system policy").to_string());
    }
    global.set_network_status(
        statuses
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · ")
            .into(),
    );
    let active = snapshot
        .networks
        .iter()
        .find(|n| n.active && n.default_route && user_network(&snapshot, n))
        .or_else(|| {
            snapshot.networks.iter().find(|n| {
                n.active && n.device == snapshot.primary_device && user_network(&snapshot, n)
            })
        })
        .or_else(|| {
            snapshot
                .networks
                .iter()
                .find(|n| n.active && user_network(&snapshot, n))
        });
    let panel_icon = if !snapshot.available || !snapshot.networking {
        "network-offline-symbolic"
    } else if snapshot.connectivity == 2
        || snapshot.connectivity == 3
        || (snapshot.connectivity == 1 && active.is_some())
    {
        "network-error-symbolic"
    } else if snapshot.vpn_active {
        "network-vpn-symbolic"
    } else if let Some(n) = active {
        if n.kind == "vpn" || n.kind == "wireguard" {
            "network-vpn-symbolic"
        } else if n.kind == "gsm" || n.kind == "cdma" {
            "network-cellular-signal-excellent-symbolic"
        } else if n.wired {
            "network-wired-symbolic"
        } else {
            strength_icon(n.strength, true)
        }
    } else if !snapshot.wifi || !snapshot.hardware {
        "network-wireless-disabled-symbolic"
    } else {
        "network-wireless-offline-symbolic"
    };
    global.set_network_icon(network_icon(panel_icon, &theme));
    global.set_network_icon_label(if let Some(n) = active {
        format!(
            "{} · {}",
            friendly_name(&snapshot, n),
            global.get_connectivity()
        )
        .into()
    } else {
        global.get_connectivity()
    });
    global.set_network_active_name(
        active
            .map(|network| friendly_name(&snapshot, network))
            .unwrap_or_default()
            .into(),
    );
    global.set_network_traffic_visible(
        snapshot.available
            && snapshot.networking
            && snapshot
                .devices
                .iter()
                .any(|device| device.physical_uplink()),
    );
    let language = crate::i18n::language_code(&binding.config.get_desktop().language);
    global.set_network_download_rate(if snapshot.traffic.ready {
        rate_text(snapshot.traffic.download_bytes_per_second, language).into()
    } else {
        "—".into()
    });
    global.set_network_upload_rate(if snapshot.traffic.ready {
        rate_text(snapshot.traffic.upload_bytes_per_second, language).into()
    } else {
        "—".into()
    });
    global.set_network_devices(reconcile(
        global.get_network_devices(),
        snapshot
            .devices
            .iter()
            .filter(|d| d.user_visible())
            .map(|d| NetworkDeviceRow {
                path: d.path.clone().into(),
                name: adapter_name(d),
                iface: d.iface.clone().into(),
                kind: d.kind as i32,
                state: d.state as i32,
                managed: d.managed,
                hardware: d.hardware,
                available: d.available,
                scanning: d.scanning,
            })
            .collect(),
        |row| row.path.clone(),
    ));
    let mut names = vec![text("All adapters")];
    names.extend(
        snapshot
            .devices
            .iter()
            .filter(|d| d.kind == 2 && d.user_visible())
            .map(|d| d.iface.clone().into()),
    );
    global.set_network_device_names(model(names));
    if !binding.device.borrow().is_empty()
        && !snapshot
            .devices
            .iter()
            .any(|d| d.path == *binding.device.borrow() && d.user_visible())
    {
        binding.device.borrow_mut().clear();
        global.set_network_device_index(0);
    }
    let search = global.get_network_search().to_lowercase();
    let selected_device = binding.device.borrow();
    let rows: Vec<_> = snapshot
        .networks
        .iter()
        .filter(|n| {
            user_network(&snapshot, n)
                && (selected_device.is_empty()
                    || n.device == *selected_device
                    || (n.wired && n.active)
                    || n.kind == "vpn"
                    || n.kind == "wireguard")
                && (search.is_empty()
                    || n.name.to_lowercase().contains(&search)
                    || n.iface.to_lowercase().contains(&search))
        })
        .map(|n| NetworkRow {
            path: n.path.clone().into(),
            name: friendly_name(&snapshot, n).into(),
            detail: row_status(n, &snapshot.pending).into(),
            icon: network_icon(
                if n.kind == "vpn" || n.kind == "wireguard" {
                    "network-vpn-symbolic"
                } else if n.kind == "gsm" || n.kind == "cdma" {
                    "network-cellular-signal-excellent-symbolic"
                } else if n.wired {
                    "network-wired-symbolic"
                } else {
                    strength_icon(n.strength, n.active)
                },
                &theme,
            ),
            strength: n.strength as i32,
            active: n.active,
            saved: !n.saved.is_empty(),
            password: !n.wired && !["open", "owe", "owe-transition"].contains(&n.security.as_str()),
            supported: n.available
                && n.state != 10
                && (!n.wired || n.state >= 30)
                && (!n.saved.is_empty()
                    || n.wired
                    || [
                        "open",
                        "owe",
                        "owe-transition",
                        "wpa-psk",
                        "sae",
                        "wpa2-wpa3",
                    ]
                    .contains(&n.security.as_str())),
            busy: (40..100).contains(&n.state) || n.state == 110 || snapshot.pending == n.path,
            device: n.device.clone().into(),
            iface: n.iface.clone().into(),
            security: n.security.clone().into(),
            available: n.available,
            wired: n.wired,
            editable: snapshot
                .profiles
                .iter()
                .find(|p| p.path == n.saved)
                .map(|p| {
                    allowed(if p.shared {
                        "org.freedesktop.NetworkManager.settings.modify.system"
                    } else {
                        "org.freedesktop.NetworkManager.settings.modify.own"
                    })
                })
                .unwrap_or(can_edit),
            error: text(&n.error),
        })
        .collect();
    global.set_networks(reconcile(global.get_networks(), rows, |row| {
        row.path.clone()
    }));
    let profile_rows = snapshot
        .profiles
        .iter()
        .filter(|p| {
            search.is_empty()
                || p.name.to_lowercase().contains(&search)
                || p.ssid.to_lowercase().contains(&search)
        })
        .map(|p| {
            let network = snapshot
                .networks
                .iter()
                .find(|n| n.profile_path == p.path || n.saved == p.path);
            NetworkRow {
                path: p.path.clone().into(),
                name: p.name.clone().into(),
                detail: format!(
                    "{} · {}",
                    if p.ssid.is_empty() {
                        p.kind.as_str()
                    } else {
                        p.ssid.as_str()
                    },
                    if p.autoconnect {
                        text("Automatic connection")
                    } else {
                        text("Manual connection")
                    }
                )
                .into(),
                icon: network_icon(
                    if p.kind == "vpn" || p.kind == "wireguard" {
                        "network-vpn-symbolic"
                    } else if p.kind == "gsm" || p.kind == "cdma" {
                        "network-cellular-signal-excellent-symbolic"
                    } else if p.kind == "802-3-ethernet" {
                        "network-wired-symbolic"
                    } else {
                        "network-wireless-symbolic"
                    },
                    &theme,
                ),
                active: network.is_some_and(|n| n.active),
                saved: true,
                editable: allowed(if p.shared {
                    "org.freedesktop.NetworkManager.settings.modify.system"
                } else {
                    "org.freedesktop.NetworkManager.settings.modify.own"
                }),
                supported: true,
                available: true,
                busy: snapshot.pending == p.path
                    || network.is_some_and(|n| (40..100).contains(&n.state)),
                wired: p.kind != "802-11-wireless",
                device: p.device.clone().into(),
                security: p.security.clone().into(),
                ..Default::default()
            }
        })
        .collect();
    global.set_network_profiles(reconcile(
        global.get_network_profiles(),
        profile_rows,
        |row| row.path.clone(),
    ));
    let form_target = global.get_network_form_target();
    if !form_target.is_empty() && binding.form_started.get().is_some() {
        if snapshot
            .networks
            .iter()
            .any(|n| n.path == form_target.as_str() && n.active)
        {
            global.set_network_form_state(2);
            binding.form_started.set(None);
        } else if snapshot.operation_id > binding.form_started.get().unwrap_or(u64::MAX)
            && snapshot.operation_done
            && snapshot.operation_target == form_target.as_str()
            && !snapshot.operation_error.is_empty()
        {
            global.set_network_form_error(text(&snapshot.operation_error));
            global.set_network_form_state(3);
            binding.form_started.set(None);
        } else if !snapshot.error.is_empty()
            && snapshot.operation_target == form_target.as_str()
            && snapshot.pending != form_target.as_str()
            && snapshot.operation_id > binding.form_started.get().unwrap_or(u64::MAX)
        {
            global.set_network_form_error(text(&snapshot.error));
            global.set_network_form_state(3);
            binding.form_started.set(None);
        }
    }
    if let Some(started) = binding.profile_started.get() {
        if snapshot.operation_id > started
            && snapshot.operation_done
            && ["save", "connect-profile", "hidden", "hotspot", "import"]
                .contains(&snapshot.operation_kind.as_str())
        {
            binding.profile_started.set(None);
            global.set_network_error(text(&snapshot.operation_error));
            global.set_network_profile_busy(false);
            if snapshot.operation_error.is_empty() {
                global.set_network_profile_editing(false);
                global.set_network_profile_draft(Default::default());
            } else {
                global.set_network_error(text(&snapshot.operation_error));
            }
        }
    }
    let credential = snapshot
        .credentials
        .iter()
        .find(|c| c.id.to_string() != *binding.suppressed_credential.borrow());
    let old_id = global.get_network_credential_id();
    if let Some(c) = credential {
        let id = c.id.to_string();
        global.set_network_credential_name(c.profile_name.clone().into());
        global.set_network_credential_message(if c.hints.is_empty() {
            text(&c.message)
        } else {
            format!("{} · {}", text(&c.message), c.hints.join(", ")).into()
        });
        global.set_network_credential_password_label(text(if c.setting_name == "wireguard" {
            "Private key"
        } else if c.hints.iter().any(|h| h == "pin") {
            "SIM PIN"
        } else {
            "Network password"
        }));
        global.set_network_credential_username(c.username);
        global.set_network_credential_password(c.password);
        global.set_network_credential_key_password(c.private_key_password);
        global.set_network_credential_extra(c.setting_name == "vpn");
        global.set_network_credential_id(id.clone().into());
        if old_id != id {
            global.invoke_network_show_prompt();
        }
    } else {
        global.set_network_credential_id("".into());
    }
    let deferred = global.get_network_editor_request();
    if !deferred.is_empty() && snapshot.available {
        let target = deferred.to_string();
        drop(snapshot);
        drop(selected_device);
        if edit_profile(window, binding, &target) {
            global.set_network_editor_request("".into());
        }
    }
}

fn ui_draft(d: &ProfileDraft) -> NetworkProfileDraft {
    let mut ui = NetworkProfileDraft::default();
    macro_rules! strings { ($($field:ident),* $(,)?) => { $(ui.$field = d.$field.clone().into();)* }; }
    strings!(
        path,
        name,
        uuid,
        device,
        kind,
        ssid,
        security,
        password,
        username,
        anonymous_identity,
        eap,
        phase2_auth,
        ca_cert,
        client_cert,
        private_key,
        private_key_password,
        ipv4_method,
        ipv4_addresses,
        ipv4_gateway,
        ipv4_dns,
        ipv4_routes,
        ipv6_method,
        ipv6_addresses,
        ipv6_gateway,
        ipv6_dns,
        ipv6_routes,
        mac_address,
        band,
        bssid,
        vpn_service,
        vpn_data,
        vpn_secrets,
        apn,
        number,
        pin,
        proxy_method,
        proxy_pac_url,
        proxy_pac_script,
        domain_suffix_match
    );
    ui.autoconnect = d.autoconnect;
    ui.remember = d.remember;
    ui.shared = d.shared;
    ui.hidden = d.hidden;
    ui.metered = d.metered as i32;
    ui.powersave = d.powersave as i32;
    ui.priority = d.priority;
    ui.mtu = d.mtu as i32;
    ui
}
fn service_draft(ui: NetworkProfileDraft) -> ProfileDraft {
    let mut d = ProfileDraft::default();
    macro_rules! strings { ($($field:ident),* $(,)?) => { $(d.$field = ui.$field.to_string();)* }; }
    strings!(
        path,
        name,
        uuid,
        device,
        kind,
        ssid,
        security,
        password,
        username,
        anonymous_identity,
        eap,
        phase2_auth,
        ca_cert,
        client_cert,
        private_key,
        private_key_password,
        ipv4_method,
        ipv4_addresses,
        ipv4_gateway,
        ipv4_dns,
        ipv4_routes,
        ipv6_method,
        ipv6_addresses,
        ipv6_gateway,
        ipv6_dns,
        ipv6_routes,
        mac_address,
        band,
        bssid,
        vpn_service,
        vpn_data,
        vpn_secrets,
        apn,
        number,
        pin,
        proxy_method,
        proxy_pac_url,
        proxy_pac_script,
        domain_suffix_match
    );
    d.autoconnect = ui.autoconnect;
    d.remember = ui.remember;
    d.shared = ui.shared;
    d.hidden = ui.hidden;
    d.metered = ui.metered.max(0) as u32;
    d.powersave = ui.powersave.max(0) as u32;
    d.priority = ui.priority;
    d.mtu = ui.mtu.max(0) as u32;
    d
}
fn edit_profile(window: &SystemHost, binding: &Binding, target: &str) -> bool {
    let snapshot = binding.snapshot.borrow();
    let network = snapshot
        .networks
        .iter()
        .find(|n| n.path == target || n.profile_path == target);
    let path = network
        .map(|n| {
            if n.profile_path.is_empty() {
                n.saved.as_str()
            } else {
                n.profile_path.as_str()
            }
        })
        .unwrap_or(target);
    let draft = if let Some(profile) = snapshot
        .profiles
        .iter()
        .find(|p| p.path == path || p.uuid == target)
    {
        profile.clone()
    } else if let Some(n) = network {
        ProfileDraft {
            name: n.name.clone(),
            ssid: n.name.clone(),
            device: n.device.clone(),
            security: n.security.clone(),
            kind: if n.wired {
                "802-3-ethernet"
            } else {
                "802-11-wireless"
            }
            .into(),
            ..Default::default()
        }
    } else {
        return false;
    };
    let global = window.global::<SystemState>();
    global.set_network_profile_draft(ui_draft(&draft));
    global.set_network_profile_editing(true);
    global.set_network_profile_busy(false);
    global.set_network_error("".into());
    true
}

pub fn install(window: &SystemHost, config: Arc<rix_config::ConfigManager>) {
    let service = rix_services::network::start();
    let state = Rc::new(Binding {
        window: window.as_weak(),
        config: config.clone(),
        commands: service.commands.clone(),
        snapshot: RefCell::new(Snapshot::default()),
        local_error: RefCell::new(String::new()),
        local_error_operation: Cell::new(0),
        device: RefCell::new(String::new()),
        suppressed_credential: RefCell::new(String::new()),
        profile_started: Cell::new(None),
        form_started: Cell::new(None),
    });
    BINDINGS.with(|b| {
        let mut b = b.borrow_mut();
        b.retain(|_, s| s.window.upgrade().is_some());
        b.insert(key(window), state.clone());
    });
    let global = window.global::<SystemState>();
    macro_rules! action {
        ($callback:ident, $value:expr) => {{
            let weak = window.as_weak();
            let state = state.clone();
            global.$callback(move || {
                if let Some(w) = weak.upgrade() {
                    send(&w, &state, $value);
                }
            });
        }};
    }
    action!(on_network_refresh, Action::Refresh);
    let weak = window.as_weak();
    let st = state.clone();
    global.on_network_reset_error(move || {
        if let Some(w) = weak.upgrade() {
            st.local_error.borrow_mut().clear();
            w.global::<SystemState>().set_network_error("".into());
            send(&w, &st, Action::ResetError);
        }
    });
    action!(on_network_check_connectivity, Action::CheckConnectivity);
    let weak = window.as_weak();
    let st = state.clone();
    global.on_networking_toggle(move |enabled| {
        if let Some(w) = weak.upgrade() {
            send(&w, &st, Action::Networking(enabled));
        }
    });
    let weak = window.as_weak();
    let st = state.clone();
    global.on_wifi_toggle(move |enabled| {
        if let Some(w) = weak.upgrade() {
            send(&w, &st, Action::Wifi(enabled));
        }
    });
    let weak = window.as_weak();
    let st = state.clone();
    global.on_network_connect(move |path, password, remember| {
        if let Some(w) = weak.upgrade() {
            st.form_started.set(Some(st.snapshot.borrow().operation_id));
            let g = w.global::<SystemState>();
            g.set_network_form_state(1);
            g.set_network_form_error("".into());
            let saved_profile = st
                .snapshot
                .borrow()
                .profiles
                .iter()
                .find(|p| p.path == path.as_str())
                .map(|p| p.path.clone());
            let action = if let Some(path) = saved_profile {
                Action::ActivateProfile {
                    path,
                    device: st.device.borrow().clone(),
                }
            } else {
                Action::Connect {
                    path: path.into(),
                    password: password.into(),
                    remember,
                }
            };
            send(&w, &st, action);
        }
    });
    let weak = window.as_weak();
    let st = state.clone();
    global.on_network_disconnect(move |path| {
        if let Some(w) = weak.upgrade() {
            send(&w, &st, Action::Disconnect(path.into()));
        }
    });
    let weak = window.as_weak();
    let st = state.clone();
    global.on_network_forget(move |path| {
        if let Some(w) = weak.upgrade() {
            send(&w, &st, Action::Forget(path.into()));
        }
    });
    let weak = window.as_weak();
    let st = state.clone();
    global.on_network_cancel(move |path| {
        if let Some(w) = weak.upgrade() {
            send(&w, &st, Action::Cancel(path.into()));
        }
    });
    global
        .on_network_validate_password(|security, password| validate_password(&security, &password));
    let weak = window.as_weak();
    let st = state.clone();
    global.on_network_search_changed(move |value| {
        if let Some(w) = weak.upgrade() {
            w.global::<SystemState>().set_network_search(value);
            render(&w, &st, None);
        }
    });
    let weak = window.as_weak();
    let st = state.clone();
    global.on_network_select_device(move |index| {
        if let Some(w) = weak.upgrade() {
            let device = if index > 0 {
                st.snapshot
                    .borrow()
                    .devices
                    .iter()
                    .filter(|d| d.kind == 2 && d.user_visible())
                    .nth(index as usize - 1)
                    .map(|d| d.path.clone())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            *st.device.borrow_mut() = device;
            w.global::<SystemState>().set_network_device_index(index);
            render(&w, &st, None);
        }
    });
    let weak = window.as_weak();
    let st = state.clone();
    global.on_network_edit_profile(move |target| {
        if let Some(w) = weak.upgrade() {
            if !edit_profile(&w, &st, &target) {
                w.global::<SystemState>().set_network_editor_request(target);
            }
        }
    });
    let weak = window.as_weak();
    let st = state.clone();
    global.on_network_new_profile(move |kind| {
        if let Some(w) = weak.upgrade() {
            let mut draft = ProfileDraft {
                device: st.device.borrow().clone(),
                ..Default::default()
            };
            match kind.as_str() {
                "hidden" => draft.hidden = true,
                "hotspot" => {
                    draft.kind = "hotspot".into();
                    draft.ipv4_method = "shared".into();
                }
                other => draft.kind = other.into(),
            }
            let g = w.global::<SystemState>();
            g.set_network_profile_draft(ui_draft(&draft));
            g.set_network_profile_editing(true);
            g.set_network_profile_busy(false);
            g.set_network_error("".into());
        }
    });
    let weak = window.as_weak();
    let st = state.clone();
    global.on_network_close_profile(move || {
        if let Some(w) = weak.upgrade() {
            let g = w.global::<SystemState>();
            if g.get_network_profile_busy() {
                send(&w, &st, Action::Cancel("profile-editor".into()));
            }
            st.profile_started.set(None);
            g.set_network_profile_editing(false);
            g.set_network_profile_busy(false);
            g.set_network_profile_draft(Default::default());
        }
    });
    let weak = window.as_weak();
    let st = state.clone();
    global.on_network_save_profile(move |draft, activate, hotspot, hidden| {
        if let Some(w) = weak.upgrade() {
            st.profile_started
                .set(Some(st.snapshot.borrow().operation_id));
            let g = w.global::<SystemState>();
            g.set_network_profile_busy(true);
            g.set_network_error("".into());
            let draft = service_draft(draft);
            let action = if hotspot && activate {
                Action::Hotspot(draft)
            } else if hidden && activate {
                Action::Hidden(draft)
            } else if activate {
                Action::ConnectProfile { draft }
            } else {
                Action::SaveProfile(draft)
            };
            send(&w, &st, action);
        }
    });
    let weak = window.as_weak();
    let st = state.clone();
    global.on_network_import_profile(move |path, kind| {
        if let Some(w) = weak.upgrade() {
            st.profile_started
                .set(Some(st.snapshot.borrow().operation_id));
            w.global::<SystemState>().set_network_profile_busy(true);
            send(
                &w,
                &st,
                Action::ImportProfile {
                    path: path.into(),
                    kind: kind.into(),
                },
            );
        }
    });
    let weak = window.as_weak();
    let st = state.clone();
    global.on_network_reply_secrets(
        move |id, username, password, private_key_password, extra, cancel| {
            let Some(w) = weak.upgrade() else {
                return false;
            };
            let Ok(number) = id.parse() else {
                return false;
            };
            let action = Action::ReplySecrets {
                id: number,
                username: username.into(),
                password: password.into(),
                private_key_password: private_key_password.into(),
                extra: extra.into(),
                cancel,
            };
            if !send_credential(&w, &st, action) {
                return false;
            }
            if cancel {
                *st.suppressed_credential.borrow_mut() = id.to_string();
                w.global::<SystemState>()
                    .set_network_credential_id("".into());
            }
            true
        },
    );
    let weak = window.as_weak();
    let st = state.clone();
    global.on_network_open_portal(move || {
        if let Some(w) = weak.upgrade() {
            let uri = st.snapshot.borrow().portal_uri.clone();
            let uri = if uri.starts_with("https://") || uri.starts_with("http://") {
                uri
            } else {
                "http://networkcheck.kde.org".into()
            };
            if std::process::Command::new("xdg-open")
                .arg(uri)
                .spawn()
                .is_err()
            {
                report(&w, "Could not open the network sign-in page");
            }
        }
    });
    let weak = window.as_weak();
    let st = state.clone();
    global.on_network_visible(move |visible| {
        let _ = service.visible.send(visible);
        if visible {
            let _ = service.commands.try_send(Action::Refresh);
        } else {
            if let Some(w) = weak.upgrade() {
                let g = w.global::<SystemState>();
                let id = g.get_network_credential_id();
                if let Ok(number) = id.parse() {
                    *st.suppressed_credential.borrow_mut() = id.to_string();
                    g.set_network_credential_id("".into());
                    send_credential(
                        &w,
                        &st,
                        Action::ReplySecrets {
                            id: number,
                            username: String::new(),
                            password: String::new(),
                            private_key_password: String::new(),
                            extra: String::new(),
                            cancel: true,
                        },
                    );
                }
            }
        }
    });
    let weak = window.as_weak();
    let mut updates = service.updates;
    tokio::spawn(async move {
        while updates.changed().await.is_ok() {
            let snapshot = updates.borrow_and_update().clone();
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    if let Some(st) = binding(&w) {
                        *st.snapshot.borrow_mut() = snapshot;
                        render(&w, &st, None);
                    }
                }
            });
        }
    });
    let weak = window.as_weak();
    let mut themes = config.subscribe_theme();
    tokio::spawn(async move {
        loop {
            match themes.recv().await {
                Ok(theme) => {
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            super::refresh_network_icons(&w, &theme.icon_theme);
                        }
                    });
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rates_use_binary_units_and_the_selected_decimal_separator() {
        assert_eq!(rate_text(0, "en"), format!("0 {}", text("B/s")));
        assert_eq!(rate_text(1024, "en"), format!("1.0 {}", text("KiB/s")));
        assert_eq!(rate_text(1536, "en"), format!("1.5 {}", text("KiB/s")));
        assert_eq!(
            rate_text(5 * 1024 * 1024, "en"),
            format!("5.0 {}", text("MiB/s"))
        );
    }
    #[test]
    fn compact_network_model_hides_plumbing_and_exposes_passive_current_rates() {
        use rix_services::network::{Device, Network, Traffic};
        use slint::Model;
        i_slint_backend_testing::init_no_event_loop();
        let window = SystemHost::new().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let config = Arc::new(rix_config::ConfigManager::init_in(temp.path().to_owned()).unwrap());
        let (commands, _receiver) = tokio::sync::mpsc::channel(8);
        let mut snapshot = Snapshot {
            available: true,
            networking: true,
            wifi: true,
            hardware: true,
            secret_agent: true,
            connectivity: 4,
            primary_device: "wifi".into(),
            ..Default::default()
        };
        snapshot.devices = vec![
            Device {
                path: "wifi".into(),
                iface: "wlan0".into(),
                kind: 2,
                managed: true,
                state: 100,
                ..Default::default()
            },
            Device {
                path: "eth".into(),
                iface: "eth0".into(),
                kind: 1,
                managed: true,
                state: 100,
                ..Default::default()
            },
            Device {
                path: "lo".into(),
                iface: "lo".into(),
                kind: 32,
                managed: true,
                state: 100,
                ..Default::default()
            },
            Device {
                path: "veth".into(),
                iface: "veth0".into(),
                kind: 1,
                managed: true,
                virtual_device: true,
                state: 100,
                ..Default::default()
            },
            Device {
                path: "unmanaged".into(),
                iface: "eth1".into(),
                kind: 1,
                managed: false,
                state: 100,
                ..Default::default()
            },
        ];
        for device in &snapshot.devices {
            snapshot.networks.push(Network {
                path: device.path.clone(),
                name: device.iface.clone(),
                iface: device.iface.clone(),
                device: device.path.clone(),
                device_kind: device.kind,
                kind: if device.kind == 2 {
                    "802-11-wireless"
                } else {
                    "802-3-ethernet"
                }
                .into(),
                wired: device.kind != 2,
                active: true,
                available: true,
                default_route: device.path == "wifi",
                address: "192.0.2.4".into(),
                ipv6: "2001:db8::4".into(),
                ..Default::default()
            });
        }
        snapshot.networks.push(Network {
            path: "vpn".into(),
            name: "Work VPN".into(),
            kind: "vpn".into(),
            active: true,
            available: true,
            ..Default::default()
        });
        snapshot.traffic = Traffic {
            device: "wlan0".into(),
            download_bytes_per_second: 1536,
            upload_bytes_per_second: 64,
            ready: true,
        };
        let binding = synthetic_binding(&window, config, snapshot, commands);
        render(&window, &binding, None);
        let global = window.global::<SystemState>();
        assert_eq!(global.get_network_devices().row_count(), 2);
        assert_eq!(global.get_networks().row_count(), 3);
        let rows: Vec<_> = global.get_networks().iter().collect();
        assert!(rows.iter().any(|row| row.name == text("Ethernet")));
        assert!(rows.iter().any(|row| row.name == "Work VPN"));
        assert!(rows.iter().all(|row| !row.detail.contains("192.0.2.4")
            && !row.detail.contains("2001:db8::4")
            && !row.detail.contains("wlan0")));
        assert!(global.get_network_traffic_visible());
        let language = crate::i18n::language_code(&binding.config.get_desktop().language);
        assert_eq!(
            global.get_network_download_rate(),
            rate_text(1536, language)
        );
        assert_eq!(global.get_network_upload_rate(), rate_text(64, language));
        binding.snapshot.borrow_mut().traffic.ready = false;
        render(&window, &binding, None);
        assert_eq!(global.get_network_download_rate(), "—");
    }
    #[test]
    fn password_validation_matches_byte_and_raw_psk_rules() {
        assert!(!validate_password("wpa-psk", "").is_empty());
        assert!(!validate_password("wpa-psk", "short").is_empty());
        assert!(validate_password("wpa-psk", "12345678").is_empty());
        assert!(validate_password("wpa-psk", &"a".repeat(64)).is_empty());
        assert!(!validate_password("wpa-psk", &"z".repeat(64)).is_empty());
        assert!(validate_password("sae", "1").is_empty());
        assert!(!validate_password("wpa2-wpa3", "short").is_empty());
        assert!(!validate_password("wpa-psk", "bad_non_ascii_§§§").is_empty());
        assert!(validate_password("owe-transition", "").is_empty());
    }
    #[test]
    fn draft_roundtrip_preserves_advanced_fields_without_debugging_secrets() {
        let draft = ProfileDraft {
            name: "Office".into(),
            ssid: "Company".into(),
            password: "synthetic-fixture-only".into(),
            security: "enterprise".into(),
            username: "test".into(),
            ipv4_method: "manual".into(),
            ipv4_addresses: "192.0.2.5/24".into(),
            ipv6_dns: "2001:db8::1".into(),
            vpn_service: "fixture".into(),
            vpn_data: "remote=fixture".into(),
            shared: true,
            ..Default::default()
        };
        let result = service_draft(ui_draft(&draft));
        assert!(result == draft);
    }
    fn synthetic_binding(
        window: &SystemHost,
        config: Arc<rix_config::ConfigManager>,
        snapshot: Snapshot,
        commands: tokio::sync::mpsc::Sender<Action>,
    ) -> Binding {
        Binding {
            window: window.as_weak(),
            config,
            commands,
            snapshot: RefCell::new(snapshot),
            local_error: RefCell::new(String::new()),
            local_error_operation: Cell::new(0),
            device: RefCell::new(String::new()),
            suppressed_credential: RefCell::new(String::new()),
            profile_started: Cell::new(None),
            form_started: Cell::new(None),
        }
    }
    #[test]
    fn route_permissions_and_operation_results_are_scoped_to_the_request() {
        i_slint_backend_testing::init_no_event_loop();
        let window = SystemHost::new().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let config = Arc::new(rix_config::ConfigManager::init_in(temp.path().to_owned()).unwrap());
        let (commands, _receiver) = tokio::sync::mpsc::channel(8);
        let mut snapshot = Snapshot {
            available: true,
            wifi: true,
            hardware: true,
            networking: true,
            secret_agent: true,
            connectivity: 4,
            primary_device: "fixture-wifi".into(),
            ..Default::default()
        };
        snapshot.devices.push(rix_services::network::Device {
            path: "fixture-wifi".into(),
            iface: "wlan-fixture".into(),
            kind: 2,
            state: 100,
            managed: true,
            hardware: true,
            available: true,
            ..Default::default()
        });
        snapshot.networks = vec![
            rix_services::network::Network {
                path: "wired".into(),
                name: "Ethernet fixture".into(),
                device: "fixture-ethernet".into(),
                wired: true,
                kind: "802-3-ethernet".into(),
                active: true,
                state: 100,
                available: true,
                ..Default::default()
            },
            rix_services::network::Network {
                path: "wifi".into(),
                name: "Wi-Fi fixture".into(),
                device: "fixture-wifi".into(),
                kind: "802-11-wireless".into(),
                active: true,
                state: 100,
                available: true,
                default_route: true,
                strength: 31,
                ..Default::default()
            },
        ];
        snapshot.permissions = vec![
            (
                "org.freedesktop.NetworkManager.enable-disable-wifi".into(),
                "no".into(),
            ),
            (
                "org.freedesktop.NetworkManager.network-control".into(),
                "auth".into(),
            ),
        ];
        let binding = synthetic_binding(&window, config, snapshot, commands);
        render(&window, &binding, None);
        let global = window.global::<SystemState>();
        assert!(
            global.get_network_icon_label().contains("Wi-Fi fixture"),
            "Default Wi-Fi route must win over unrelated active Ethernet"
        );
        assert!(!global.get_network_can_toggle());
        assert!(
            global.get_network_can_connect(),
            "Polkit authentication remains available"
        );
        global.set_network_form_target("requested-row".into());
        global.set_network_form_state(1);
        binding.form_started.set(Some(0));
        {
            let mut state = binding.snapshot.borrow_mut();
            state.operation_id = 1;
            state.operation_kind = "connect".into();
            state.operation_done = true;
            state.operation_target = "another-row".into();
            state.operation_error = "Other device failed".into();
            state.error = "Other device failed".into();
        }
        render(&window, &binding, None);
        assert_eq!(
            global.get_network_form_state(),
            1,
            "Unrelated failures must not terminate this form"
        );
        binding.snapshot.borrow_mut().operation_target = "requested-row".into();
        render(&window, &binding, None);
        assert_eq!(global.get_network_form_state(), 3);
        binding.form_started.set(Some(1));
        {
            let mut state = binding.snapshot.borrow_mut();
            state.operation_id = 2;
            state.operation_error.clear();
            state.error.clear();
            state.networks.push(rix_services::network::Network {
                path: "requested-row".into(),
                active: true,
                ..Default::default()
            });
        }
        render(&window, &binding, None);
        assert_eq!(global.get_network_form_state(), 2);
    }
    #[test]
    fn a_full_command_queue_reports_failure_and_retains_credential_prompt() {
        i_slint_backend_testing::init_no_event_loop();
        let window = SystemHost::new().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let config = Arc::new(rix_config::ConfigManager::init_in(temp.path().to_owned()).unwrap());
        let (commands, _receiver) = tokio::sync::mpsc::channel(1);
        assert!(commands.try_send(Action::Refresh).is_ok());
        let binding = synthetic_binding(&window, config, Snapshot::default(), commands);
        let global = window.global::<SystemState>();
        global.set_network_credential_id("fixture-id".into());
        assert!(!send(&window, &binding, Action::Refresh));
        assert!(!global.get_network_error().is_empty());
        assert_eq!(global.get_network_form_state(), 3);
        assert_eq!(global.get_network_credential_id(), "fixture-id");
    }
}
