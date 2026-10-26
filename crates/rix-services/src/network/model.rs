use tokio::sync::{mpsc, watch};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Network {

    pub path: String,
    pub device: String,
    pub name: String,
    pub ssid: Vec<u8>,
    pub strength: u8,
    pub security: String,
    pub state: u32,
    pub state_reason: u32,
    pub active: bool,
    pub saved: String,
    pub address: String,
    pub wired: bool,
    pub ap_path: String,
    pub profile_path: String,
    pub uuid: String,
    pub iface: String,
    pub available: bool,
    pub ipv6: String,
    pub device_kind: u32,
    pub frequency: u32,
    pub bssid: String,
    pub metered: u32,
    pub error: String,
    pub default_route: bool,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Device {
    pub path: String,
    pub iface: String,
    pub kind: u32,
    pub state: u32,
    pub reason: u32,
    pub managed: bool,
    pub hardware: bool,
    pub scanning: bool,
    pub available: bool,
    pub error: String,
    pub last_scan: i64,

    pub virtual_device: bool,
}
impl Device {
    pub fn user_visible(&self) -> bool {
        self.managed
            && self.iface != "lo"
            && !self.virtual_device
            && matches!(self.kind, 1 | 2 | 8 | 16 | 29)
    }
    pub fn physical_uplink(&self) -> bool {
        self.user_visible() && self.state == 100 && matches!(self.kind, 1 | 2 | 8)
    }
}
pub(super) fn virtual_interface(kind: u32, iface: &str, udi: Option<&str>) -> bool {
    iface == "lo"
        || kind == 32
        || (matches!(kind, 1 | 2 | 8)
            && udi.is_some_and(|value| value.starts_with("/sys/devices/virtual/net/")))
        || iface.starts_with("veth")
        || iface.starts_with("docker")
        || iface.starts_with("virbr")
        || iface.starts_with("br-")
        || iface.starts_with("cni")
        || iface.starts_with("flannel")
}

#[derive(Clone, PartialEq)]
pub struct ProfileDraft {
    pub path: String,
    pub name: String,
    pub uuid: String,
    pub device: String,
    pub kind: String,
    pub ssid: String,
    pub security: String,
    pub password: String,
    pub username: String,
    pub anonymous_identity: String,
    pub eap: String,
    pub phase2_auth: String,
    pub ca_cert: String,
    pub client_cert: String,
    pub private_key: String,
    pub private_key_password: String,
    pub ipv4_method: String,
    pub ipv4_addresses: String,
    pub ipv4_gateway: String,
    pub ipv4_dns: String,
    pub ipv4_routes: String,
    pub ipv6_method: String,
    pub ipv6_addresses: String,
    pub ipv6_gateway: String,
    pub ipv6_dns: String,
    pub ipv6_routes: String,
    pub mac_address: String,
    pub band: String,
    pub bssid: String,
    pub autoconnect: bool,
    pub remember: bool,
    pub shared: bool,
    pub hidden: bool,
    pub metered: u32,
    pub powersave: u32,
    pub vpn_service: String,
    pub vpn_data: String,
    pub vpn_secrets: String,
    pub apn: String,
    pub number: String,
    pub pin: String,
    pub proxy_method: String,
    pub proxy_pac_url: String,
    pub proxy_pac_script: String,
    pub priority: i32,
    pub mtu: u32,
    pub domain_suffix_match: String,
}
impl Default for ProfileDraft {
    fn default() -> Self {
        Self {
            path: String::new(),
            name: String::new(),
            uuid: String::new(),
            device: String::new(),
            kind: "802-11-wireless".into(),
            ssid: String::new(),
            security: "wpa-psk".into(),
            password: String::new(),
            username: String::new(),
            anonymous_identity: String::new(),
            eap: "peap".into(),
            phase2_auth: "mschapv2".into(),
            ca_cert: String::new(),
            client_cert: String::new(),
            private_key: String::new(),
            private_key_password: String::new(),
            ipv4_method: "auto".into(),
            ipv4_addresses: String::new(),
            ipv4_gateway: String::new(),
            ipv4_dns: String::new(),
            ipv4_routes: String::new(),
            ipv6_method: "auto".into(),
            ipv6_addresses: String::new(),
            ipv6_gateway: String::new(),
            ipv6_dns: String::new(),
            ipv6_routes: String::new(),
            mac_address: String::new(),
            band: String::new(),
            bssid: String::new(),
            autoconnect: true,
            remember: true,
            shared: false,
            hidden: false,
            metered: 0,
            powersave: 0,
            vpn_service: String::new(),
            vpn_data: String::new(),
            vpn_secrets: String::new(),
            apn: String::new(),
            number: String::new(),
            pin: String::new(),
            proxy_method: "none".into(),
            proxy_pac_url: String::new(),
            proxy_pac_script: String::new(),
            priority: 0,
            mtu: 0,
            domain_suffix_match: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CredentialRequest {
    pub id: u64,
    pub profile_path: String,
    pub profile_name: String,
    pub setting_name: String,
    pub hints: Vec<String>,
    pub message: String,
    pub username: bool,
    pub password: bool,
    pub private_key_password: bool,
    pub retry: bool,
}
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Activation {
    pub path: String,
    pub profile: String,
    pub device: String,
    pub state: u32,
    pub reason: u32,
}
#[derive(Clone, PartialEq, Default)]
pub struct Snapshot {
    pub networks: Vec<Network>,
    pub wifi: bool,
    pub hardware: bool,
    pub connectivity: u32,
    pub pending: String,
    pub error: String,
    pub available: bool,
    pub devices: Vec<Device>,
    pub profiles: Vec<ProfileDraft>,
    pub scanning: bool,
    pub permissions: Vec<(String, String)>,
    pub credentials: Vec<CredentialRequest>,
    pub secret_agent: bool,
    pub networking: bool,
    pub portal_uri: String,
    pub operation_id: u64,
    pub operation_kind: String,
    pub operation_target: String,
    pub operation_done: bool,
    pub operation_error: String,
    pub primary_device: String,
    pub activations: Vec<Activation>,
    pub vpn_active: bool,
    pub traffic: Traffic,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Traffic {
    pub device: String,
    pub download_bytes_per_second: u64,
    pub upload_bytes_per_second: u64,
    pub ready: bool,
}

pub enum Action {
    Refresh,
    Wifi(bool),
    Networking(bool),
    Connect {
        path: String,
        password: String,
        remember: bool,
    },
    Disconnect(String),
    Forget(String),
    Cancel(String),

    CancelActive {
        active: Vec<String>,
        devices: Vec<String>,
    },
    Hidden(ProfileDraft),
    Hotspot(ProfileDraft),
    SaveProfile(ProfileDraft),
    ConnectProfile {
        draft: ProfileDraft,
    },
    ActivateProfile {
        path: String,
        device: String,
    },
    CheckConnectivity,
    ResetError,
    ImportProfile {
        path: String,
        kind: String,
    },
    ReplySecrets {
        id: u64,
        username: String,
        password: String,
        private_key_password: String,
        extra: String,
        cancel: bool,
    },
}
pub struct Service {
    pub updates: watch::Receiver<Snapshot>,
    pub commands: mpsc::Sender<Action>,
    pub visible: watch::Sender<bool>,
}
