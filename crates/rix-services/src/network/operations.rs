use super::{
    profiles, properties, proxy, Action, Network, ProfileDraft, Settings, Snapshot, NM, ROOT,
};
use anyhow::{bail, Context, Result};
use std::{collections::HashMap, path::Path, time::Duration};
use zbus::{
    zvariant::{OwnedObjectPath, OwnedValue, Value},
    Connection,
};
#[derive(Default)]
pub(super) struct Applied {
    pub active: String,
    pub profile: String,
    pub device: String,
    pub scans: Vec<(String, i64)>,
}
fn object(value: &str) -> Result<OwnedObjectPath> {
    Ok(OwnedObjectPath::try_from(if value.is_empty() {
        "/"
    } else {
        value
    })?)
}
fn lookup<'a>(state: &'a Snapshot, key: &str) -> Result<&'a Network> {
    state
        .networks
        .iter()
        .find(|n| n.path == key || n.profile_path == key)
        .context("This network is no longer available. Refresh the list")
}
async fn get_settings(connection: &Connection, path: &str) -> Result<Settings> {
    Ok(proxy(
        connection,
        path,
        "org.freedesktop.NetworkManager.Settings.Connection",
    )
    .await?
    .call("GetSettings", &())
    .await?)
}
async fn preserve_secrets(connection: &Connection, path: &str, settings: &mut Settings) {
    let Ok(saved) = proxy(
        connection,
        path,
        "org.freedesktop.NetworkManager.Settings.Connection",
    )
    .await
    else {
        return;
    };
    for group in [
        "802-11-wireless-security",
        "802-1x",
        "vpn",
        "gsm",
        "cdma",
        "wireguard",
    ] {
        if !settings.contains_key(group) {
            continue;
        }
        if let Ok(secrets) = saved.call::<_, _, Settings>("GetSecrets", &(group,)).await {
            for (key, values) in secrets {
                settings.entry(key).or_default().extend(values);
            }
        }
    }
}
async fn activate(
    connection: &Connection,
    profile: &str,
    device: &str,
    specific: &str,
) -> Result<Applied> {
    let active: OwnedObjectPath = proxy(connection, ROOT, NM)
        .await?
        .call(
            "ActivateConnection",
            &(object(profile)?, object(device)?, object(specific)?),
        )
        .await?;
    Ok(Applied {
        active: active.to_string(),
        profile: profile.into(),
        device: device.into(),
        ..Default::default()
    })
}
async fn add_activate(
    connection: &Connection,
    settings: Settings,
    device: &str,
    specific: &str,
    remember: bool,
) -> Result<Applied> {
    let options: HashMap<String, OwnedValue> = HashMap::from([(
        "persist".into(),
        profiles::string(if remember { "disk" } else { "volatile" }),
    )]);
    let (profile, active, _): (
        OwnedObjectPath,
        OwnedObjectPath,
        HashMap<String, OwnedValue>,
    ) = proxy(connection, ROOT, NM)
        .await?
        .call(
            "AddAndActivateConnection2",
            &(settings, object(device)?, object(specific)?, options),
        )
        .await?;
    Ok(Applied {
        active: active.to_string(),
        profile: profile.to_string(),
        device: device.into(),
        ..Default::default()
    })
}
pub(super) fn editor_kind(kind: &str) -> bool {
    matches!(
        kind,
        "save" | "connect-profile" | "hidden" | "hotspot" | "import"
    )
}
async fn deactivate_profile(connection: &Connection, profile: &str) -> Result<()> {
    let manager = proxy(connection, ROOT, NM).await?;
    for active in manager
        .get_property::<Vec<OwnedObjectPath>>("ActiveConnections")
        .await?
    {
        let Ok(values) = properties(
            connection,
            active.as_str(),
            "org.freedesktop.NetworkManager.Connection.Active",
        )
        .await
        else {
            continue;
        };
        let current = values
            .get("Connection")
            .and_then(|v| <&zbus::zvariant::ObjectPath>::try_from(v).ok())
            .map(|v| v.as_str());
        if current == Some(profile) {
            manager
                .call::<_, _, ()>("DeactivateConnection", &(active,))
                .await?;
        }
    }
    Ok(())
}
pub(super) fn classify(action: &Action) -> (&'static str, String) {
    match action {
        Action::Connect { path, .. } => ("connect", path.clone()),
        Action::ActivateProfile { path, .. } => ("activate", path.clone()),
        Action::Disconnect(path) => ("disconnect", path.clone()),
        Action::Cancel(path) => ("cancel", path.clone()),
        Action::CancelActive { .. } => ("cancel", "profile-editor".into()),
        Action::Forget(path) => ("forget", path.clone()),
        Action::SaveProfile(d) => (
            "save",
            if d.path.is_empty() {
                "profile-editor".into()
            } else {
                d.path.clone()
            },
        ),
        Action::ConnectProfile { draft: d } => (
            "connect-profile",
            if d.path.is_empty() {
                "profile-editor".into()
            } else {
                d.path.clone()
            },
        ),
        Action::Hidden(d) => (
            "hidden",
            if d.path.is_empty() {
                "profile-editor".into()
            } else {
                d.path.clone()
            },
        ),
        Action::Hotspot(d) => (
            "hotspot",
            if d.path.is_empty() {
                "profile-editor".into()
            } else {
                d.path.clone()
            },
        ),
        Action::ImportProfile { .. } => ("import", "profile-editor".into()),
        Action::Refresh => ("scan", String::new()),
        Action::Wifi(_) => ("wifi", String::new()),
        Action::Networking(_) => ("networking", String::new()),
        Action::CheckConnectivity => ("check", String::new()),
        _ => ("", String::new()),
    }
}

