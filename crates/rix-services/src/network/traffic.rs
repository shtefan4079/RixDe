use super::{Snapshot, Traffic};
use std::{
    fs,
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
pub(super) struct Counters {
    pub rx: u64,
    pub tx: u64,
    pub at: Instant,
}
#[derive(Default)]
pub(super) struct Sampler {
    previous: Option<(String, Counters)>,
}
impl Sampler {
    pub fn reset(&mut self) {
        self.previous = None;
    }
    pub fn update(&mut self, iface: String, counters: Option<Counters>) -> Traffic {
        let Some(counters) = counters else {
            self.reset();
            return Traffic::default();
        };
        let previous = self.previous.replace((iface.clone(), counters));
        let mut traffic = Traffic {
            device: iface.clone(),
            ..Traffic::default()
        };
        if let Some((previous_iface, before)) = previous {
            let elapsed = counters.at.saturating_duration_since(before.at);
            if previous_iface == iface
                && (Duration::from_millis(200)..=Duration::from_secs(5)).contains(&elapsed)
            {
                if let (Some(rx), Some(tx)) = (
                    counters.rx.checked_sub(before.rx),
                    counters.tx.checked_sub(before.tx),
                ) {
                    let per_second = |bytes: u64| {
                        ((bytes as u128 * 1_000_000_000) / elapsed.as_nanos()).min(u64::MAX as u128)
                            as u64
                    };
                    traffic.download_bytes_per_second = per_second(rx);
                    traffic.upload_bytes_per_second = per_second(tx);
                    traffic.ready = true;
                }
            }
        }
        traffic
    }
}
pub(super) fn interface(snapshot: &Snapshot) -> Option<String> {
    if !snapshot.available || !snapshot.networking {
        return None;
    }
    let uplink = |device: &&super::Device| device.physical_uplink();
    snapshot
        .devices
        .iter()
        .filter(uplink)
        .find(|device| device.path == snapshot.primary_device)
        .or_else(|| {
            snapshot
                .networks
                .iter()
                .filter(|network| network.active && network.default_route)
                .find_map(|network| {
                    snapshot
                        .devices
                        .iter()
                        .find(|device| device.path == network.device && device.physical_uplink())
                })
        })
        .or_else(|| {
            snapshot
                .devices
                .iter()
                .filter(uplink)
                .min_by(|a, b| a.iface.cmp(&b.iface))
        })
        .map(|device| device.iface.clone())
}
pub(super) fn read(root: &Path, iface: &str) -> io::Result<Counters> {
    if iface.is_empty()
        || iface.len() > 15
        || iface == "."
        || iface == ".."
        || !iface
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let counter = |name: &str| -> io::Result<u64> {
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_NOFOLLOW)
            .open(root.join(iface).join("statistics").join(name))?;
        if !file.metadata()?.is_file() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut value = String::new();
        file.take(33).read_to_string(&mut value)?;
        if value.len() > 32 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        value
            .trim()
            .parse()
            .map_err(|_| io::ErrorKind::InvalidData.into())
    };
    Ok(Counters {
        rx: counter("rx_bytes")?,
        tx: counter("tx_bytes")?,
        at: Instant::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn device_classification_keeps_real_adapters_and_vpn_but_hides_container_plumbing() {
        use super::super::model::virtual_interface;
        for name in [
            "lo",
            "veth77",
            "docker0",
            "virbr0",
            "br-abcdef",
            "cni0",
            "flannel.1",
        ] {
            assert!(virtual_interface(1, name, None), "{name}");
        }
        assert!(virtual_interface(
            1,
            "eth0",
            Some("/sys/devices/virtual/net/eth0")
        ));
        assert!(!virtual_interface(
            1,
            "enp3s0",
            Some("/sys/devices/pci0000:00/net/enp3s0")
        ));
        assert!(!virtual_interface(2, "wlan0", None));
        assert!(!virtual_interface(
            29,
            "wg0",
            Some("/sys/devices/virtual/net/wg0")
        ));
        assert!(!virtual_interface(16, "tun0", None));
    }
    #[test]
    fn monotonic_rates_reset_on_counter_reset_adapter_change_and_stale_samples() {
        let mut sampler = Sampler::default();
        let start = Instant::now();
        let count = |rx, tx, at| Some(Counters { rx, tx, at });
        assert!(!sampler.update("eth0".into(), count(100, 200, start)).ready);
        let result = sampler.update(
            "eth0".into(),
            count(1124, 225, start + Duration::from_secs(1)),
        );
        assert!(result.ready);
        assert_eq!(result.download_bytes_per_second, 1024);
        assert_eq!(result.upload_bytes_per_second, 25);
        assert!(
            !sampler
                .update("eth0".into(), count(0, 0, start + Duration::from_secs(2)))
                .ready
        );
        assert!(
            !sampler
                .update(
                    "wlan0".into(),
                    count(50, 50, start + Duration::from_secs(3))
                )
                .ready
        );
        assert!(
            !sampler
                .update(
                    "wlan0".into(),
                    count(150, 150, start + Duration::from_secs(10))
                )
                .ready
        );
        assert!(!sampler.update("wlan0".into(), None).ready);
    }
    #[test]
    fn selection_ignores_virtual_loopback_unmanaged_and_tunnel_double_counting() {
        let mut snapshot = Snapshot {
            available: true,
            networking: true,
            primary_device: "lo".into(),
            ..Default::default()
        };
        snapshot.devices = vec![
            super::super::Device {
                path: "lo".into(),
                iface: "lo".into(),
                kind: 32,
                managed: true,
                state: 100,
                ..Default::default()
            },
            super::super::Device {
                path: "veth".into(),
                iface: "veth0".into(),
                kind: 1,
                managed: true,
                state: 100,
                virtual_device: true,
                ..Default::default()
            },
            super::super::Device {
                path: "vpn".into(),
                iface: "wg0".into(),
                kind: 29,
                managed: true,
                state: 100,
                ..Default::default()
            },
            super::super::Device {
                path: "ignored".into(),
                iface: "eth1".into(),
                kind: 1,
                managed: false,
                state: 100,
                ..Default::default()
            },
            super::super::Device {
                path: "wifi".into(),
                iface: "wlan0".into(),
                kind: 2,
                managed: true,
                state: 100,
                ..Default::default()
            },
        ];
        assert_eq!(interface(&snapshot).as_deref(), Some("wlan0"));
        assert!(
            snapshot.devices[2].user_visible(),
            "VPN remains visible but is not counted twice"
        );
        snapshot.networking = false;
        assert_eq!(interface(&snapshot), None);
    }
    #[test]
    fn bounded_sysfs_fixture_rejects_bad_names_and_malformed_counters() {
        let directory = tempfile::tempdir().unwrap();
        let stats = directory.path().join("eth0/statistics");
        fs::create_dir_all(&stats).unwrap();
        fs::write(stats.join("rx_bytes"), "1234\n").unwrap();
        fs::write(stats.join("tx_bytes"), "456\n").unwrap();
        let result = read(directory.path(), "eth0").unwrap();
        assert_eq!(result.rx, 1234);
        assert_eq!(result.tx, 456);
        assert!(read(directory.path(), "../eth0").is_err());
        fs::write(stats.join("rx_bytes"), "9".repeat(100)).unwrap();
        assert!(read(directory.path(), "eth0").is_err());
    }
}
