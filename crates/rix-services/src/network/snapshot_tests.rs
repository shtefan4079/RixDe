use super::*;
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};
use zbus::zvariant::{OwnedObjectPath, Value};
fn op(path: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(path).unwrap()
}
struct Manager;
#[zbus::interface(name = "org.freedesktop.NetworkManager")]
impl Manager {
    async fn enable(&self, _enabled: bool) {
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    #[zbus(property)]
    fn devices(&self) -> Vec<OwnedObjectPath> {
        vec![op("/test/device"), op("/test/removed_device")]
    }
    #[zbus(property)]
    fn wireless_enabled(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn wireless_hardware_enabled(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn connectivity(&self) -> u32 {
        4
    }
    #[zbus(property)]
    fn networking_enabled(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn active_connections(&self) -> Vec<OwnedObjectPath> {
        vec![op("/test/active")]
    }
    #[zbus(property)]
    fn primary_connection(&self) -> OwnedObjectPath {
        op("/test/active")
    }
    fn get_permissions(&self) -> HashMap<String, String> {
        HashMap::from([(
            "org.freedesktop.NetworkManager.enable-disable-wifi".into(),
            "yes".into(),
        )])
    }
}
struct AgentManager;
#[zbus::interface(name = "org.freedesktop.NetworkManager.AgentManager")]
impl AgentManager {
    fn register_with_capabilities(&self, _identifier: String, _capabilities: u32) {}
}
struct Device;
#[zbus::interface(name = "org.freedesktop.NetworkManager.Device")]
impl Device {
    #[zbus(property)]
    fn device_type(&self) -> u32 {
        2
    }
    #[zbus(property)]
    fn state(&self) -> u32 {
        50
    }
    #[zbus(property)]
    fn state_reason(&self) -> (u32, u32) {
        (50, 0)
    }
    #[zbus(property)]
    fn interface(&self) -> String {
        "fixture0".into()
    }
    #[zbus(property)]
    fn managed(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn firmware_missing(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn nm_plugin_missing(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn available_connections(&self) -> Vec<OwnedObjectPath> {
        vec![op("/test/profile2")]
    }
    #[zbus(property)]
    fn ip4_config(&self) -> OwnedObjectPath {
        op("/")
    }
    #[zbus(property)]
    fn ip6_config(&self) -> OwnedObjectPath {
        op("/")
    }
    #[zbus(property)]
    fn metered(&self) -> u32 {
        0
    }
}
struct Wireless;
#[zbus::interface(name = "org.freedesktop.NetworkManager.Device.Wireless")]
impl Wireless {
    #[zbus(property)]
    fn active_access_point(&self) -> OwnedObjectPath {
        op("/test/ap1")
    }
    #[zbus(property)]
    fn access_points(&self) -> Vec<OwnedObjectPath> {
        vec![op("/test/ap1"), op("/test/ap2"), op("/test/removed_ap")]
    }
    #[zbus(property)]
    fn wireless_capabilities(&self) -> u32 {
        0x40
    }
    #[zbus(property)]
    fn last_scan(&self) -> i64 {
        10
    }
}
struct AccessPoint(Arc<AtomicU8>);
#[zbus::interface(name = "org.freedesktop.NetworkManager.AccessPoint")]
impl AccessPoint {
    #[zbus(property)]
    fn ssid(&self) -> Vec<u8> {
        b"fixture".to_vec()
    }
    #[zbus(property)]
    fn flags(&self) -> u32 {
        1
    }
    #[zbus(property)]
    fn wpa_flags(&self) -> u32 {
        0
    }
    #[zbus(property)]
    fn rsn_flags(&self) -> u32 {
        0x500
    }
    #[zbus(property)]
    fn strength(&self) -> u8 {
        self.0.load(Ordering::SeqCst)
    }
    #[zbus(property)]
    fn hw_address(&self) -> String {
        "00:01:02:03:04:05".into()
    }
    #[zbus(property)]
    fn frequency(&self) -> u32 {
        2412
    }
}
struct SettingsList;
#[zbus::interface(name = "org.freedesktop.NetworkManager.Settings")]
impl SettingsList {
    fn list_connections(&self) -> Vec<OwnedObjectPath> {
        vec![op("/test/profile1"), op("/test/profile2")]
    }
}
struct Profile(&'static str);
#[zbus::interface(name = "org.freedesktop.NetworkManager.Settings.Connection")]
impl Profile {
    fn get_settings(&self) -> Settings {
        HashMap::from([
            (
                "connection".into(),
                HashMap::from([
                    ("id".into(), profiles::string(self.0)),
                    ("uuid".into(), profiles::string(self.0)),
                    ("type".into(), profiles::string("802-11-wireless")),
                ]),
            ),
            (
                "802-11-wireless".into(),
                HashMap::from([(
                    "ssid".into(),
                    Value::from(b"fixture".to_vec()).try_to_owned().unwrap(),
                )]),
            ),
            (
                "802-11-wireless-security".into(),
                HashMap::from([("key-mgmt".into(), profiles::string("sae"))]),
            ),
        ])
    }
}
struct Active;
#[zbus::interface(name = "org.freedesktop.NetworkManager.Connection.Active")]
impl Active {
    #[zbus(property)]
    fn connection(&self) -> OwnedObjectPath {
        op("/test/profile2")
    }
    #[zbus(property)]
    fn state(&self) -> u32 {
        1
    }
    #[zbus(property)]
    fn state_reason(&self) -> (u32, u32) {
        (1, 0)
    }
    #[zbus(property)]
    fn devices(&self) -> Vec<OwnedObjectPath> {
        vec![op("/test/device")]
    }
    #[zbus(property)]
    fn vpn(&self) -> bool {
        false
    }
}
#[tokio::test]
#[ignore = "requires isolated dbus-run-session; never use the system bus"]
async fn private_bus_snapshot_handles_hotplug_matching_pending_ap_and_stable_keys() {
    let selected = Arc::new(AtomicU8::new(40));
    let other = Arc::new(AtomicU8::new(80));
    let bus = zbus::connection::Builder::session()
        .unwrap()
        .serve_at(ROOT, Manager)
        .unwrap()
        .serve_at("/test/device", Device)
        .unwrap()
        .serve_at("/test/device", Wireless)
        .unwrap()
        .serve_at("/test/ap1", AccessPoint(selected.clone()))
        .unwrap()
        .serve_at("/test/ap2", AccessPoint(other.clone()))
        .unwrap()
        .serve_at("/org/freedesktop/NetworkManager/Settings", SettingsList)
        .unwrap()
        .serve_at("/test/profile1", Profile("profile1"))
        .unwrap()
        .serve_at("/test/profile2", Profile("profile2"))
        .unwrap()
        .serve_at("/test/active", Active)
        .unwrap()
        .name(NM)
        .unwrap()
        .build()
        .await
        .unwrap();
    let snapshot = snapshot(&bus).await.unwrap();
    assert!(snapshot.available);
    assert_eq!(snapshot.devices.len(), 1);
    assert_eq!(snapshot.profiles.len(), 2);
    let row = snapshot
        .networks
        .iter()
        .find(|n| !n.ap_path.is_empty())
        .unwrap();
    assert_eq!(
        row.ap_path, "/test/ap1",
        "The selected AP must win even while connecting with a weaker signal"
    );
    assert_eq!(
        row.profile_path, "/test/profile2",
        "Ignore the same-SSID profile unavailable through this adapter"
    );
    assert_eq!(row.state, 50);
    assert!(!row.active);
    assert!(
        snapshot
            .networks
            .iter()
            .any(|n| n.profile_path == "/test/profile1" && n.ap_path.is_empty()),
        "Offline saved profiles remain manageable"
    );
    let key = row.path.clone();
    other.store(95, Ordering::SeqCst);
    selected.store(15, Ordering::SeqCst);
    let next = super::snapshot(&bus).await.unwrap();
    let row = next.networks.iter().find(|n| n.path == key).unwrap();
    assert_eq!(row.ap_path, "/test/ap1");
    assert_eq!(row.strength, 15);
}

#[tokio::test]
#[ignore = "requires isolated dbus-run-session; never use the system bus"]
async fn private_bus_service_answers_secrets_during_slow_operation_and_recovers_nm_restart() {
    let nm = zbus::connection::Builder::session()
        .unwrap()
        .serve_at(ROOT, Manager)
        .unwrap()
        .serve_at("/org/freedesktop/NetworkManager/Settings", SettingsList)
        .unwrap()
        .serve_at("/org/freedesktop/NetworkManager/AgentManager", AgentManager)
        .unwrap()
        .name(NM)
        .unwrap()
        .build()
        .await
        .unwrap();
    let client = Connection::session().await.unwrap();
    let destination = client.unique_name().unwrap().as_str().to_string();
    let mut service = start_with_connection(Some(client));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            service.updates.changed().await.unwrap();
            if service.updates.borrow().available {
                break;
            }
        }
    })
    .await
    .unwrap();
    service
        .commands
        .send(Action::Networking(false))
        .await
        .unwrap();
    let nm_call = nm.clone();
    let value: Settings = HashMap::from([
        (
            "connection".into(),
            HashMap::from([("id".into(), profiles::string("fixture"))]),
        ),
        (
            "802-11-wireless-security".into(),
            HashMap::from([("psk-flags".into(), OwnedValue::from(1u32))]),
        ),
    ]);
    let call = tokio::spawn(async move {
        Proxy::new(
            &nm_call,
            destination,
            "/org/freedesktop/NetworkManager/SecretAgent",
            "org.freedesktop.NetworkManager.SecretAgent",
        )
        .await
        .unwrap()
        .call::<_, _, Settings>(
            "GetSecrets",
            &(
                value,
                op("/test/profile"),
                "802-11-wireless-security",
                vec!["psk"],
                1u32,
            ),
        )
        .await
    });
    let id = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            service.updates.changed().await.unwrap();
            if let Some(request) = service.updates.borrow().credentials.first() {
                break request.id;
            }
        }
    })
    .await
    .unwrap();
    let now = Instant::now();
    service
        .commands
        .send(Action::ReplySecrets {
            id,
            username: String::new(),
            password: "fixture-secret".into(),
            private_key_password: String::new(),
            extra: String::new(),
            cancel: false,
        })
        .await
        .unwrap();
    let answer = tokio::time::timeout(Duration::from_millis(700), call)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(now.elapsed() < Duration::from_millis(700));
    assert_eq!(
        profiles::str_setting(&answer, "802-11-wireless-security", "psk"),
        Some("fixture-secret")
    );
    nm.release_name(NM).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            service.updates.changed().await.unwrap();
            let state = service.updates.borrow();
            if !state.available && !state.error.is_empty() {
                assert!(state.networks.is_empty());
                break;
            }
        }
    })
    .await
    .unwrap();
    nm.request_name(NM).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            service.updates.changed().await.unwrap();
            if service.updates.borrow().available {
                break;
            }
        }
    })
    .await
    .unwrap();
}
