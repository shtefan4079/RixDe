use rix_config::{OutputMode, OutputProfile};
use rix_ipc::{OutputInfo, OutputPreview, OutputState};
use smithay::{
    output::{Mode, Output},
    utils::Transform,
};
use std::time::{Duration, Instant};

pub(crate) const CONFIRM_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) fn mode(mode: Mode) -> OutputMode {
    OutputMode {
        width: mode.size.w as u32,
        height: mode.size.h as u32,
        refresh_millihz: mode.refresh as u32,
    }
}
pub(crate) fn profile(output: &Output) -> Option<OutputProfile> {
    Some(OutputProfile {
        mode: mode(output.current_mode()?),
        flipped: matches!(
            output.current_transform(),
            Transform::Flipped
                | Transform::Flipped90
                | Transform::Flipped180
                | Transform::Flipped270
        ),
        rotation: match output.current_transform() {
            Transform::_90 | Transform::Flipped270 => 90,
            Transform::_180 | Transform::Flipped => 180,
            Transform::_270 | Transform::Flipped90 => 270,
            _ => 0,
        },
        scale_percent: (output.current_scale().fractional_scale() * 100.).round() as u16,
        position: output
            .user_data()
            .get::<Placement>()
            .and_then(|p| *p.0.lock().unwrap()),
    })
}

#[derive(Default)]
pub(crate) struct Placement(pub std::sync::Mutex<Option<rix_config::OutputPosition>>);

pub(crate) fn transform(profile: &OutputProfile) -> Transform {
    match (profile.rotation, profile.flipped) {
        (90, false) => Transform::_90,
        (180, false) => Transform::_180,
        (270, false) => Transform::_270,
        (90, true) => Transform::Flipped270,
        (180, true) => Transform::Flipped,
        (270, true) => Transform::Flipped90,
        (_, true) => Transform::Flipped180,
        _ => Transform::Normal,
    }
}
pub(crate) fn identity(output: &Output) -> String {
    output
        .user_data()
        .get::<crate::monitor::Monitor>()
        .and_then(|m| m.identity.clone())
        .unwrap_or_else(|| output.name())
}

pub(crate) fn saved_profile(
    output: &Output,
    settings: &rix_config::DisplaySettings,
) -> Option<OutputProfile> {
    settings
        .outputs
        .get(&identity(output))
        .or_else(|| settings.outputs.get(&output.name()))
        .copied()
}

pub(crate) fn snapshot<'a>(
    outputs: impl Iterator<Item = &'a Output>,
    native: bool,
    pending: Option<OutputPreview>,
) -> OutputState {
    OutputState {
        native,
        pending,
        outputs: outputs
            .filter_map(|output| {
                let current = profile(output)?;
                let mut modes: Vec<_> = output.modes().into_iter().map(mode).collect();
                modes.sort_by_key(|m| {
                    (
                        std::cmp::Reverse(m.width),
                        std::cmp::Reverse(m.height),
                        std::cmp::Reverse(m.refresh_millihz),
                    )
                });
                modes.dedup();
                Some(OutputInfo {
                    name: output.name(),
                    identity: identity(output),
                    description: output
                        .user_data()
                        .get::<crate::monitor::Monitor>()
                        .map_or_else(
                            || output.name(),
                            |m| format!("{} {} · {}", m.make, m.model, output.name()),
                        ),
                    current,
                    preferred: output.preferred_mode().map(mode),
                    modes,
                })
            })
            .collect(),
    }
}