const IMPORT_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) async fn run_import_command(
    program: &Path,
    kind: &str,
    path: &str,
    timeout: Duration,
) -> Result<()> {
    let mut command = tokio::process::Command::new(program);
    command
        .args([
            "--wait",
            "20",
            "connection",
            "import",
            "type",
            kind,
            "file",
            path,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .context("Install nmcli and the appropriate VPN plugin to import this file")?;
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => {
            let status = status
                .context("Install nmcli and the appropriate VPN plugin to import this file")?;
            if !status.success() {
                bail!("The VPN file could not be imported. Check its format and the installed NetworkManager plugin");
            }
            Ok(())
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            bail!("The VPN import timed out. Check that NetworkManager and its VPN plugin are responding");
        }
    }
}

pub(super) async fn apply(
    connection: &Connection,
    action: Action,
    state: &Snapshot,
) -> Result<Applied> {
    let manager = proxy(connection, ROOT, NM).await?;
    match action {
        Action::Wifi(enabled) => {
            manager.set_property("WirelessEnabled", enabled).await?;
        }
        Action::Networking(enabled) => {
            manager.call::<_, _, ()>("Enable", &(enabled,)).await?;
        }
        Action::Refresh => {
            if !state.wifi {
                bail!("Turn on Wi-Fi before scanning");
            }
            let mut result = Applied::default();
            let mut first_error = None;
            for device in state
                .devices
                .iter()
                .filter(|d| d.kind == 2 && d.managed && d.hardware)
            {
                let wifi = proxy(
                    connection,
                    &device.path,
                    "org.freedesktop.NetworkManager.Device.Wireless",
                )
                .await?;
                let last = wifi.get_property::<i64>("LastScan").await.unwrap_or(-1);
                match wifi
                    .call::<_, _, ()>("RequestScan", &(HashMap::<String, OwnedValue>::new(),))
                    .await
                {
                    Ok(()) => result.scans.push((device.path.clone(), last)),
                    Err(error) => {
                        if first_error.is_none() {
                            first_error = Some(error)
                        }
                    }
                }
            }
            if result.scans.is_empty() {
                if let Some(error) = first_error {
                    let message = error.to_string();
                    if message.contains("ScanningNotAllowed") || message.contains("scan already") {
                        return Ok(result);
                    }
                    return Err(error.into());
                }
                bail!("No available Wi-Fi adapter");
            }
            return Ok(result);
        }
        Action::CheckConnectivity => {
            let _: u32 = manager.call("CheckConnectivity", &()).await?;
        }
        Action::Forget(key) => {
            let path = state
                .profiles
                .iter()
                .find(|p| p.path == key)
                .map(|p| p.path.clone())
                .or_else(|| lookup(state, &key).ok().map(|n| n.profile_path.clone()))
                .filter(|p| !p.is_empty())
                .context("No saved network profile")?;
            deactivate_profile(connection, &path).await?;
            proxy(
                connection,
                &path,
                "org.freedesktop.NetworkManager.Settings.Connection",
            )
            .await?
            .call::<_, _, ()>("Delete", &())
            .await?;
        }
        Action::Disconnect(key) | Action::Cancel(key) => {
            if key == "profile-editor" {
                return Ok(Applied::default());
            }
            let network = lookup(state, &key)?;
            let profile = state
                .profiles
                .iter()
                .find(|p| p.path == network.profile_path);
            if profile.is_some_and(|p| {
                matches!(
                    p.kind.as_str(),
                    "vpn" | "wireguard" | "bridge" | "bond" | "vlan"
                )
            }) || network.device.is_empty()
            {
                deactivate_profile(connection, &network.profile_path).await?;
            } else {
                proxy(
                    connection,
                    &network.device,
                    "org.freedesktop.NetworkManager.Device",
                )
                .await?
                .call::<_, _, ()>("Disconnect", &())
                .await?;
            }
        }
        Action::CancelActive { active, devices } => {
            for path in active {
                manager
                    .call::<_, _, ()>("DeactivateConnection", &(object(&path)?,))
                    .await?;
            }
            for path in devices {
                proxy(connection, &path, "org.freedesktop.NetworkManager.Device")
                    .await?
                    .call::<_, _, ()>("Disconnect", &())
                    .await?;
            }
        }
        Action::Connect {
            path,
            password,
            remember,
        } => {
            let network = lookup(state, &path)?;
            if !network.available && network.profile_path.is_empty() {
                bail!("This network device is unavailable. Check Wi-Fi, radio blocking and device settings");
            }
            if network.security == "unknown" {
                bail!("Network security is not available yet. Refresh the list");
            }
            if !network.saved.is_empty() && password.is_empty() && remember {
                return activate(
                    connection,
                    &network.saved,
                    &network.device,
                    &network.ap_path,
                )
                .await;
            }
            let mut settings = if network.saved.is_empty() {
                Settings::new()
            } else {
                let mut settings = get_settings(connection, &network.saved).await?;
                preserve_secrets(connection, &network.saved, &mut settings).await;
                settings
            };
            if !remember && !network.saved.is_empty() {
                settings
                    .entry("connection".into())
                    .or_default()
                    .insert("uuid".into(), profiles::string(&profiles::new_uuid()?));
            }
            if network.saved.is_empty() {
                let security = match network.security.as_str() {
                    "wpa2-wpa3" => "wpa-psk",
                    "owe-transition" => "open",
                    s => s,
                };
                let draft = ProfileDraft {
                    name: network.name.clone(),

                    ssid: std::str::from_utf8(&network.ssid)
                        .map(str::to_owned)
                        .unwrap_or_else(|_| "?".repeat(network.ssid.len())),
                    kind: if network.wired {
                        "802-3-ethernet"
                    } else {
                        "802-11-wireless"
                    }
                    .into(),
                    security: security.into(),
                    password,
                    remember,
                    ..Default::default()
                };
                settings = profiles::build(&draft, settings, &profiles::user_name()?)?;
                if !network.wired {
                    settings
                        .entry("802-11-wireless".into())
                        .or_default()
                        .insert(
                            "ssid".into(),
                            Value::from(network.ssid.clone()).try_to_owned()?,
                        );
                }
            } else if !password.is_empty() {

                let key = profiles::str_setting(&settings, "802-11-wireless-security", "key-mgmt")
                    .unwrap_or("open")
                    .to_string();
                let (sec, field) = match key.as_str() {
                    "wpa-psk" | "sae" => {
                        if !profiles::valid_psk(&password, key == "sae") {
                            bail!("WPA2 passwords must contain 8 to 63 ASCII characters or 64 hexadecimal digits");
                        }
                        ("802-11-wireless-security", "psk")
                    }
                    "none" => ("802-11-wireless-security", "wep-key0"),
                    "wpa-eap" | "wpa-eap-suite-b-192" | "ieee8021x" => ("802-1x", "password"),
                    _ => bail!("Edit this profile to change its credentials"),
                };
                settings
                    .entry(sec.into())
                    .or_default()
                    .insert(field.into(), profiles::string(&password));
                settings.entry(sec.into()).or_default().insert(
                    if field.starts_with("wep-key") {
                        "wep-key-flags".into()
                    } else {
                        format!("{field}-flags")
                    },
                    OwnedValue::from(0u32),
                );
            }
            if network.saved.is_empty() || !remember {
                return add_activate(
                    connection,
                    settings,
                    &network.device,
                    &network.ap_path,
                    remember,
                )
                .await;
            }
            let _: HashMap<String, OwnedValue> = proxy(
                connection,
                &network.saved,
                "org.freedesktop.NetworkManager.Settings.Connection",
            )
            .await?
            .call(
                "Update2",
                &(settings, 1u32, HashMap::<String, OwnedValue>::new()),
            )
            .await?;
            return activate(
                connection,
                &network.saved,
                &network.device,
                &network.ap_path,
            )
            .await;
        }
        Action::ActivateProfile { path, device } => {
            let profile = state
                .profiles
                .iter()
                .find(|p| p.path == path || p.uuid == path)
                .context("The saved profile is no longer available")?;
            return activate(connection, &profile.path, &device, "/").await;
        }
        Action::SaveProfile(mut draft) => {
            let hotspot = draft.kind == "hotspot";
            if hotspot {
                prepare_hotspot(&mut draft);
            }
            return save_draft(connection, draft, state, false, hotspot).await;
        }
        Action::ConnectProfile { draft } => {
            return save_draft(connection, draft, state, true, false).await
        }
        Action::Hidden(mut draft) => {
            draft.hidden = true;
            return save_draft(connection, draft, state, true, false).await;
        }
        Action::Hotspot(mut draft) => {
            prepare_hotspot(&mut draft);
            return save_draft(connection, draft, state, true, true).await;
        }
        Action::ImportProfile { path, kind } => {
            if !matches!(
                kind.as_str(),
                "keyfile"
                    | "wireguard"
                    | "openvpn"
                    | "vpnc"
                    | "openconnect"
                    | "strongswan"
                    | "pptp"
                    | "l2tp"
            ) {
                bail!("Select a supported VPN import type");
            }
            let metadata = tokio::fs::metadata(&path)
                .await
                .context("The import file could not be read")?;
            if !metadata.is_file() || metadata.len() > 1024 * 1024 {
                bail!("The import file must be a regular file no larger than 1 MiB");
            }
            if kind == "keyfile" {
                let settings = tokio::task::spawn_blocking(move || super::keyfile::read(&path))
                    .await
                    .context("The network import could not be completed")??;
                let flags = 1u32 | 0x20u32;
                let (profile, _): (OwnedObjectPath, HashMap<String, OwnedValue>) = proxy(
                    connection,
                    "/org/freedesktop/NetworkManager/Settings",
                    "org.freedesktop.NetworkManager.Settings",
                )
                .await?
                .call(
                    "AddConnection2",
                    &(settings, flags, HashMap::<String, OwnedValue>::new()),
                )
                .await?;
                return Ok(Applied {
                    profile: profile.to_string(),
                    ..Default::default()
                });
            }
            run_import_command(Path::new("nmcli"), &kind, &path, IMPORT_TIMEOUT).await?;
        }
        Action::ResetError | Action::ReplySecrets { .. } => {}
    }
    Ok(Applied::default())
}
fn prepare_hotspot(draft: &mut ProfileDraft) {
    draft.kind = "802-11-wireless".into();
    draft.ipv4_method = "shared".into();
    draft.ipv6_method = "disabled".into();
    draft.autoconnect = false;
}
async fn save_draft(
    connection: &Connection,
    mut draft: ProfileDraft,
    state: &Snapshot,
    activate_now: bool,
    hotspot: bool,
) -> Result<Applied> {
    let mut old = if draft.path.is_empty() {
        Settings::new()
    } else {
        let mut old = get_settings(connection, &draft.path).await?;
        preserve_secrets(connection, &draft.path, &mut old).await;
        old
    };
    let clone_temporary = !draft.remember && !draft.path.is_empty();
    if clone_temporary {
        draft.uuid = profiles::new_uuid()?;
        old.entry("connection".into())
            .or_default()
            .insert("uuid".into(), profiles::string(&draft.uuid));
    }
    let mut device = if draft.device.starts_with('/') {
        draft.device.clone()
    } else {
        state
            .devices
            .iter()
            .find(|d| d.iface == draft.device && d.managed)
            .or_else(|| {
                if draft.device.is_empty() {
                    state.devices.iter().find(|d| {
                        d.managed
                            && d.available
                            && ((draft.kind == "802-11-wireless" && d.kind == 2)
                                || (draft.kind == "802-3-ethernet" && d.kind == 1)
                                || (matches!(draft.kind.as_str(), "gsm" | "cdma") && d.kind == 8))
                    })
                } else {
                    None
                }
            })
            .map(|d| d.path.clone())
            .unwrap_or_default()
    };
    if hotspot && activate_now && draft.device.is_empty() {

        device.clear();
        for adapter in state
            .devices
            .iter()
            .filter(|d| d.kind == 2 && d.managed && d.hardware && d.available)
        {
            let Ok(caps) = properties(
                connection,
                &adapter.path,
                "org.freedesktop.NetworkManager.Device.Wireless",
            )
            .await
            else {
                continue;
            };
            if super::snapshot::num(&caps, "WirelessCapabilities") & 0x40 != 0 {
                device = adapter.path.clone();
                break;
            }
        }
        if device.is_empty() {
            bail!("This adapter does not support creating a Wi-Fi hotspot");
        }
    }
    if draft.device.starts_with('/') {
        if let Some(adapter) = state.devices.iter().find(|d| d.path == draft.device) {
            draft.device = adapter.iface.clone();
        } else {
            bail!("The selected network adapter is no longer available");
        }
    }
    let mut settings = profiles::build(&draft, old, &profiles::user_name()?)?;
    if hotspot && activate_now {
        let caps = properties(
            connection,
            &device,
            "org.freedesktop.NetworkManager.Device.Wireless",
        )
        .await?;
        if super::snapshot::num(&caps, "WirelessCapabilities") & 0x40 == 0 {
            bail!("This adapter does not support creating a Wi-Fi hotspot");
        }
    }
    if hotspot {
        settings
            .entry("802-11-wireless".into())
            .or_default()
            .insert("mode".into(), profiles::string("ap"));
    }
    if activate_now && (draft.path.is_empty() || clone_temporary) {
        return add_activate(connection, settings, &device, "/", draft.remember).await;
    }
    let path = if draft.path.is_empty() || clone_temporary {
        let flags = if draft.remember { 1u32 } else { 2u32 };
        let (path, _): (OwnedObjectPath, HashMap<String, OwnedValue>) = proxy(
            connection,
            "/org/freedesktop/NetworkManager/Settings",
            "org.freedesktop.NetworkManager.Settings",
        )
        .await?
        .call(
            "AddConnection2",
            &(settings, flags, HashMap::<String, OwnedValue>::new()),
        )
        .await?;
        path.to_string()
    } else {
        let _: HashMap<String, OwnedValue> = proxy(
            connection,
            &draft.path,
            "org.freedesktop.NetworkManager.Settings.Connection",
        )
        .await?
        .call(
            "Update2",
            &(settings, 1u32, HashMap::<String, OwnedValue>::new()),
        )
        .await?;
        draft.path.clone()
    };
    if activate_now {
        activate(connection, &path, &device, "/").await
    } else {
        Ok(Applied {
            profile: path,
            device,
            ..Default::default()
        })
    }
}
