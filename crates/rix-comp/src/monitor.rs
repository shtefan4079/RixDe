use smithay::reexports::drm;
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Monitor {
    pub make: String,
    pub model: String,
    pub identity: Option<String>,
}

impl Monitor {
    pub fn from_edid(bytes: &[u8]) -> Option<Self> {
        let base = bytes.get(..128)?;
        if base[..8] != [0, 255, 255, 255, 255, 255, 255, 0]
            || base.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) != 0
        {
            return None;
        }
        let manufacturer = u16::from_be_bytes([base[8], base[9]]);
        let mut make = String::new();
        for shift in [10, 5, 0] {
            let letter = ((manufacturer >> shift) & 31) as u8;
            if !(1..=26).contains(&letter) {
                return None;
            }
            make.push(char::from(b'A' + letter - 1));
        }
        let product = u16::from_le_bytes([base[10], base[11]]);
        let serial = u32::from_le_bytes(base[12..16].try_into().ok()?);
        let mut model = format!("{product:04X}");
        let mut text_serial = None;
        for descriptor in base[54..126].chunks_exact(18) {
            if descriptor[..3] != [0, 0, 0] || descriptor[4] != 0 {
                continue;
            }
            let text: String = descriptor[5..18]
                .iter()
                .take_while(|&&b| b != 0 && b != b'\n')
                .filter(|&&b| (32..=126).contains(&b))
                .map(|&b| char::from(b))
                .collect();
            let text = text.trim();
            match descriptor[3] {
                0xfc if !text.is_empty() => model = text.into(),
                0xff if text.chars().any(|c| c.is_ascii_alphanumeric() && c != '0') => {
                    text_serial = Some(
                        text.as_bytes()
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<String>(),
                    );
                }
                _ => {}
            }
        }

        let serial = if serial != 0 && serial != u32::MAX {
            Some(format!("n{serial:08x}"))
        } else {
            text_serial.map(|s| format!("s{s}"))
        };
        let identity = serial.map(|s| format!("edid:{make}:{product:04x}:{s}"));
        Some(Self {
            make,
            model,
            identity,
        })
    }

    pub fn read(
        device: &impl drm::control::Device,
        connector: drm::control::connector::Handle,
    ) -> Option<Self> {
        let properties = device.get_properties(connector).ok()?;
        let (handles, values) = properties.as_props_and_values();
        for (&handle, &value) in handles.iter().zip(values) {
            let info = device.get_property(handle).ok()?;
            if info.name().to_bytes() != b"EDID" {
                continue;
            }
            if let drm::control::property::Value::Blob(blob) =
                info.value_type().convert_value(value)
            {
                return Self::from_edid(&device.get_property_blob(blob).ok()?);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn edid(serial: u32) -> [u8; 128] {
        let mut bytes = [0u8; 128];
        bytes[..8].copy_from_slice(&[0, 255, 255, 255, 255, 255, 255, 0]);

        bytes[8..10].copy_from_slice(&((1u16 << 10) | (2 << 5) | 3).to_be_bytes());
        bytes[10..12].copy_from_slice(&42u16.to_le_bytes());
        bytes[12..16].copy_from_slice(&serial.to_le_bytes());
        bytes[57] = 0xfc;
        bytes[59..72].copy_from_slice(b"Test Monitor\n");
        checksum(&mut bytes);
        bytes
    }
    fn checksum(bytes: &mut [u8; 128]) {
        bytes[127] = 0u8.wrapping_sub(bytes[..127].iter().fold(0u8, |s, b| s.wrapping_add(*b)));
    }
    #[test]
    fn monitor_identity_survives_connector_changes_and_distinguishes_serials() {
        let a = Monitor::from_edid(&edid(100)).unwrap();
        assert_eq!(a.make, "ABC");
        assert_eq!(a.model, "Test Monitor");
        assert_eq!(a.identity.as_deref(), Some("edid:ABC:002a:n00000064"));
        assert_ne!(a.identity, Monitor::from_edid(&edid(101)).unwrap().identity);
        assert!(Monitor::from_edid(&edid(0)).unwrap().identity.is_none());
    }
    #[test]
    fn malformed_edid_is_not_used_for_persistence() {
        let mut bytes = edid(100);
        assert!(Monitor::from_edid(&bytes[..127]).is_none());
        bytes[20] ^= 1;
        assert!(Monitor::from_edid(&bytes).is_none());
        checksum(&mut bytes);
        bytes[0] = 42;
        checksum(&mut bytes);
        assert!(Monitor::from_edid(&bytes).is_none());
        bytes = edid(100);
        bytes[8] = 0;
        checksum(&mut bytes);
        assert!(Monitor::from_edid(&bytes).is_none());
    }
    #[test]
    fn descriptor_serial_is_bounded_and_cannot_inject_a_profile_path() {
        let mut bytes = edid(0);
        bytes[75] = 0xff;
        bytes[77..90].copy_from_slice(b"123/../A\n    ");
        checksum(&mut bytes);
        let monitor = Monitor::from_edid(&bytes).unwrap();
        assert_eq!(
            monitor.identity.as_deref(),
            Some("edid:ABC:002a:s3132332f2e2e2f41")
        );
    }
}