pub(crate) struct Trial {
    pub id: u64,
    pub output: Output,
    pub previous: OutputProfile,
    pub tested: OutputProfile,
    deadline: Instant,
}
impl Trial {
    pub fn new(
        id: u64,
        output: Output,
        previous: OutputProfile,
        tested: OutputProfile,
        now: Instant,
    ) -> Self {
        Self {
            id,
            output,
            previous,
            tested,
            deadline: now + CONFIRM_TIMEOUT,
        }
    }
    pub fn expired(&self, now: Instant) -> bool {
        now >= self.deadline
    }
    pub fn can_confirm(&self, id: u64, now: Instant) -> bool {
        id == self.id && !self.expired(now)
    }
    pub fn preview(&self, now: Instant) -> OutputPreview {
        OutputPreview {
            ready: false,
            id: self.id,
            output: self.output.name(),
            remaining_seconds: self
                .deadline
                .saturating_duration_since(now)
                .as_secs_f64()
                .ceil() as u32,
        }
    }
    pub fn revert(&mut self, now: Instant) {
        self.deadline = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::output::{PhysicalProperties, Subpixel};
    fn output() -> Output {
        let output = Output::new(
            "test".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "".into(),
                model: "".into(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: (1024, 768).into(),
                refresh: 59_940,
            }),
            None,
            None,
            None,
        );
        output
    }
    #[test]
    fn confirmation_is_fenced_by_id_and_compositor_deadline() {
        let now = Instant::now();
        let output = output();
        let profile = profile(&output).unwrap();
        let mut trial = Trial::new(8, output, profile, profile, now);
        assert!(!trial.can_confirm(7, now));
        assert!(trial.can_confirm(8, now + Duration::from_secs(14)));
        assert!(!trial.can_confirm(8, now + CONFIRM_TIMEOUT));
        assert!(trial.expired(now + CONFIRM_TIMEOUT));
        assert_eq!(
            trial
                .preview(now + Duration::from_millis(500))
                .remaining_seconds,
            15
        );
        trial.revert(now);
        assert!(!trial.can_confirm(8, now));
    }
    #[test]
    fn connector_profiles_preserve_legacy_config_and_each_other() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("display.json"),
            r#"{"width":1440,"height":900,"refresh_hz":75,"flipped":true}"#,
        )
        .unwrap();
        let config = std::sync::Arc::new(
            rix_config::ConfigManager::init_in(directory.path().to_path_buf()).unwrap(),
        );
        let legacy = config.get_display();
        assert!(legacy.outputs.is_empty());
        assert_eq!(
            (legacy.width, legacy.height, legacy.refresh_hz),
            (1440, 900, 75)
        );
        let profile = profile(&output()).unwrap();
        std::thread::scope(|scope| {
            for name in ["DP-test", "HDMI-test"] {
                let config = config.clone();
                scope.spawn(move || config.save_output_profile(name.into(), profile).unwrap());
            }
        });
        let saved = config.get_display();
        assert_eq!(saved.outputs.len(), 2);
        assert_eq!(saved.width, legacy.width);
        assert_eq!(saved.height, legacy.height);
        assert_eq!(saved.refresh_hz, legacy.refresh_hz);
        assert!(saved.flipped);
        let disk = rix_config::ConfigManager::init_in(directory.path().to_path_buf())
            .unwrap()
            .get_display();
        assert_eq!(saved, disk);
    }

    #[test]
    fn profile_transform_roundtrip_keeps_scale_position_and_legacy_flip() {
        let output = output();
        for rotation in [0, 90, 180, 270] {
            for flipped in [false, true] {
                let mut expected = profile(&output).unwrap();
                expected.scale_percent = 125;
                expected.rotation = rotation;
                expected.flipped = flipped;
                expected.position = Some(rix_config::OutputPosition { x: -800, y: 120 });
                output.user_data().insert_if_missing(Placement::default);
                *output
                    .user_data()
                    .get::<Placement>()
                    .unwrap()
                    .0
                    .lock()
                    .unwrap() = expected.position;
                output.change_current_state(
                    None,
                    Some(transform(&expected)),
                    Some(smithay::output::Scale::Fractional(1.25)),
                    None,
                );
                assert_eq!(profile(&output).unwrap(), expected);
            }
        }
        let legacy: OutputProfile = serde_json::from_str(
            r#"{"mode":{"width":800,"height":600,"refresh_millihz":60000},"flipped":true}"#,
        )
        .unwrap();
        assert_eq!(legacy.scale_percent, 100);
        assert_eq!(legacy.rotation, 0);
        assert_eq!(transform(&legacy), Transform::Flipped180);
        let mut invalid = legacy;
        invalid.scale_percent = 0;
        assert!(invalid.validate().is_err());
        invalid = legacy;
        invalid.position = Some(rix_config::OutputPosition { x: i32::MIN, y: 0 });
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn monitor_profile_lookup_preserves_connector_fallback() {
        let output = output();
        let mut settings = rix_config::DisplaySettings::default();
        let connector_profile = profile(&output).unwrap();
        settings.outputs.insert(output.name(), connector_profile);
        output
            .user_data()
            .insert_if_missing(|| crate::monitor::Monitor {
                make: "ABC".into(),
                model: "Test".into(),
                identity: Some("edid:ABC:0042:n1".into()),
            });
        assert_eq!(saved_profile(&output, &settings), Some(connector_profile));
        let mut monitor_profile = connector_profile;
        monitor_profile.scale_percent = 150;
        settings.outputs.insert(identity(&output), monitor_profile);
        assert_eq!(saved_profile(&output, &settings), Some(monitor_profile));
    }

    #[test]
    fn output_snapshot_preserves_fractional_refresh_rates() {
        let output = output();
        output.add_mode(Mode {
            size: (1024, 768).into(),
            refresh: 60_000,
        });
        let state = snapshot([&output].into_iter(), true, None);
        assert_eq!(state.outputs[0].modes.len(), 2);
        assert_eq!(state.outputs[0].current.mode.refresh_millihz, 59_940);
    }

    #[test]
    fn snapshot_skips_outputs_without_a_current_mode() {
        let bare = Output::new(
            "bare".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "".into(),
                model: "".into(),
            },
        );
        let healthy = output();
        let state = snapshot([&bare, &healthy].into_iter(), false, None);
        assert_eq!(state.outputs.len(), 1);
        assert_eq!(state.outputs[0].name, "test");
    }

    #[test]
    fn snapshot_reports_the_seeded_placement() {
        let output = output();
        output.user_data().insert_if_missing(|| {
            Placement(std::sync::Mutex::new(Some(rix_config::OutputPosition {
                x: 1920,
                y: -120,
            })))
        });
        let state = snapshot([&output].into_iter(), false, None);
        assert_eq!(
            state.outputs[0].current.position,
            Some(rix_config::OutputPosition { x: 1920, y: -120 })
        );
    }
}
