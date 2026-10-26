use super::model::ProfileDraft;
use super::{proxy, Settings};
use anyhow::{bail, Context, Result};
use std::{collections::HashMap, net::IpAddr};
use zbus::{
    zvariant::{OwnedObjectPath, OwnedValue, Value},
    Connection,
};

pub(super) struct Profile {
    pub path: String,
    pub settings: Settings,
    pub draft: ProfileDraft,
    pub priority: i32,
    pub timestamp: u64,
}
pub(super) fn string(value: &str) -> OwnedValue {
    Value::from(value)
        .try_to_owned()
        .expect("string D-Bus value")
}
pub(super) fn str_setting<'a>(settings: &'a Settings, group: &str, key: &str) -> Option<&'a str> {
    settings
        .get(group)?
        .get(key)
        .and_then(|v| <&str>::try_from(v).ok())
}
pub(super) fn value_vec<T>(settings: &Settings, group: &str, key: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    T::try_from(settings.get(group)?.get(key)?.try_clone().ok()?).ok()
}
pub(super) fn ssid(settings: &Settings) -> Option<Vec<u8>> {
    value_vec(settings, "802-11-wireless", "ssid")
}
fn dict_lines(settings: &Settings, group: &str, key: &str, address: bool) -> String {
    let values: Vec<HashMap<String, OwnedValue>> =
        value_vec(settings, group, key).unwrap_or_default();
    values
        .iter()
        .filter_map(|row| {
            let text = <&str>::try_from(row.get(if address { "address" } else { "dest" })?).ok()?;
            let prefix = u32::try_from(row.get("prefix")?).ok()?;
            let mut line = format!("{text}/{prefix}");
            if !address {
                if let Some(gateway) = row.get("next-hop").and_then(|v| <&str>::try_from(v).ok()) {
                    line.push(' ');
                    line.push_str(gateway);
                    if let Some(metric) = row.get("metric").and_then(|v| u32::try_from(v).ok()) {
                        line.push_str(&format!(" {metric}"));
                    }
                }
            }
            Some(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn certificate(settings: &Settings, key: &str) -> String {
    let bytes: Vec<u8> = value_vec(settings, "802-1x", key).unwrap_or_default();
    if !bytes.starts_with(b"file://") && !bytes.starts_with(b"pkcs11:") {
        return String::new();
    }
    String::from_utf8_lossy(&bytes)
        .trim_end_matches('\0')
        .trim_start_matches("file://")
        .into()
}
fn map_lines(settings: &Settings, group: &str, key: &str) -> String {
    let map: HashMap<String, String> = value_vec(settings, group, key).unwrap_or_default();
    let mut entries: Vec<_> = map.into_iter().map(|(k, v)| format!("{k}={v}")).collect();
    entries.sort();
    entries.join("\n")
}
pub(super) fn draft(path: &str, settings: &Settings) -> ProfileDraft {
    let text = |g, k| str_setting(settings, g, k).unwrap_or_default().to_string();
    let security = str_setting(settings, "802-11-wireless-security", "key-mgmt").unwrap_or("open");
    let eap: Vec<String> = value_vec(settings, "802-1x", "eap").unwrap_or_default();
    let permissions: Vec<String> =
        value_vec(settings, "connection", "permissions").unwrap_or_default();
    let mut result = ProfileDraft {
        path: path.into(),
        name: text("connection", "id"),
        uuid: text("connection", "uuid"),
        kind: text("connection", "type"),
        device: text("connection", "interface-name"),
        ssid: String::from_utf8_lossy(&ssid(settings).unwrap_or_default()).into(),
        security: if security == "none" {
            "wep".into()
        } else {
            security.into()
        },
        username: text("802-1x", "identity"),
        anonymous_identity: text("802-1x", "anonymous-identity"),
        eap: eap.first().cloned().unwrap_or_else(|| "peap".into()),
        phase2_auth: text("802-1x", "phase2-auth"),
        ca_cert: certificate(settings, "ca-cert"),
        client_cert: certificate(settings, "client-cert"),
        private_key: certificate(settings, "private-key"),
        autoconnect: settings
            .get("connection")
            .and_then(|m| m.get("autoconnect"))
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(true),
        shared: permissions.is_empty(),
        hidden: settings
            .get("802-11-wireless")
            .and_then(|m| m.get("hidden"))
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false),
        metered: settings
            .get("connection")
            .and_then(|m| m.get("metered"))
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0),
        powersave: settings
            .get("802-11-wireless")
            .and_then(|m| m.get("powersave"))
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0),
        mac_address: text("802-11-wireless", "assigned-mac-address"),
        band: text("802-11-wireless", "band"),
        bssid: value_vec::<Vec<u8>>(settings, "802-11-wireless", "bssid")
            .unwrap_or_default()
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":"),
        vpn_service: text("vpn", "service-type"),
        vpn_data: map_lines(settings, "vpn", "data"),
        apn: text("gsm", "apn"),
        number: text("gsm", "number"),
        proxy_method: if settings
            .get("proxy")
            .and_then(|m| m.get("method"))
            .and_then(|v| i32::try_from(v).ok())
            == Some(1)
        {
            "auto".into()
        } else {
            "none".into()
        },
        proxy_pac_url: text("proxy", "pac-url"),
        proxy_pac_script: text("proxy", "pac-script"),
        priority: settings
            .get("connection")
            .and_then(|m| m.get("autoconnect-priority"))
            .and_then(|v| i32::try_from(v).ok())
            .unwrap_or(0),
        mtu: settings
            .get(if text("connection", "type") == "802-3-ethernet" {
                "802-3-ethernet"
            } else {
                "802-11-wireless"
            })
            .and_then(|m| m.get("mtu"))
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0),
        domain_suffix_match: text("802-1x", "domain-suffix-match"),
        ..Default::default()
    };
    if result.kind == "gsm" {
        result.username = text("gsm", "username");
    }
    if result.kind == "vpn" {
        result.username = text("vpn", "user-name");
    }
    if result.kind == "802-3-ethernet" {
        result.mac_address = text("802-3-ethernet", "assigned-mac-address");
    }
    for family in ["ipv4", "ipv6"] {
        let method = str_setting(settings, family, "method")
            .unwrap_or("auto")
            .to_string();
        let addresses = dict_lines(settings, family, "address-data", true);
        let mut addresses = addresses;
        let mut gateway = text(family, "gateway");
        if addresses.is_empty() {
            if family == "ipv4" {
                let old: Vec<Vec<u32>> =
                    value_vec(settings, family, "addresses").unwrap_or_default();
                addresses = old
                    .iter()
                    .filter(|v| v.len() >= 2)
                    .map(|v| format!("{}/{}", std::net::Ipv4Addr::from(v[0].to_ne_bytes()), v[1]))
                    .collect::<Vec<_>>()
                    .join("\n");
                if gateway.is_empty() {
                    gateway = old
                        .first()
                        .and_then(|v| v.get(2))
                        .filter(|v| **v != 0)
                        .map(|v| std::net::Ipv4Addr::from(v.to_ne_bytes()).to_string())
                        .unwrap_or_default();
                }
            } else {
                let old: Vec<(Vec<u8>, u32, Vec<u8>)> =
                    value_vec(settings, family, "addresses").unwrap_or_default();
                addresses = old
                    .iter()
                    .filter_map(|(v, p, _)| {
                        <[u8; 16]>::try_from(v.as_slice())
                            .ok()
                            .map(|v| format!("{}/{p}", std::net::Ipv6Addr::from(v)))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if gateway.is_empty() {
                    gateway = old
                        .first()
                        .and_then(|(_, _, v)| <[u8; 16]>::try_from(v.as_slice()).ok())
                        .filter(|v| *v != [0; 16])
                        .map(|v| std::net::Ipv6Addr::from(v).to_string())
                        .unwrap_or_default();
                }
            }
        }
        let mut dns: Vec<String> = value_vec(settings, family, "dns-data").unwrap_or_default();

        if dns.is_empty() {
            if family == "ipv4" {
                let words: Vec<u32> = value_vec(settings, family, "dns").unwrap_or_default();
                dns = words
                    .into_iter()
                    .map(|v| std::net::Ipv4Addr::from(v.to_ne_bytes()).to_string())
                    .collect();
            } else {
                let bytes: Vec<Vec<u8>> = value_vec(settings, family, "dns").unwrap_or_default();
                dns = bytes
                    .into_iter()
                    .filter_map(|v| <[u8; 16]>::try_from(v).ok())
                    .map(|v| std::net::Ipv6Addr::from(v).to_string())
                    .collect();
            }
        }
        let mut routes = dict_lines(settings, family, "route-data", false);
        if routes.is_empty() {
            if family == "ipv4" {
                let old: Vec<Vec<u32>> = value_vec(settings, family, "routes").unwrap_or_default();
                routes = old
                    .iter()
                    .filter(|v| v.len() >= 4)
                    .map(|v| {
                        format!(
                            "{}/{} {} {}",
                            std::net::Ipv4Addr::from(v[0].to_ne_bytes()),
                            v[1],
                            std::net::Ipv4Addr::from(v[2].to_ne_bytes()),
                            v[3]
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
            } else {
                let old: Vec<(Vec<u8>, u32, Vec<u8>, u32)> =
                    value_vec(settings, family, "routes").unwrap_or_default();
                routes = old
                    .iter()
                    .filter_map(|(d, p, g, m)| {
                        Some(format!(
                            "{}/{p} {} {m}",
                            std::net::Ipv6Addr::from(<[u8; 16]>::try_from(d.as_slice()).ok()?),
                            std::net::Ipv6Addr::from(<[u8; 16]>::try_from(g.as_slice()).ok()?)
                        ))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
            }
        }
        if family == "ipv4" {
            result.ipv4_method = method;
            result.ipv4_addresses = addresses;
            result.ipv4_gateway = gateway;
            result.ipv4_dns = dns.join(", ");
            result.ipv4_routes = routes;
        } else {
            result.ipv6_method = method;
            result.ipv6_addresses = addresses;
            result.ipv6_gateway = gateway;
            result.ipv6_dns = dns.join(", ");
            result.ipv6_routes = routes;
        }
    }
    result
}
pub(super) async fn list(connection: &Connection) -> Result<Vec<Profile>> {
    let settings = proxy(
        connection,
        "/org/freedesktop/NetworkManager/Settings",
        "org.freedesktop.NetworkManager.Settings",
    )
    .await?;
    let paths: Vec<OwnedObjectPath> = settings.call("ListConnections", &()).await?;
    let mut profiles = Vec::new();
    for path in paths {
        let Ok(entry) = proxy(
            connection,
            path.as_str(),
            "org.freedesktop.NetworkManager.Settings.Connection",
        )
        .await
        else {
            continue;
        };
        let Ok(settings) = entry.call::<_, _, Settings>("GetSettings", &()).await else {
            continue;
        };
        let public = draft(path.as_str(), &settings);
        let priority = settings
            .get("connection")
            .and_then(|v| v.get("autoconnect-priority"))
            .and_then(|v| i32::try_from(v).ok())
            .unwrap_or(0);
        let timestamp = settings
            .get("connection")
            .and_then(|v| v.get("timestamp"))
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(0);
        profiles.push(Profile {
            path: path.to_string(),
            settings,
            draft: public,
            priority,
            timestamp,
        });
    }
    profiles.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then(b.timestamp.cmp(&a.timestamp))
            .then(a.draft.uuid.cmp(&b.draft.uuid))
    });
    Ok(profiles)
}
pub(super) fn security(flags: u32, wpa: u32, rsn: u32) -> String {
    let sec = wpa | rsn;
    if sec & 0x2000 != 0 {
        "wpa-eap-suite-b-192"
    } else if sec & 0x200 != 0 {
        "enterprise"
    } else if sec & 0x400 != 0 && sec & 0x100 != 0 {
        "wpa2-wpa3"
    } else if sec & 0x400 != 0 {
        "sae"
    } else if sec & 0x100 != 0 {
        "wpa-psk"
    } else if sec & 0x800 != 0 {
        "owe"
    } else if sec & 0x1000 != 0 {
        "owe-transition"
    } else if flags & 1 != 0 {
        "wep"
    } else {
        "open"
    }
    .into()
}
pub(super) fn compatible(
    profile: &Profile,
    ssid_bytes: &[u8],
    security: &str,
    device_available: &[OwnedObjectPath],
    bssid: &str,
    frequency: u32,
) -> bool {
    if !device_available.iter().any(|v| v.as_str() == profile.path)
        || ssid(&profile.settings).as_deref() != Some(ssid_bytes)
    {
        return false;
    }
    let key =
        str_setting(&profile.settings, "802-11-wireless-security", "key-mgmt").unwrap_or("open");
    let mode =
        str_setting(&profile.settings, "802-11-wireless", "mode").unwrap_or("infrastructure");
    if mode != "infrastructure" {
        return false;
    }
    let security_ok = match security {
        "open" | "owe-transition" => {
            key == "open" || (security == "owe-transition" && key == "owe")
        }
        "owe" => key == "owe",
        "wep" => matches!(key, "none" | "ieee8021x"),
        "enterprise" => key == "wpa-eap",
        "wpa-eap-suite-b-192" => key == "wpa-eap-suite-b-192",
        "sae" => matches!(key, "sae" | "wpa-psk"),
        "wpa2-wpa3" => matches!(key, "sae" | "wpa-psk"),
        "wpa-psk" => key == "wpa-psk",
        _ => false,
    };
    if !security_ok {
        return false;
    }
    if !profile.draft.bssid.is_empty() && !profile.draft.bssid.eq_ignore_ascii_case(bssid) {
        return false;
    }
    match profile.draft.band.as_str() {
        "bg" if !(2400..2500).contains(&frequency) => false,
        "a" if !(4900..5900).contains(&frequency) => false,
        "6GHz" if frequency < 5900 => false,
        _ => true,
    }
}
pub(super) fn wifi_key(device: &str, bytes: &[u8], security: &str) -> String {
    format!(
        "wifi:{device}:{}:{security}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}
fn list_words(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(|c: char| c.is_whitespace() || c == ',' || c == ';')
        .filter(|v| !v.is_empty())
}
fn family_addr(text: &str, ipv6: bool) -> Result<IpAddr> {
    let address: IpAddr = text.parse().context("Invalid IP address")?;
    if address.is_ipv6() != ipv6 {
        bail!("IP address has the wrong address family");
    }
    Ok(address)
}
fn address_data(text: &str, ipv6: bool) -> Result<Vec<HashMap<String, OwnedValue>>> {
    list_words(text)
        .map(|part| {
            let (address, prefix) = part
                .split_once('/')
                .context("Addresses must use address/prefix notation")?;
            family_addr(address, ipv6)?;
            let prefix: u32 = prefix.parse().context("Invalid network prefix")?;
            if prefix > if ipv6 { 128 } else { 32 } {
                bail!("Invalid network prefix");
            }
            Ok(HashMap::from([
                ("address".into(), string(address)),
                ("prefix".into(), OwnedValue::from(prefix)),
            ]))
        })
        .collect()
}
fn route_data(text: &str, ipv6: bool) -> Result<Vec<HashMap<String, OwnedValue>>> {
    text.split(['\n', ';'])
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() > 3 {
                bail!("Routes use destination/prefix [gateway] [metric]");
            }
            let mut row = address_data(fields[0], ipv6)?
                .pop()
                .context("Invalid route")?;
            let dest = row.remove("address").unwrap();
            row.insert("dest".into(), dest);
            if let Some(gateway) = fields.get(1) {
                family_addr(gateway, ipv6)?;
                row.insert("next-hop".into(), string(gateway));
            }
            if let Some(metric) = fields.get(2) {
                row.insert(
                    "metric".into(),
                    OwnedValue::from(metric.parse::<u32>().context("Invalid route metric")?),
                );
            }
            Ok(row)
        })
        .collect()
}
pub(super) fn map_data(text: &str) -> Result<HashMap<String, String>> {
    if text.trim_start().starts_with('{') {
        return serde_json::from_str(text)
            .context("VPN properties must be a JSON object of string values");
    }
    let mut map = HashMap::new();
    for line in text.lines().map(str::trim).filter(|s| !s.is_empty()) {
        let (key, value) = line
            .split_once('=')
            .context("VPN properties use key=value lines")?;
        if key.is_empty() || key.contains('\0') || value.contains('\0') {
            bail!("Invalid VPN property");
        }
        map.insert(key.into(), value.into());
    }
    Ok(map)
}
fn set_text(map: &mut HashMap<String, OwnedValue>, key: &str, value: &str) {
    if value.is_empty() {
        map.remove(key);
    } else {
        map.insert(key.into(), string(value));
    }
}
pub(super) fn valid_psk(password: &str, sae: bool) -> bool {
    if sae {
        !password.is_empty() && password.len() <= 255 && !password.contains('\0')
    } else {
        password.is_ascii()
            && ((8..=63).contains(&password.len())
                || (password.len() == 64 && password.bytes().all(|b| b.is_ascii_hexdigit())))
    }
}
pub(super) fn build(
    draft: &ProfileDraft,
    mut settings: Settings,
    username: &str,
) -> Result<Settings> {
    let original = self::draft(&draft.path, &settings);
    let existing = !settings.is_empty();
    if draft.name.trim().is_empty() || draft.name.contains('\0') {
        bail!("Enter a profile name");
    }
    if !matches!(
        draft.kind.as_str(),
        "802-11-wireless" | "802-3-ethernet" | "vpn" | "gsm" | "cdma" | "wireguard"
    ) && settings.is_empty()
    {
        bail!("Import or select an existing profile for this connection type");
    }
    let con = settings.entry("connection".into()).or_default();
    con.insert("id".into(), string(&draft.name));
    con.insert("type".into(), string(&draft.kind));
    if !draft.uuid.is_empty() {
        con.insert("uuid".into(), string(&draft.uuid));
    }
    con.insert("autoconnect".into(), OwnedValue::from(draft.autoconnect));
    if draft.metered > 4 || draft.powersave > 3 {
        bail!("Invalid metered or power saving value");
    }
    con.insert("metered".into(), OwnedValue::from(draft.metered));
    if !(-999..=999).contains(&draft.priority) {
        bail!("Connection priority must be between -999 and 999");
    }
    con.insert(
        "autoconnect-priority".into(),
        OwnedValue::from(draft.priority),
    );
    let permissions: Vec<String> = if draft.shared {
        Vec::new()
    } else {
        vec![format!("user:{username}:")]
    };
    con.insert(
        "permissions".into(),
        Value::from(permissions).try_to_owned()?,
    );

    if !draft.device.starts_with('/') {
        set_text(con, "interface-name", &draft.device);
    }
    if draft.kind == "802-11-wireless" {
        if draft.ssid.is_empty() || draft.ssid.len() > 32 || draft.ssid.contains('\0') {
            bail!("SSID must contain 1 to 32 bytes");
        }
        let wifi = settings.entry("802-11-wireless".into()).or_default();
        let old_ssid = wifi
            .get("ssid")
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| Vec::<u8>::try_from(v).ok());
        let bytes = old_ssid
            .filter(|v| String::from_utf8_lossy(v) == draft.ssid)
            .unwrap_or_else(|| draft.ssid.as_bytes().to_vec());
        wifi.insert("ssid".into(), Value::from(bytes).try_to_owned()?);
        wifi.insert("hidden".into(), OwnedValue::from(draft.hidden));
        wifi.insert("powersave".into(), OwnedValue::from(draft.powersave));
        wifi.insert("mtu".into(), OwnedValue::from(draft.mtu));
        set_text(wifi, "assigned-mac-address", &draft.mac_address);
        set_text(wifi, "band", &draft.band);
        if !draft.band.is_empty() && !matches!(draft.band.as_str(), "a" | "bg" | "6GHz") {
            bail!("Invalid Wi-Fi band");
        }
        if draft.bssid.is_empty() {
            wifi.remove("bssid");
        } else {
            let bytes: Vec<u8> = draft
                .bssid
                .split(':')
                .map(|v| u8::from_str_radix(v, 16))
                .collect::<std::result::Result<_, _>>()
                .context("Invalid BSSID")?;
            if bytes.len() != 6 {
                bail!("BSSID must contain six hexadecimal bytes");
            }
            wifi.insert("bssid".into(), Value::from(bytes).try_to_owned()?);
        }
        let key = match draft.security.as_str() {
            "wpa2-wpa3" => "wpa-psk",
            "enterprise" => "wpa-eap",
            "wep" => "none",
            "owe-transition" => "open",
            value => value,
        };
        if key == "unknown" {
            bail!("Network security is not available yet. Refresh the list");
        }
        if key == "open" {
            settings.remove("802-11-wireless-security");
            settings.remove("802-1x");
        } else {
            if !matches!(
                key,
                "none"
                    | "ieee8021x"
                    | "owe"
                    | "wpa-psk"
                    | "sae"
                    | "wpa-eap"
                    | "wpa-eap-suite-b-192"
            ) {
                bail!("Unsupported Wi-Fi security mode");
            }
            let sec = settings
                .entry("802-11-wireless-security".into())
                .or_default();
            sec.insert("key-mgmt".into(), string(key));
            if matches!(key, "wpa-psk" | "sae") && !draft.password.is_empty() {
                if !valid_psk(&draft.password, key == "sae") {
                    bail!("WPA2 passwords must contain 8 to 63 ASCII characters or 64 hexadecimal digits");
                }
                sec.insert("psk".into(), string(&draft.password));
                sec.insert("psk-flags".into(), OwnedValue::from(0u32));
            } else if key == "none" && !draft.password.is_empty() {
                let len = draft.password.len();
                if !([5, 13].contains(&len) && draft.password.is_ascii())
                    && !([10, 26].contains(&len)
                        && draft.password.bytes().all(|b| b.is_ascii_hexdigit()))
                {
                    bail!("WEP keys must contain 5 or 13 ASCII characters, or 10 or 26 hexadecimal digits");
                }
                sec.insert("wep-key0".into(), string(&draft.password));
                sec.insert("wep-key-type".into(), OwnedValue::from(1u32));
                sec.insert("wep-key-flags".into(), OwnedValue::from(0u32));
            }
            if matches!(key, "wpa-eap" | "wpa-eap-suite-b-192" | "ieee8021x") {
                let eap = settings.entry("802-1x".into()).or_default();
                if !matches!(
                    draft.eap.as_str(),
                    "tls"
                        | "peap"
                        | "ttls"
                        | "pwd"
                        | "fast"
                        | "leap"
                        | "md5"
                        | "sim"
                        | "aka"
                        | "aka'"
                ) {
                    bail!("Select a supported EAP method");
                }
                if draft.username.is_empty()
                    && !matches!(draft.eap.as_str(), "sim" | "aka" | "aka'")
                {
                    bail!("Enter the network username");
                }
                eap.insert(
                    "eap".into(),
                    Value::from(vec![draft.eap.clone()]).try_to_owned()?,
                );
                set_text(eap, "identity", &draft.username);
                set_text(eap, "anonymous-identity", &draft.anonymous_identity);
                if matches!(draft.eap.as_str(), "peap" | "ttls" | "fast") {
                    set_text(eap, "phase2-auth", &draft.phase2_auth);
                } else {
                    eap.remove("phase2-auth");
                }
                set_text(eap, "domain-suffix-match", &draft.domain_suffix_match);
                for (name, text) in [
                    ("ca-cert", &draft.ca_cert),
                    ("client-cert", &draft.client_cert),
                    ("private-key", &draft.private_key),
                ] {
                    let original_text = match name {
                        "ca-cert" => &original.ca_cert,
                        "client-cert" => &original.client_cert,
                        _ => &original.private_key,
                    };
                    if existing && text == original_text {
                        continue;
                    }
                    if text.is_empty() {
                        eap.remove(name);
                    } else if text.starts_with("pkcs11:") {
                        eap.insert(
                            name.into(),
                            Value::from(format!("{text}\0").into_bytes()).try_to_owned()?,
                        );
                    } else {
                        eap.insert(
                            name.into(),
                            Value::from(format!("file://{text}\0").into_bytes()).try_to_owned()?,
                        );
                    }
                }

                if matches!(draft.eap.as_str(), "tls" | "peap" | "ttls" | "fast")
                    && draft.ca_cert.is_empty()
                {
                    eap.insert("system-ca-certs".into(), OwnedValue::from(true));
                } else if !draft.ca_cert.is_empty()
                    && (!existing || draft.ca_cert != original.ca_cert)
                {

                    eap.insert("system-ca-certs".into(), OwnedValue::from(false));
                }
                if !draft.password.is_empty() {
                    eap.insert("password".into(), string(&draft.password));
                    eap.insert("password-flags".into(), OwnedValue::from(0u32));
                }
                if !draft.private_key_password.is_empty() {
                    eap.insert(
                        "private-key-password".into(),
                        string(&draft.private_key_password),
                    );
                    eap.insert("private-key-password-flags".into(), OwnedValue::from(0u32));
                }
            }
        }
    } else if draft.kind == "802-3-ethernet" {
        let ethernet = settings.entry("802-3-ethernet".into()).or_default();
        ethernet.insert("mtu".into(), OwnedValue::from(draft.mtu));
        set_text(ethernet, "assigned-mac-address", &draft.mac_address);
    } else if draft.kind == "vpn" {
        if !draft
            .vpn_service
            .starts_with("org.freedesktop.NetworkManager.")
        {
            bail!("Enter the VPN plugin service type");
        }
        let vpn = settings.entry("vpn".into()).or_default();
        vpn.insert("service-type".into(), string(&draft.vpn_service));
        set_text(vpn, "user-name", &draft.username);
        vpn.insert(
            "data".into(),
            Value::from(map_data(&draft.vpn_data)?).try_to_owned()?,
        );
        if !draft.vpn_secrets.is_empty() {
            vpn.insert(
                "secrets".into(),
                Value::from(map_data(&draft.vpn_secrets)?).try_to_owned()?,
            );
        }
    } else if draft.kind == "gsm" || draft.kind == "cdma" {
        let cell = settings.entry(draft.kind.clone()).or_default();
        set_text(cell, "username", &draft.username);
        set_text(cell, "number", &draft.number);
        if draft.kind == "gsm" {
            set_text(cell, "apn", &draft.apn);
        }
        if !draft.password.is_empty() {
            cell.insert("password".into(), string(&draft.password));
            cell.insert("password-flags".into(), OwnedValue::from(0u32));
        }
        if !draft.pin.is_empty() && draft.kind == "gsm" {
            cell.insert("pin".into(), string(&draft.pin));
            cell.insert("pin-flags".into(), OwnedValue::from(0u32));
        }
    }
    if !matches!(draft.proxy_method.as_str(), "none" | "auto") {
        bail!("Proxy method must be none or auto");
    }
    let proxy = settings.entry("proxy".into()).or_default();
    proxy.insert(
        "method".into(),
        OwnedValue::from(if draft.proxy_method == "auto" {
            1i32
        } else {
            0i32
        }),
    );
    set_text(proxy, "pac-url", &draft.proxy_pac_url);
    set_text(proxy, "pac-script", &draft.proxy_pac_script);
    for (family, method, addresses, gateway, dns, routes) in [
        (
            "ipv4",
            &draft.ipv4_method,
            &draft.ipv4_addresses,
            &draft.ipv4_gateway,
            &draft.ipv4_dns,
            &draft.ipv4_routes,
        ),
        (
            "ipv6",
            &draft.ipv6_method,
            &draft.ipv6_addresses,
            &draft.ipv6_gateway,
            &draft.ipv6_dns,
            &draft.ipv6_routes,
        ),
    ] {
        let ipv6 = family == "ipv6";
        if existing {
            let same = if ipv6 {
                method == &original.ipv6_method
                    && addresses == &original.ipv6_addresses
                    && gateway == &original.ipv6_gateway
                    && dns == &original.ipv6_dns
                    && routes == &original.ipv6_routes
            } else {
                method == &original.ipv4_method
                    && addresses == &original.ipv4_addresses
                    && gateway == &original.ipv4_gateway
                    && dns == &original.ipv4_dns
                    && routes == &original.ipv4_routes
            };
            if same {
                continue;
            }
        }
        if !matches!(
            method.as_str(),
            "auto" | "manual" | "link-local" | "shared" | "disabled" | "ignore" | "dhcp"
        ) {
            bail!("Invalid IP configuration method");
        }
        let ip = settings.entry(family.into()).or_default();
        ip.insert("method".into(), string(method));
        let addresses = address_data(addresses, ipv6)?;
        if method == "manual" && addresses.is_empty() {
            bail!("Manual IP configuration needs an address");
        }

        ip.remove("addresses");
        ip.remove("routes");
        ip.remove("dns");
        ip.insert(
            "address-data".into(),
            Value::from(addresses).try_to_owned()?,
        );
        ip.insert(
            "route-data".into(),
            Value::from(route_data(routes, ipv6)?).try_to_owned()?,
        );
        if !gateway.is_empty() {
            family_addr(gateway, ipv6)?;
        }
        set_text(ip, "gateway", gateway);
        let dns: Vec<String> = list_words(dns)
            .map(|s| {
                let address = s.split_once('#').map(|v| v.0).unwrap_or(s);
                family_addr(address, ipv6)?;
                Ok(s.into())
            })
            .collect::<Result<_>>()?;
        ip.insert("dns-data".into(), Value::from(dns).try_to_owned()?);
    }
    Ok(settings)
}
pub(super) fn new_uuid() -> Result<String> {
    let value = std::fs::read_to_string("/proc/sys/kernel/random/uuid")
        .context("Could not create a connection ID")?;
    Ok(value.trim().into())
}
pub(super) fn user_name() -> Result<String> {

    let uid = unsafe { libc::getuid() };
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut found = std::ptr::null_mut();
    let mut buf = vec![0u8; 16384];
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            entry.as_mut_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut found,
        )
    };
    if rc != 0 || found.is_null() {
        bail!("Could not determine the current user");
    }
    let name = unsafe { std::ffi::CStr::from_ptr((*found).pw_name) }
        .to_str()
        .context("Invalid user name")?;
    Ok(name.into())
}
