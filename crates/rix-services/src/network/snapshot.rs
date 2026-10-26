use super::{
    model::{Activation, Device, Network, Snapshot},
    profiles, properties, NM, ROOT,
};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use zbus::{
    zvariant::{OwnedObjectPath, OwnedValue},
    Connection,
};
type Map = HashMap<String, OwnedValue>;
pub(super) fn text<'a>(map: &'a Map, key: &str) -> Option<&'a str> {
    map.get(key).and_then(|v| <&str>::try_from(v).ok())
}
pub(super) fn num(map: &Map, key: &str) -> u32 {
    map.get(key)
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(0)
}
fn boolean(map: &Map, key: &str) -> bool {
    map.get(key)
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false)
}
fn path(map: &Map, key: &str) -> String {
    map.get(key)
        .and_then(|v| <&zbus::zvariant::ObjectPath>::try_from(v).ok())
        .map(|v| v.as_str().to_string())
        .unwrap_or_else(|| "/".into())
}
fn vec_value<T: TryFrom<OwnedValue>>(map: &Map, key: &str) -> Option<T> {
    T::try_from(map.get(key)?.try_clone().ok()?).ok()
}
pub(super) fn reason_message(reason: u32) -> String {
    match reason {
        5 => "The network did not provide an IP address",
        6 => "The network IP configuration expired",
        7 => "The network password or credentials are missing",
        8..=11 => "Wi-Fi authentication failed. Check the password and security settings",
        15..=17 => "The network did not provide an IP address (DHCP)",
        20 => "The network configuration is unavailable",
        29 => "The mobile network APN was rejected",
        31 => "The mobile network registration was denied",
        34 | 59 => "The SIM PIN was rejected",
        35 => "The network device firmware is missing",
        36 => "The network device was removed",
        38 => "The saved network profile was removed",
        39 => "The network connection was cancelled",
        45 => "Insert a SIM card to connect",
        46 => "Enter the SIM PIN to connect",
        47 => "The SIM requires a PUK code",
        52 => "ModemManager is not available",
        53 => "The Wi-Fi network was not found",
        64 => "A network address is already in use",
        65 => "This IP configuration method is not supported",
        69..=78 => "The network device is not managed by NetworkManager",
        _ => "The network connection failed. Open the network settings to check its configuration",
    }
    .into()
}
async fn ip_addresses(connection: &Connection, ip_path: &str, family: &str) -> String {
    if ip_path == "/" {
        return String::new();
    }
    let Ok(values) = properties(
        connection,
        ip_path,
        &format!("org.freedesktop.NetworkManager.{family}Config"),
    )
    .await
    else {
        return String::new();
    };
    let addresses: Vec<Map> = vec_value(&values, "AddressData").unwrap_or_default();
    addresses
        .iter()
        .filter_map(|v| text(v, "address"))
        .collect::<Vec<_>>()
        .join(", ")
}
pub(super) async fn load(connection: &Connection) -> Result<Snapshot> {
    let manager = properties(connection, ROOT, NM).await?;
    let paths: Vec<OwnedObjectPath> = if let Some(v) = vec_value(&manager, "Devices") {
        v
    } else {
        super::proxy(connection, ROOT, NM)
            .await?
            .call("GetDevices", &())
            .await?
    };
    let (profiles, profile_error) = match profiles::list(connection).await {
        Ok(p) => (p, String::new()),
        Err(_) => (
            Vec::new(),
            "Saved network profiles could not be read".into(),
        ),
    };
    let mut snapshot = Snapshot {
        wifi: boolean(&manager, "WirelessEnabled"),
        hardware: boolean(&manager, "WirelessHardwareEnabled"),
        connectivity: num(&manager, "Connectivity"),
        networking: boolean(&manager, "NetworkingEnabled"),
        portal_uri: text(&manager, "ConnectivityCheckUri")
            .unwrap_or_default()
            .into(),
        available: true,
        error: profile_error,
        profiles: profiles.iter().map(|v| v.draft.clone()).collect(),
        ..Default::default()
    };
    snapshot.permissions = super::proxy(connection, ROOT, NM)
        .await?
        .call::<_, _, HashMap<String, String>>("GetPermissions", &())
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();
    snapshot.permissions.sort();
    let mut active_profiles = HashMap::<String, Map>::new();
    for active in
        vec_value::<Vec<OwnedObjectPath>>(&manager, "ActiveConnections").unwrap_or_default()
    {
        if let Ok(values) = properties(
            connection,
            active.as_str(),
            "org.freedesktop.NetworkManager.Connection.Active",
        )
        .await
        {
            let reason = values
                .get("StateReason")
                .and_then(|v| v.try_clone().ok())
                .and_then(|v| <(u32, u32)>::try_from(v).ok())
                .map(|v| v.1)
                .unwrap_or(0);
            snapshot.activations.push(Activation {
                path: active.to_string(),
                profile: path(&values, "Connection"),
                device: vec_value::<Vec<OwnedObjectPath>>(&values, "Devices")
                    .and_then(|v| v.first().cloned())
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
                state: num(&values, "State"),
                reason,
            });
            if boolean(&values, "Vpn") && num(&values, "State") == 2 {
                snapshot.vpn_active = true;
            }
            if active.as_str() == path(&manager, "PrimaryConnection") {
                snapshot.primary_device = vec_value::<Vec<OwnedObjectPath>>(&values, "Devices")
                    .and_then(|v| v.first().cloned())
                    .map(|v| v.to_string())
                    .unwrap_or_default();
            }
            active_profiles.insert(path(&values, "Connection"), values);
        }
    }
    let mut available_by_device = HashMap::<String, Vec<OwnedObjectPath>>::new();
    let mut addresses_by_device = HashMap::<String, (String, String)>::new();
    let mut represented = HashSet::new();
    for device_path in paths {
        let Ok(values) = properties(
            connection,
            device_path.as_str(),
            "org.freedesktop.NetworkManager.Device",
        )
        .await
        else {
            continue;
        };
        let kind = num(&values, "DeviceType");
        let state = num(&values, "State");
        let reason = values
            .get("StateReason")
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| <(u32, u32)>::try_from(v).ok())
            .map(|v| v.1)
            .unwrap_or(0);
        let iface = text(&values, "Interface").unwrap_or_default().to_string();
        let managed = boolean(&values, "Managed");
        let device_error = if boolean(&values, "FirmwareMissing") {
            "The network device firmware is missing".into()
        } else if boolean(&values, "NmPluginMissing") {
            "A NetworkManager device plugin is missing".into()
        } else if !managed {
            "The network device is not managed by NetworkManager".into()
        } else if state == 120 {
            reason_message(reason)
        } else {
            String::new()
        };
        let available: Vec<OwnedObjectPath> =
            vec_value(&values, "AvailableConnections").unwrap_or_default();
        available_by_device.insert(device_path.to_string(), available.clone());
        let address = ip_addresses(connection, &path(&values, "Ip4Config"), "IP4").await;
        let ipv6 = ip_addresses(connection, &path(&values, "Ip6Config"), "IP6").await;
        addresses_by_device.insert(device_path.to_string(), (address.clone(), ipv6.clone()));
        let mut device = Device {
            path: device_path.to_string(),
            iface: iface.clone(),
            kind,
            state,
            reason,
            managed,
            hardware: kind != 2 || snapshot.hardware,
            available: managed && state >= 30,
            error: device_error.clone(),
            virtual_device: super::model::virtual_interface(kind, &iface, text(&values, "Udi")),
            ..Default::default()
        };
        if kind == 1 {
            let device_active = snapshot
                .activations
                .iter()
                .find(|a| a.device == device_path.as_str() && matches!(a.state, 1 | 2))
                .map(|a| a.profile.as_str());
            let active_profile = profiles
                .iter()
                .find(|p| device_active == Some(p.path.as_str()));
            let saved = active_profile.or_else(|| {
                profiles
                    .iter()
                    .find(|p| available.iter().any(|a| a.as_str() == p.path))
            });
            if let Some(saved) = saved {
                represented.insert(saved.path.clone());
            }
            snapshot.networks.push(Network {
                path: format!("device:{device_path}"),
                device: device_path.to_string(),
                name: iface.clone(),
                iface,
                state,
                state_reason: reason,
                active: state == 100,
                saved: saved.map(|v| v.path.clone()).unwrap_or_default(),
                profile_path: saved.map(|v| v.path.clone()).unwrap_or_default(),
                uuid: saved.map(|v| v.draft.uuid.clone()).unwrap_or_default(),
                address,
                ipv6,
                wired: true,
                device_kind: kind,
                kind: "802-3-ethernet".into(),
                available: device.available,
                error: device_error,
                metered: num(&values, "Metered"),
                ..Default::default()
            });
        } else if kind == 2 {
            let Ok(wifi) = properties(
                connection,
                device_path.as_str(),
                "org.freedesktop.NetworkManager.Device.Wireless",
            )
            .await
            else {
                snapshot.devices.push(device);
                continue;
            };
            device.last_scan = wifi
                .get("LastScan")
                .and_then(|v| i64::try_from(v).ok())
                .unwrap_or(-1);
            let active_ap = path(&wifi, "ActiveAccessPoint");
            let aps: Vec<OwnedObjectPath> = vec_value(&wifi, "AccessPoints").unwrap_or_default();
            let mut networks = HashMap::<String, Network>::new();
            for ap in aps {
                let Ok(ap_values) = properties(
                    connection,
                    ap.as_str(),
                    "org.freedesktop.NetworkManager.AccessPoint",
                )
                .await
                else {
                    continue;
                };
                let ssid: Vec<u8> = vec_value(&ap_values, "Ssid").unwrap_or_default();
                if ssid.is_empty() {
                    continue;
                }

                let protection = match (
                    ap_values.get("Flags").and_then(|v| u32::try_from(v).ok()),
                    ap_values
                        .get("WpaFlags")
                        .and_then(|v| u32::try_from(v).ok()),
                    ap_values
                        .get("RsnFlags")
                        .and_then(|v| u32::try_from(v).ok()),
                ) {
                    (Some(flags), Some(wpa), Some(rsn)) => profiles::security(flags, wpa, rsn),
                    _ => "unknown".into(),
                };
                let bssid = text(&ap_values, "HwAddress")
                    .unwrap_or_default()
                    .to_string();
                let frequency = num(&ap_values, "Frequency");
                let selected = active_ap == ap.as_str();
                let strength = ap_values
                    .get("Strength")
                    .and_then(|v| u8::try_from(v).ok())
                    .unwrap_or(0);
                let saved = profiles
                    .iter()
                    .filter(|p| {
                        profiles::compatible(p, &ssid, &protection, &available, &bssid, frequency)
                    })
                    .min_by_key(|p| {
                        !snapshot.activations.iter().any(|a| {
                            a.device == device_path.as_str()
                                && a.profile == p.path
                                && matches!(a.state, 1 | 2)
                        })
                    });
                if let Some(saved) = saved {
                    represented.insert(saved.path.clone());
                }
                let key = profiles::wifi_key(device_path.as_str(), &ssid, &protection);
                let network = Network {
                    path: key.clone(),
                    device: device_path.to_string(),
                    name: String::from_utf8_lossy(&ssid).into(),
                    ssid,
                    strength,
                    security: protection,
                    state: if selected { state } else { 30 },
                    state_reason: if selected { reason } else { 0 },
                    active: selected && state == 100,
                    saved: saved.map(|v| v.path.clone()).unwrap_or_default(),
                    profile_path: saved.map(|v| v.path.clone()).unwrap_or_default(),
                    uuid: saved.map(|v| v.draft.uuid.clone()).unwrap_or_default(),
                    address: if selected {
                        address.clone()
                    } else {
                        String::new()
                    },
                    ipv6: if selected {
                        ipv6.clone()
                    } else {
                        String::new()
                    },
                    ap_path: ap.to_string(),
                    iface: iface.clone(),
                    available: device.available && snapshot.wifi,
                    device_kind: kind,
                    kind: "802-11-wireless".into(),
                    frequency,
                    bssid,
                    error: if selected {
                        device_error.clone()
                    } else {
                        String::new()
                    },
                    metered: num(&values, "Metered"),
                    ..Default::default()
                };
                let replace = networks.get(&key).is_none_or(|previous| {
                    selected || (previous.ap_path != active_ap && strength > previous.strength)
                });
                if replace {
                    networks.insert(key, network);
                }
            }
            snapshot.networks.extend(networks.into_values());
        }
        snapshot.devices.push(device);
    }

    for profile in &profiles {
        if represented.contains(&profile.path) {
            continue;
        }
        let active = active_profiles.get(&profile.path);
        let active_state = active.map(|v| num(v, "State")).unwrap_or(0);
        let active_devices = active
            .and_then(|v| vec_value::<Vec<OwnedObjectPath>>(v, "Devices"))
            .unwrap_or_default();
        let selected_device = active_devices
            .first()
            .map(|p| p.to_string())
            .or_else(|| {
                available_by_device
                    .iter()
                    .filter(|(_, v)| v.iter().any(|a| a.as_str() == profile.path))
                    .map(|(k, _)| k.clone())
                    .min()
            })
            .unwrap_or_default();
        let device = snapshot.devices.iter().find(|v| v.path == selected_device);
        let (address, ipv6) = addresses_by_device
            .get(&selected_device)
            .cloned()
            .unwrap_or_default();
        snapshot.networks.push(Network {
            path: format!(
                "profile:{}",
                if profile.draft.uuid.is_empty() {
                    &profile.path
                } else {
                    &profile.draft.uuid
                }
            ),
            device: selected_device,
            name: profile.draft.name.clone(),
            ssid: profiles::ssid(&profile.settings).unwrap_or_default(),
            security: profile.draft.security.clone(),
            active: active_state == 2,
            state: if active_state == 2 {
                100
            } else if active_state == 1 {
                40
            } else {
                30
            },
            saved: profile.path.clone(),
            profile_path: profile.path.clone(),
            uuid: profile.draft.uuid.clone(),
            available: device.is_some_and(|v| v.available)
                || matches!(
                    profile.draft.kind.as_str(),
                    "vpn" | "wireguard" | "bridge" | "bond" | "vlan"
                ),
            address,
            ipv6,
            wired: profile.draft.kind == "802-3-ethernet",
            device_kind: device.map(|v| v.kind).unwrap_or(0),
            kind: profile.draft.kind.clone(),
            iface: device.map(|v| v.iface.clone()).unwrap_or_default(),
            ..Default::default()
        });
    }
    snapshot.networks.sort_by(|a, b| {
        b.active
            .cmp(&a.active)
            .then(b.wired.cmp(&a.wired))
            .then(a.name.cmp(&b.name))
            .then(a.path.cmp(&b.path))
    });
    snapshot.devices.sort_by(|a, b| a.iface.cmp(&b.iface));
    for network in &mut snapshot.networks {
        network.default_route = network.active
            && (network.device == snapshot.primary_device
                || active_profiles
                    .get(&network.profile_path)
                    .is_some_and(|active| {
                        boolean(active, "Default") || boolean(active, "Default6")
                    }));
    }
    Ok(snapshot)
}
