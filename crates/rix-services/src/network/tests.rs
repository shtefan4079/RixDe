use super::*;
use profiles::{ssid as setting_ssid, str_setting as setting_string, string};
use std::sync::{Arc, Mutex};
use zbus::zvariant::{OwnedObjectPath, Value};
fn settings(name: &str, ssid: &[u8], security: &str) -> Settings {
    let mut values = HashMap::from([
        (
            "connection".into(),
            HashMap::from([
                ("id".into(), string(name)),
                ("uuid".into(), string(name)),
                ("type".into(), string("802-11-wireless")),
            ]),
        ),
        (
            "802-11-wireless".into(),
            HashMap::from([(
                "ssid".into(),
                Value::from(ssid.to_vec()).try_to_owned().unwrap(),
            )]),
        ),
    ]);
    if security != "open" {
        values.insert(
            "802-11-wireless-security".into(),
            HashMap::from([("key-mgmt".into(), string(security))]),
        );
    }
    values
}
fn cloned(settings: &Settings) -> Settings {
    settings
        .iter()
        .map(|(g, m)| {
            (
                g.clone(),
                m.iter()
                    .map(|(k, v)| (k.clone(), v.try_clone().unwrap()))
                    .collect(),
            )
        })
        .collect()
}
fn profile(path: &str, security: &str) -> profiles::Profile {
    let settings = settings(path, b"fixture", security);
    let draft = profiles::draft(path, &settings);
    profiles::Profile {
        path: path.into(),
        settings,
        draft,
        priority: 0,
        timestamp: 0,
    }
}
#[test]
fn security_modes_keep_transition_and_suite_b_distinct() {
    assert_eq!(profiles::security(0, 0, 0), "open");
    assert_eq!(profiles::security(1, 0, 0x100), "wpa-psk");
    assert_eq!(profiles::security(1, 0, 0x400), "sae");
    assert_eq!(profiles::security(1, 0x100, 0x400), "wpa2-wpa3");
    assert_eq!(profiles::security(1, 0, 0x200), "enterprise");
    assert_eq!(profiles::security(1, 0, 0x2000), "wpa-eap-suite-b-192");
    assert_eq!(profiles::security(1, 0, 0), "wep");
    assert_eq!(profiles::security(0, 0, 0x1000), "owe-transition");
}
#[test]
fn profile_compatibility_checks_device_security_and_bssid() {
    let mut saved = profile("/test/profile", "sae");
    let available = vec![OwnedObjectPath::try_from("/test/profile").unwrap()];
    assert!(!profiles::compatible(
        &saved,
        b"fixture",
        "sae",
        &[],
        "00:01:02:03:04:05",
        2412
    ));
    assert!(!profiles::compatible(
        &saved,
        b"fixture",
        "wpa-psk",
        &available,
        "00:01:02:03:04:05",
        2412
    ));
    assert!(profiles::compatible(
        &saved,
        b"fixture",
        "wpa2-wpa3",
        &available,
        "00:01:02:03:04:05",
        2412
    ));
    saved.draft.bssid = "00:01:02:03:04:06".into();
    assert!(!profiles::compatible(
        &saved,
        b"fixture",
        "sae",
        &available,
        "00:01:02:03:04:05",
        2412
    ));
    saved.draft.bssid.clear();
    saved.draft.band = "a".into();
    assert!(!profiles::compatible(
        &saved, b"fixture", "sae", &available, "", 2412
    ));
}
#[test]
fn saved_wep_and_suite_b_are_compatible_with_correct_aps() {
    for (key, mode) in [
        ("none", "wep"),
        ("ieee8021x", "wep"),
        ("wpa-eap-suite-b-192", "wpa-eap-suite-b-192"),
    ] {
        let profile = profile("/test/profile", key);
        let available = vec![OwnedObjectPath::try_from("/test/profile").unwrap()];
        assert!(profiles::compatible(
            &profile, b"fixture", mode, &available, "", 2412
        ));
    }
}
#[test]
fn row_identity_preserves_raw_ssid_and_is_not_an_ap_path() {
    assert_ne!(
        profiles::wifi_key("/device/a", &[0xff], "sae"),
        profiles::wifi_key("/device/a", &[0xfe], "sae")
    );
    assert_ne!(
        profiles::wifi_key("/device/a", b"fixture", "sae"),
        profiles::wifi_key("/device/b", b"fixture", "sae")
    );
}
#[test]
fn non_utf8_saved_ssid_roundtrips_an_unrelated_edit() {
    let values = settings("Original name", &[b'f', 0xff, b'o'], "wpa-psk");
    let mut draft = profiles::draft("/test/profile", &values);
    draft.name = "Renamed".into();
    let result = profiles::build(&draft, values, "fixture-user").unwrap();
    assert_eq!(setting_ssid(&result), Some(vec![b'f', 0xff, b'o']));
    assert_eq!(setting_string(&result, "connection", "id"), Some("Renamed"));
}
#[test]
fn selecting_an_explicit_eap_ca_replaces_system_trust_policy() {
    let mut values = settings("Office", b"Fixture", "wpa-eap");
    values.insert(
        "802-1x".into(),
        HashMap::from([
            ("identity".into(), string("fixture-user")),
            (
                "eap".into(),
                Value::from(vec!["peap"]).try_to_owned().unwrap(),
            ),
            ("system-ca-certs".into(), OwnedValue::from(true)),
        ]),
    );
    let mut draft = profiles::draft("/test/profile", &values);
    draft.ca_cert = "/tmp/fixture-ca.pem".into();
    let result = profiles::build(&draft, values, "fixture-user").unwrap();
    assert!(!bool::try_from(&result["802-1x"]["system-ca-certs"]).unwrap());
    assert_eq!(
        Vec::<u8>::try_from(result["802-1x"]["ca-cert"].try_clone().unwrap()).unwrap(),
        b"file:///tmp/fixture-ca.pem\0"
    );
}
#[test]
fn profile_editor_builds_modern_ip_routes_dns_scope_and_proxy() {
    let draft = ProfileDraft {
        name: "Fixture".into(),
        ssid: "Fixture".into(),
        password: "fixture-password".into(),
        ipv4_method: "manual".into(),
        ipv4_addresses: "192.0.2.5/24".into(),
        ipv4_gateway: "192.0.2.1".into(),
        ipv4_dns: "192.0.2.53".into(),
        ipv4_routes: "198.51.100.0/24 192.0.2.1 20".into(),
        ipv6_dns: "2001:db8::53".into(),
        proxy_method: "auto".into(),
        proxy_pac_url: "https://proxy.example/pac".into(),
        priority: 20,
        ..Default::default()
    };
    let values = profiles::build(&draft, Settings::new(), "fixture-user").unwrap();
    let public = profiles::draft("/profile/test", &values);
    assert_eq!(public.ipv4_addresses, "192.0.2.5/24");
    assert_eq!(public.ipv4_gateway, "192.0.2.1");
    assert_eq!(public.ipv4_dns, "192.0.2.53");
    assert_eq!(public.ipv4_routes, "198.51.100.0/24 192.0.2.1 20");
    assert_eq!(public.ipv6_dns, "2001:db8::53");
    assert_eq!(public.priority, 20);
    assert!(!public.shared);
    assert!(public.password.is_empty());
    let permissions: Vec<String> =
        profiles::value_vec(&values, "connection", "permissions").unwrap();
    assert_eq!(permissions, vec!["user:fixture-user:"]);
    assert_eq!(
        setting_string(&values, "proxy", "pac-url"),
        Some("https://proxy.example/pac")
    );
}
#[test]
fn imported_blob_certificate_and_ip_attributes_survive_name_edit() {
    let mut values = settings("Certificate network", b"fixture", "wpa-eap");
    values.insert(
        "802-1x".into(),
        HashMap::from([
            (
                "eap".into(),
                Value::from(vec!["tls".to_string()]).try_to_owned().unwrap(),
            ),
            ("identity".into(), string("fixture-user")),
            (
                "ca-cert".into(),
                Value::from(vec![0x30u8, 0x82, 0x00, 0x01])
                    .try_to_owned()
                    .unwrap(),
            ),
        ]),
    );
    values.insert(
        "ipv4".into(),
        HashMap::from([
            ("method".into(), string("auto")),
            (
                "dns".into(),
                Value::from(vec![u32::from_ne_bytes([192, 0, 2, 53])])
                    .try_to_owned()
                    .unwrap(),
            ),
            ("route-table".into(), OwnedValue::from(100u32)),
        ]),
    );
    let mut draft = profiles::draft("/test/profile", &values);
    draft.name = "Renamed".into();
    let result = profiles::build(&draft, values, "fixture-user").unwrap();
    assert_eq!(
        profiles::value_vec::<Vec<u8>>(&result, "802-1x", "ca-cert"),
        Some(vec![0x30, 0x82, 0, 1])
    );
    assert!(result["ipv4"].contains_key("dns"));
    assert_eq!(u32::try_from(&result["ipv4"]["route-table"]).unwrap(), 100);
}
#[test]
fn invalid_ip_credentials_and_ssid_are_rejected_before_dbus() {
    let mut draft = ProfileDraft {
        name: "Fixture".into(),
        ssid: "fixture".into(),
        password: "short".into(),
        ..Default::default()
    };
    assert!(profiles::build(&draft, Settings::new(), "user").is_err());
    draft.password = "fixture-password".into();
    draft.ipv4_method = "manual".into();
    assert!(profiles::build(&draft, Settings::new(), "user").is_err());
    draft.ipv4_addresses = "2001:db8::1/64".into();
    assert!(profiles::build(&draft, Settings::new(), "user").is_err());
    draft.ipv4_addresses = "192.0.2.1/99".into();
    assert!(profiles::build(&draft, Settings::new(), "user").is_err());
    draft.ipv4_method = "auto".into();
    draft.ipv4_addresses.clear();
    draft.ssid = "x".repeat(33);
    assert!(profiles::build(&draft, Settings::new(), "user").is_err());
}
#[test]
fn sae_short_password_allowed_and_wpa2_unicode_rejected() {
    assert!(profiles::valid_psk("a", true));
    assert!(!profiles::valid_psk("unicode_§§§_test", false));
    assert!(profiles::valid_psk(&"A".repeat(64), false));
}
#[test]
fn editor_preserves_vpn_and_gsm_fields_without_exporting_secrets() {
    let vpn = ProfileDraft {
        kind: "vpn".into(),
        name: "VPN fixture".into(),
        vpn_service: "org.freedesktop.NetworkManager.openvpn".into(),
        vpn_data: "{\"remote\":\"vpn.example\"}".into(),
        vpn_secrets: "{\"password\":\"fixture-secret\"}".into(),
        username: "vpn-user".into(),
        ..Default::default()
    };
    let values = profiles::build(&vpn, Settings::new(), "fixture-user").unwrap();
    let public = profiles::draft("/vpn/fixture", &values);
    assert_eq!(public.username, "vpn-user");
    assert!(public.vpn_secrets.is_empty());
    assert_eq!(public.vpn_data, "remote=vpn.example");
    let gsm = ProfileDraft {
        kind: "gsm".into(),
        name: "Mobile fixture".into(),
        apn: "internet.example".into(),
        username: "cell-user".into(),
        password: "fixture-secret".into(),
        pin: "0000".into(),
        ..Default::default()
    };
    let values = profiles::build(&gsm, Settings::new(), "fixture-user").unwrap();
    assert_eq!(
        setting_string(&values, "gsm", "apn"),
        Some("internet.example")
    );
    let public = profiles::draft("/gsm/fixture", &values);
    assert!(public.password.is_empty());
    assert!(public.pin.is_empty());
}
#[test]
fn unavailable_service_has_no_stale_connection() {
    let state = unavailable("fixture unavailable".into());
    assert!(!state.available);
    assert!(!state.wifi);
    assert!(state.networks.is_empty());
    assert!(state.devices.is_empty());
}
#[test]
fn pending_overlay_uses_profile_lifecycle_not_ap_identity() {
    let mut state = Snapshot {
        networks: vec![Network {
            path: "wifi:stable".into(),
            profile_path: "/profile/temporary".into(),
            ap_path: "/ap/roamed".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let pending = HashMap::from([(
        1,
        Pending {
            key: "profile-editor".into(),
            active: "/active/1".into(),
            profile: "/profile/temporary".into(),
            device: "/device/1".into(),
            started: Instant::now(),
            operation: 1,
            editor: true,
            wireless: true,
        },
    )]);
    overlay_pending(&mut state, &pending);
    assert_eq!(state.networks[0].state, 40);
    assert_eq!(state.pending, "profile-editor");
}

#[derive(Default)]
struct Received {
    count: usize,
    password: String,
    persistence: String,
    ssid: Vec<u8>,
    updates: usize,
    updated_name: String,
    updated_security: String,
    device: String,
    hotspot_mode: String,
    saved_profiles: usize,
    events: Vec<String>,
}
struct Manager(Arc<Mutex<Received>>);
#[zbus::interface(name = "org.freedesktop.NetworkManager")]
impl Manager {
    fn add_and_activate_connection2(
        &self,
        settings: Settings,
        device: OwnedObjectPath,
        _specific: OwnedObjectPath,
        options: HashMap<String, OwnedValue>,
    ) -> (
        OwnedObjectPath,
        OwnedObjectPath,
        HashMap<String, OwnedValue>,
    ) {
        let mut r = self.0.lock().unwrap();
        r.count += 1;
        r.device = device.to_string();
        r.hotspot_mode = setting_string(&settings, "802-11-wireless", "mode")
            .unwrap_or_default()
            .into();
        r.password = setting_string(&settings, "802-11-wireless-security", "psk")
            .unwrap_or_default()
            .into();
        r.persistence = options
            .get("persist")
            .and_then(|v| <&str>::try_from(v).ok())
            .unwrap_or_default()
            .into();
        r.ssid = setting_ssid(&settings).unwrap_or_default();
        (
            OwnedObjectPath::try_from("/test/new_profile").unwrap(),
            OwnedObjectPath::try_from("/test/active").unwrap(),
            HashMap::new(),
        )
    }
    #[zbus(property)]
    fn active_connections(&self) -> Vec<OwnedObjectPath> {
        vec![OwnedObjectPath::try_from("/test/active").unwrap()]
    }
    fn deactivate_connection(&self, path: OwnedObjectPath) {
        self.0
            .lock()
            .unwrap()
            .events
            .push(format!("deactivate:{path}"));
    }
    fn activate_connection(
        &self,
        _profile: OwnedObjectPath,
        _device: OwnedObjectPath,
        _specific: OwnedObjectPath,
    ) -> OwnedObjectPath {
        OwnedObjectPath::try_from("/test/active").unwrap()
    }
}
struct Saved(Arc<Mutex<Received>>);
#[zbus::interface(name = "org.freedesktop.NetworkManager.Settings.Connection")]
impl Saved {
    fn delete(&self) {
        self.0.lock().unwrap().events.push("delete".into());
    }
    fn get_settings(&self) -> Settings {
        settings("Keep this custom name", b"fixture", "sae")
    }
    fn get_secrets(&self, _setting: String) -> Settings {
        HashMap::from([(
            "802-11-wireless-security".into(),
            HashMap::from([("psk".into(), string("previous-fixture-secret"))]),
        )])
    }
    fn update2(
        &self,
        settings: Settings,
        _flags: u32,
        _args: HashMap<String, OwnedValue>,
    ) -> HashMap<String, OwnedValue> {
        let mut r = self.0.lock().unwrap();
        r.updates += 1;
        r.updated_name = setting_string(&settings, "connection", "id")
            .unwrap()
            .into();
        r.updated_security = setting_string(&settings, "802-11-wireless-security", "key-mgmt")
            .unwrap()
            .into();
        HashMap::new()
    }
}
#[tokio::test]
#[ignore = "requires isolated dbus-run-session; never use the system bus"]
async fn private_bus_password_persistence_and_existing_policy() {
    let received = Arc::new(Mutex::new(Received::default()));
    let bus = zbus::connection::Builder::session()
        .unwrap()
        .serve_at(ROOT, Manager(received.clone()))
        .unwrap()
        .serve_at("/test/saved", Saved(received.clone()))
        .unwrap()
        .name(NM)
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut state = Snapshot {
        networks: vec![Network {
            path: "wifi:fixture".into(),
            ap_path: "/test/ap".into(),
            device: "/test/device".into(),
            name: "fixture".into(),
            ssid: b"fixture".to_vec(),
            security: "wpa-psk".into(),
            available: true,
            ..Default::default()
        }],
        ..Default::default()
    };
    let action = |p: &str, r| Action::Connect {
        path: "wifi:fixture".into(),
        password: p.into(),
        remember: r,
    };
    assert!(operations::apply(&bus, action("short", false), &state)
        .await
        .is_err());
    assert_eq!(received.lock().unwrap().count, 0);
    operations::apply(&bus, action("fixture-password", false), &state)
        .await
        .unwrap();
    assert_eq!(received.lock().unwrap().persistence, "volatile");
    assert_eq!(received.lock().unwrap().ssid, b"fixture");
    operations::apply(&bus, action("fixture-password", true), &state)
        .await
        .unwrap();
    assert_eq!(received.lock().unwrap().persistence, "disk");
    state.networks[0].ssid = vec![0xff; 32];
    operations::apply(&bus, action("fixture-password", true), &state)
        .await
        .unwrap();
    assert_eq!(received.lock().unwrap().ssid, vec![0xff; 32]);
    state.networks[0].ssid = b"fixture".to_vec();
    state.networks[0].saved = "/test/saved".into();
    state.networks[0].profile_path = "/test/saved".into();
    state.networks[0].security = "wpa2-wpa3".into();
    operations::apply(&bus, action("new-fixture-password", true), &state)
        .await
        .unwrap();
    {
        let r = received.lock().unwrap();
        assert_eq!(r.updates, 1);
        assert_eq!(r.updated_name, "Keep this custom name");
        assert_eq!(r.updated_security, "sae");
    }
    operations::apply(&bus, action("", false), &state)
        .await
        .unwrap();
    {
        let r = received.lock().unwrap();
        assert_eq!(
            r.updates, 1,
            "Temporary activation must not modify the saved profile"
        );
        assert_eq!(r.persistence, "volatile");
        assert_eq!(r.password, "previous-fixture-secret");
    }
}

#[tokio::test]
#[ignore = "requires isolated dbus-run-session; never use the system bus"]
async fn private_bus_agent_authentication_prompt_cache_and_cancel() {
    let nm = zbus::connection::Builder::session()
        .unwrap()
        .name(NM)
        .unwrap()
        .build()
        .await
        .unwrap();
    let server = Connection::session().await.unwrap();
    let outsider = Connection::session().await.unwrap();
    let control = agent::Control::new();
    control.export(&server).await.unwrap();
    let destination = server.unique_name().unwrap().as_str();
    let bad = Proxy::new(
        &outsider,
        destination,
        "/org/freedesktop/NetworkManager/SecretAgent",
        "org.freedesktop.NetworkManager.SecretAgent",
    )
    .await
    .unwrap();
    let mut values = settings("fixture", b"fixture", "wpa-psk");
    values
        .get_mut("802-11-wireless-security")
        .unwrap()
        .insert("psk-flags".into(), OwnedValue::from(1u32));
    let path = OwnedObjectPath::try_from("/test/profile").unwrap();
    assert!(bad
        .call::<_, _, Settings>(
            "GetSecrets",
            &(
                cloned(&values),
                path.clone(),
                "802-11-wireless-security",
                vec!["psk"],
                1u32
            )
        )
        .await
        .is_err());
    assert!(control.updates.borrow().is_empty());
    assert!(bad
        .call::<_, _, ()>("SaveSecrets", &(cloned(&values), path.clone()))
        .await
        .is_err());
    let nm2 = nm.clone();
    let destination = destination.to_string();
    let input = cloned(&values);
    let request_path = path.clone();
    let request = tokio::spawn(async move {
        Proxy::new(
            &nm2,
            destination,
            "/org/freedesktop/NetworkManager/SecretAgent",
            "org.freedesktop.NetworkManager.SecretAgent",
        )
        .await
        .unwrap()
        .call::<_, _, Settings>(
            "GetSecrets",
            &(
                input,
                request_path,
                "802-11-wireless-security",
                vec!["psk"],
                1u32,
            ),
        )
        .await
    });
    let mut updates = control.updates.clone();
    tokio::time::timeout(Duration::from_secs(2), updates.changed())
        .await
        .unwrap()
        .unwrap();
    let id = updates.borrow()[0].id;
    control
        .reply(
            id,
            String::new(),
            "fixture-secret".into(),
            String::new(),
            String::new(),
            false,
        )
        .unwrap();
    let result = request.await.unwrap().unwrap();
    assert_eq!(
        setting_string(&result, "802-11-wireless-security", "psk"),
        Some("fixture-secret")
    );
    let good = Proxy::new(
        &nm,
        server.unique_name().unwrap().as_str(),
        "/org/freedesktop/NetworkManager/SecretAgent",
        "org.freedesktop.NetworkManager.SecretAgent",
    )
    .await
    .unwrap();
    let cached: Settings = good
        .call(
            "GetSecrets",
            &(
                cloned(&values),
                path.clone(),
                "802-11-wireless-security",
                vec!["psk"],
                0u32,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        setting_string(&cached, "802-11-wireless-security", "psk"),
        Some("fixture-secret")
    );
    assert!(bad
        .call::<_, _, Settings>(
            "GetSecrets",
            &(
                cloned(&values),
                path.clone(),
                "802-11-wireless-security",
                vec!["psk"],
                0u32
            )
        )
        .await
        .is_err());
    good.call::<_, _, ()>("DeleteSecrets", &(values, path))
        .await
        .unwrap();

    let eap = HashMap::from([
        (
            "connection".into(),
            HashMap::from([("id".into(), string("EAP fixture"))]),
        ),
        (
            "802-1x".into(),
            HashMap::from([
                ("password-flags".into(), OwnedValue::from(1u32)),
                ("private-key-password-flags".into(), OwnedValue::from(3u32)),
            ]),
        ),
    ]);
    let nm2 = nm.clone();
    let destination = server.unique_name().unwrap().as_str().to_owned();
    let request_values = cloned(&eap);
    let eap_path = OwnedObjectPath::try_from("/test/eap").unwrap();
    let request_path = eap_path.clone();
    let request = tokio::spawn(async move {
        Proxy::new(
            &nm2,
            destination,
            "/org/freedesktop/NetworkManager/SecretAgent",
            "org.freedesktop.NetworkManager.SecretAgent",
        )
        .await
        .unwrap()
        .call::<_, _, Settings>(
            "GetSecrets",
            &(
                request_values,
                request_path,
                "802-1x",
                vec!["password"],
                1u32,
            ),
        )
        .await
    });
    let mut updates = control.updates.clone();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !updates.borrow().is_empty() {
                break;
            }
            updates.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let id = updates.borrow()[0].id;
    assert!(updates.borrow()[0].username);
    control
        .reply(
            id,
            "fixture-user".into(),
            "fixture-eap-password".into(),
            "temporary-key-fixture".into(),
            String::new(),
            false,
        )
        .unwrap();
    assert_eq!(
        setting_string(&request.await.unwrap().unwrap(), "802-1x", "identity"),
        Some("fixture-user")
    );
    let remembered: Settings = good
        .call(
            "GetSecrets",
            &(
                cloned(&eap),
                eap_path.clone(),
                "802-1x",
                vec!["password"],
                0u32,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        setting_string(&remembered, "802-1x", "identity"),
        Some("fixture-user")
    );
    assert_eq!(
        setting_string(&remembered, "802-1x", "password"),
        Some("fixture-eap-password")
    );
    assert!(setting_string(&remembered, "802-1x", "private-key-password").is_none());
    assert!(good
        .call::<_, _, Settings>(
            "GetSecrets",
            &(
                cloned(&eap),
                eap_path.clone(),
                "802-1x",
                vec!["private-key-password"],
                0u32
            )
        )
        .await
        .is_err());
    assert!(
        control.updates.borrow().is_empty(),
        "noninteractive request cannot publish a ghost prompt"
    );
    good.call::<_, _, ()>("DeleteSecrets", &(eap, eap_path))
        .await
        .unwrap();
    control.clear();
    assert!(control.updates.borrow().is_empty());
}

struct ActiveProfile;
#[zbus::interface(name = "org.freedesktop.NetworkManager.Connection.Active")]
impl ActiveProfile {
    #[zbus(property)]
    fn connection(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from("/test/saved").unwrap()
    }
}
struct WirelessCaps(u32);
#[zbus::interface(name = "org.freedesktop.NetworkManager.Device.Wireless")]
impl WirelessCaps {
    fn request_scan(&self, _options: HashMap<String, OwnedValue>) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::AccessDenied(
            "fixture system policy".into(),
        ))
    }
    #[zbus(property)]
    fn wireless_capabilities(&self) -> u32 {
        self.0
    }
}
struct SettingsWriter(Arc<Mutex<Received>>);
#[zbus::interface(name = "org.freedesktop.NetworkManager.Settings")]
impl SettingsWriter {
    fn add_connection2(
        &self,
        settings: Settings,
        _flags: u32,
        _args: HashMap<String, OwnedValue>,
    ) -> (OwnedObjectPath, HashMap<String, OwnedValue>) {
        let mut r = self.0.lock().unwrap();
        r.saved_profiles += 1;
        r.hotspot_mode = setting_string(&settings, "802-11-wireless", "mode")
            .unwrap_or_default()
            .into();
        (
            OwnedObjectPath::try_from("/test/new_profile").unwrap(),
            HashMap::new(),
        )
    }
}
#[tokio::test]
#[ignore = "requires isolated dbus-run-session; never use the system bus"]
async fn private_bus_forget_disconnects_before_deleting_active_profile() {
    let received = Arc::new(Mutex::new(Received::default()));
    let bus = zbus::connection::Builder::session()
        .unwrap()
        .serve_at(ROOT, Manager(received.clone()))
        .unwrap()
        .serve_at("/test/saved", Saved(received.clone()))
        .unwrap()
        .serve_at("/test/active", ActiveProfile)
        .unwrap()
        .name(NM)
        .unwrap()
        .build()
        .await
        .unwrap();
    let state = Snapshot {
        profiles: vec![ProfileDraft {
            path: "/test/saved".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    operations::apply(&bus, Action::Forget("/test/saved".into()), &state)
        .await
        .unwrap();
    assert_eq!(
        received.lock().unwrap().events,
        vec!["deactivate:/test/active", "delete"]
    );
}
#[tokio::test]
#[ignore = "requires isolated dbus-run-session; never use the system bus"]
async fn private_bus_hotspot_auto_skips_station_only_and_save_does_not_activate() {
    let received = Arc::new(Mutex::new(Received::default()));
    let bus = zbus::connection::Builder::session()
        .unwrap()
        .serve_at(ROOT, Manager(received.clone()))
        .unwrap()
        .serve_at(
            "/org/freedesktop/NetworkManager/Settings",
            SettingsWriter(received.clone()),
        )
        .unwrap()
        .serve_at("/test/station", WirelessCaps(0))
        .unwrap()
        .serve_at("/test/access_point", WirelessCaps(0x40))
        .unwrap()
        .name(NM)
        .unwrap()
        .build()
        .await
        .unwrap();
    let state = Snapshot {
        devices: vec![
            Device {
                path: "/test/station".into(),
                iface: "station0".into(),
                kind: 2,
                managed: true,
                hardware: true,
                available: true,
                ..Default::default()
            },
            Device {
                path: "/test/access_point".into(),
                iface: "ap0".into(),
                kind: 2,
                managed: true,
                hardware: true,
                available: true,
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let draft = ProfileDraft {
        name: "Fixture hotspot".into(),
        ssid: "Fixture hotspot".into(),
        kind: "hotspot".into(),
        password: "fixture-password".into(),
        ..Default::default()
    };
    operations::apply(&bus, Action::SaveProfile(draft.clone()), &state)
        .await
        .unwrap();
    {
        let r = received.lock().unwrap();
        assert_eq!(r.saved_profiles, 1);
        assert_eq!(r.count, 0);
        assert_eq!(r.hotspot_mode, "ap");
    }
    operations::apply(&bus, Action::Hotspot(draft), &state)
        .await
        .unwrap();
    let r = received.lock().unwrap();
    assert_eq!(r.count, 1);
    assert_eq!(r.device, "/test/access_point");
    assert_eq!(r.hotspot_mode, "ap");
}

#[derive(Default)]
struct CancellationState {
    calls: Vec<String>,
    active: Vec<String>,
}
struct CancellationManager(Arc<Mutex<CancellationState>>);
#[zbus::interface(name = "org.freedesktop.NetworkManager")]
impl CancellationManager {
    #[zbus(property)]
    fn devices(&self) -> Vec<OwnedObjectPath> {
        vec![]
    }
    #[zbus(property)]
    fn wireless_enabled(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn set_wireless_enabled(&self, enabled: bool) {
        self.0.lock().unwrap().calls.push(format!("wifi:{enabled}"));
    }
    #[zbus(property)]
    fn wireless_hardware_enabled(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn networking_enabled(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn connectivity(&self) -> u32 {
        4
    }
    #[zbus(property)]
    fn active_connections(&self) -> Vec<OwnedObjectPath> {
        self.0
            .lock()
            .unwrap()
            .active
            .iter()
            .map(|p| OwnedObjectPath::try_from(p.as_str()).unwrap())
            .collect()
    }
    fn get_permissions(&self) -> HashMap<String, String> {
        HashMap::new()
    }
    async fn add_and_activate_connection2(
        &self,
        _settings: Settings,
        _device: OwnedObjectPath,
        _specific: OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
    ) -> (
        OwnedObjectPath,
        OwnedObjectPath,
        HashMap<String, OwnedValue>,
    ) {
        self.0.lock().unwrap().calls.push("editor-start".into());
        tokio::time::sleep(Duration::from_millis(250)).await;
        self.0
            .lock()
            .unwrap()
            .active
            .push("/test/editor_active".into());
        (
            OwnedObjectPath::try_from("/test/new_profile").unwrap(),
            OwnedObjectPath::try_from("/test/editor_active").unwrap(),
            HashMap::new(),
        )
    }
    async fn activate_connection(
        &self,
        profile: OwnedObjectPath,
        _device: OwnedObjectPath,
        _specific: OwnedObjectPath,
    ) -> OwnedObjectPath {
        let (started, active) = if profile.as_str() == "/test/wired" {
            ("wired-start", "/test/wired_active")
        } else {
            ("row-start", "/test/row_active")
        };
        self.0.lock().unwrap().calls.push(started.into());
        tokio::time::sleep(Duration::from_millis(500)).await;
        self.0.lock().unwrap().active.push(active.into());
        OwnedObjectPath::try_from(active).unwrap()
    }
    fn deactivate_connection(&self, path: OwnedObjectPath) {
        let mut state = self.0.lock().unwrap();
        state.calls.push(format!("deactivate:{path}"));
        state.active.retain(|p| p != path.as_str());
    }
}
struct CancellationSettings;
#[zbus::interface(name = "org.freedesktop.NetworkManager.Settings")]
impl CancellationSettings {
    fn list_connections(&self) -> Vec<OwnedObjectPath> {
        vec![
            OwnedObjectPath::try_from("/test/saved").unwrap(),
            OwnedObjectPath::try_from("/test/wired").unwrap(),
        ]
    }
}
struct CancellationWired;
#[zbus::interface(name = "org.freedesktop.NetworkManager.Settings.Connection")]
impl CancellationWired {
    fn get_settings(&self) -> Settings {
        HashMap::from([(
            "connection".into(),
            HashMap::from([
                ("id".into(), string("Wired fixture")),
                ("uuid".into(), string("wired-fixture")),
                ("type".into(), string("802-3-ethernet")),
            ]),
        )])
    }
}
struct CancellationActive(&'static str);
#[zbus::interface(name = "org.freedesktop.NetworkManager.Connection.Active")]
impl CancellationActive {
    #[zbus(property)]
    fn connection(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(self.0).unwrap()
    }
    #[zbus(property)]
    fn state(&self) -> u32 {
        1
    }
    #[zbus(property)]
    fn devices(&self) -> Vec<OwnedObjectPath> {
        vec![]
    }
    #[zbus(property)]
    fn vpn(&self) -> bool {
        false
    }
}
#[tokio::test]
#[ignore = "requires isolated dbus-run-session; never use the system bus"]
async fn private_bus_editor_cancel_cleans_late_activation_and_preserves_row_operation() {
    let received = Arc::new(Mutex::new(CancellationState::default()));
    let nm = zbus::connection::Builder::session()
        .unwrap()
        .serve_at(ROOT, CancellationManager(received.clone()))
        .unwrap()
        .serve_at(
            "/org/freedesktop/NetworkManager/Settings",
            CancellationSettings,
        )
        .unwrap()
        .serve_at(
            "/test/saved",
            Saved(Arc::new(Mutex::new(Received::default()))),
        )
        .unwrap()
        .serve_at(
            "/test/editor_active",
            CancellationActive("/test/new_profile"),
        )
        .unwrap()
        .serve_at("/test/row_active", CancellationActive("/test/saved"))
        .unwrap()
        .name(NM)
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut service = start_with_connection(Some(Connection::session().await.unwrap()));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            service.updates.changed().await.unwrap();
            if service
                .updates
                .borrow()
                .profiles
                .iter()
                .any(|p| p.path == "/test/saved")
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    service
        .commands
        .send(Action::ConnectProfile {
            draft: ProfileDraft {
                name: "Fixture editor".into(),
                ssid: "Fixture editor".into(),
                password: "fixture-password".into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if received
                .lock()
                .unwrap()
                .calls
                .contains(&"editor-start".into())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    service
        .commands
        .send(Action::ActivateProfile {
            path: "/test/saved".into(),
            device: String::new(),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if received.lock().unwrap().calls.contains(&"row-start".into()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    service
        .commands
        .send(Action::Cancel("profile-editor".into()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let done = {
                let r = received.lock().unwrap();
                r.calls.contains(&"deactivate:/test/editor_active".into())
                    && r.active.contains(&"/test/row_active".into())
            };
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let r = received.lock().unwrap();
    assert_eq!(r.active, vec!["/test/row_active"]);
    assert!(!r.calls.contains(&"deactivate:/test/row_active".into()));
    drop(r);
    drop(service);
    drop(nm);
}

#[tokio::test]
#[ignore = "requires isolated dbus-run-session; never use the system bus"]
async fn private_bus_scan_permission_failure_is_not_reported_as_busy() {
    let bus = zbus::connection::Builder::session()
        .unwrap()
        .serve_at(ROOT, Manager(Arc::new(Mutex::new(Received::default()))))
        .unwrap()
        .serve_at("/test/device", WirelessCaps(0x40))
        .unwrap()
        .name(NM)
        .unwrap()
        .build()
        .await
        .unwrap();
    let state = Snapshot {
        wifi: true,
        devices: vec![Device {
            path: "/test/device".into(),
            kind: 2,
            managed: true,
            hardware: true,
            ..Default::default()
        }],
        ..Default::default()
    };
    let error = operations::apply(&bus, Action::Refresh, &state)
        .await
        .err()
        .unwrap();
    assert_eq!(
        action_error(&error),
        "Permission to change network settings was denied"
    );
}

#[tokio::test]
#[ignore = "requires isolated dbus-run-session; never use the system bus"]
async fn private_bus_wifi_off_preserves_inflight_wired_activation() {
    let received = Arc::new(Mutex::new(CancellationState::default()));
    let _nm = zbus::connection::Builder::session()
        .unwrap()
        .serve_at(ROOT, CancellationManager(received.clone()))
        .unwrap()
        .serve_at(
            "/org/freedesktop/NetworkManager/Settings",
            CancellationSettings,
        )
        .unwrap()
        .serve_at("/test/wired", CancellationWired)
        .unwrap()
        .serve_at("/test/wired_active", CancellationActive("/test/wired"))
        .unwrap()
        .name(NM)
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut service = start_with_connection(Some(Connection::session().await.unwrap()));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            service.updates.changed().await.unwrap();
            if service
                .updates
                .borrow()
                .profiles
                .iter()
                .any(|p| p.path == "/test/wired")
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    service
        .commands
        .send(Action::ActivateProfile {
            path: "/test/wired".into(),
            device: String::new(),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if received
                .lock()
                .unwrap()
                .calls
                .contains(&"wired-start".into())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    service.commands.send(Action::Wifi(false)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let done = {
                let r = received.lock().unwrap();
                r.calls.contains(&"wifi:false".into())
                    && r.active.contains(&"/test/wired_active".into())
            };
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let r = received.lock().unwrap();
    assert_eq!(r.active, vec!["/test/wired_active"]);
    assert!(!r.calls.contains(&"deactivate:/test/wired_active".into()));
}

#[tokio::test]
async fn vpn_import_timeout_kills_the_stuck_child() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("stuck-nmcli");
    std::fs::write(&program, "#!/bin/sh\nwhile :; do :; done\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let started = Instant::now();
    let error = operations::run_import_command(
        &program,
        "openvpn",
        "/nonexistent/fixture.ovpn",
        Duration::from_millis(200),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("timed out"));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the child was not killed"
    );
}

#[tokio::test]
async fn vpn_import_failure_does_not_forward_plugin_output() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("failing-nmcli");
    std::fs::write(
        &program,
        "#!/bin/sh\necho 'secret=fixture-value' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let error = operations::run_import_command(
        &program,
        "openvpn",
        "/nonexistent/fixture.ovpn",
        Duration::from_secs(10),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("could not be imported"));
    assert!(!error.to_string().contains("fixture-value"));
}
