use super::*;
use rix_services::plugins::Manifest;
use std::path::PathBuf;
use wayland_server::Display;

const ID: &str = "org.example.capture-test";

struct Fixture {
    _directory: tempfile::TempDir,
    counter: PathBuf,
    _display: Display<()>,
    handle: DisplayHandle,
    manager: PluginManager,
    settings: rix_config::DesktopSettings,
    now: Instant,
}

impl Fixture {
    fn new(exits: u32) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let counter = directory.path().join("starts");
        let script = format!(
            "count=0; if [ -f \"$1\" ]; then read -r count < \"$1\"; fi; \
             count=$((count + 1)); printf '%s\\n' \"$count\" > \"$1\"; \
             if [ \"$count\" -le {exits} ]; then exit 0; fi; exec sleep 60"
        );
        let plugin = Plugin {
            manifest: Manifest {
                api: 1,
                id: ID.into(),
                name: "Disposable capture extension".into(),
                description: "Plugin supervision regression fixture".into(),
                exec: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    script,
                    "rix-plugin-test".into(),
                    counter.to_string_lossy().into_owned(),
                ],
                requires: Vec::new(),
                capabilities: vec![Capability::ScreenCapture],
                settings: Vec::new(),
                session_bus: false,
            },
            directory: directory.path().into(),
            unavailable: Vec::new(),
        };
        let display = Display::<()>::new().unwrap();
        let handle = display.handle();
        let settings = rix_config::DesktopSettings {
            enabled_plugins: vec![ID.into()],
            ..Default::default()
        };
        let now = Instant::now();
        let mut manager = PluginManager::default();
        manager.slots.insert(
            ID.into(),
            Slot {
                capture_fps: settings.screen_cast_fps,
                plugin,
                installed: true,
                enabled: true,
                running: None,
                failures: 0,
                next_start: now,
                restart_suppressed: false,
                start_blocked: false,
                message: String::new(),
            },
        );
        Self {
            _directory: directory,
            counter,
            _display: display,
            handle,
            manager,
            settings,
            now,
        }
    }
    fn tick(&mut self) {
        self.manager.tick_running(
            &self.handle,
            "disposable-plugin-test",
            &self.settings,
            self.now,
        );
    }
    fn slot(&self) -> &Slot {
        &self.manager.slots[ID]
    }
    fn starts(&self) -> u32 {
        std::fs::read_to_string(&self.counter)
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0)
    }
    fn wait_for_start(&self, expected: u32) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.starts() != expected {
            assert!(
                Instant::now() < deadline,
                "fixture did not start {expected} times"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn wait_for_exit(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let running = self
                .manager
                .slots
                .get_mut(ID)
                .unwrap()
                .running
                .as_mut()
                .unwrap();
            if running.child.try_wait().unwrap().is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "fixture did not exit");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn deliver_catalog(&mut self, plugin: Plugin) {
        self.deliver_catalog_entries(vec![plugin]);
    }
    fn deliver_catalog_entries(&mut self, plugins: Vec<Plugin>) {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        sender.send(plugins).unwrap();
        self.manager.discovery = Some(receiver);
        self.manager
            .tick(&self.handle, "disposable-plugin-test", &self.settings);
    }
}

#[test]
fn transient_exits_recover_after_more_than_three_failures() {
    let mut fixture = Fixture::new(4);
    for count in 1..=4 {
        fixture.tick();
        fixture.wait_for_start(count);
        let running = fixture.slot().running.as_ref().unwrap();
        assert!(
            running
                .client
                .get_data::<ClientState>()
                .unwrap()
                .allow_capture
        );
        fixture.wait_for_exit();
        fixture.tick();
        assert!(fixture.slot().running.is_none());
        assert_eq!(fixture.slot().failures, count);
        let retry = fixture.slot().next_start;
        fixture.now = retry - Duration::from_millis(1);
        fixture.tick();
        assert!(
            fixture.slot().running.is_none(),
            "retry must respect its backoff"
        );
        fixture.now = retry;
    }
    fixture.tick();
    fixture.wait_for_start(5);
    assert!(fixture.manager.statuses()[0].running);
    fixture.now += Duration::from_secs(31);
    fixture.tick();
    assert_eq!(
        fixture.slot().failures,
        0,
        "healthy uptime must reset crash-loop backoff"
    );
}

#[test]
fn explicit_capture_stop_does_not_retry_if_persistence_fails() {
    let mut fixture = Fixture::new(0);
    fixture.tick();
    fixture.wait_for_start(1);
    assert_eq!(fixture.manager.stop_capture(&fixture.handle), vec![ID]);
    fixture.wait_for_exit();

    fixture.now += Duration::from_secs(300);
    fixture.tick();
    assert!(fixture.slot().running.is_none());
    assert!(fixture.slot().restart_suppressed);
    assert_eq!(fixture.starts(), 1);
    assert_eq!(fixture.slot().message, "Screen sharing stopped by the user");

    let mut updated = fixture.slot().plugin.clone();
    updated.manifest.description.push_str(" updated");
    fixture.deliver_catalog_entries(Vec::new());
    assert!(!fixture.slot().installed);
    assert!(fixture.slot().restart_suppressed);
    fixture.deliver_catalog(updated);
    assert!(fixture.slot().installed);
    assert!(fixture.slot().restart_suppressed);
    assert!(fixture.slot().running.is_none());
    assert_eq!(fixture.starts(), 1);
    assert_eq!(fixture.slot().message, "Screen sharing stopped by the user");
}

#[test]
fn disabling_a_failed_plugin_prevents_its_scheduled_retry() {
    let mut fixture = Fixture::new(1);
    fixture.tick();
    fixture.wait_for_start(1);
    fixture.wait_for_exit();
    fixture.tick();
    fixture.settings.enabled_plugins.clear();
    fixture.now += Duration::from_secs(300);
    fixture.tick();
    assert!(fixture.manager.slots.is_empty());
    assert_eq!(fixture.starts(), 1);
}

#[test]
fn explicit_off_on_grants_restart_after_capture_revocation() {
    let mut fixture = Fixture::new(0);
    let plugin = fixture.slot().plugin.clone();
    fixture.tick();
    fixture.wait_for_start(1);
    fixture.manager.stop_capture(&fixture.handle);

    fixture.settings.enabled_plugins.clear();
    fixture.tick();
    fixture.settings.enabled_plugins = vec![ID.into()];
    fixture.tick();

    if !fixture.manager.slots.contains_key(ID) {
        fixture.deliver_catalog(plugin);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while fixture.starts() < 2 {
        fixture.tick();
        assert!(
            Instant::now() < deadline,
            "explicit off/on did not restart capture"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(!fixture.slot().restart_suppressed);
    assert!(fixture.manager.statuses()[0].running);
}

#[test]
fn transient_spawn_failures_keep_retrying_with_capped_backoff() {
    let mut fixture = Fixture::new(0);
    fixture
        .manager
        .slots
        .get_mut(ID)
        .unwrap()
        .plugin
        .manifest
        .exec[0] = fixture
        ._directory
        .path()
        .join("not-created-yet")
        .to_string_lossy()
        .into_owned();
    for count in 1..=8 {
        fixture.tick();
        assert!(fixture.slot().running.is_none());
        assert_eq!(fixture.slot().failures, count);
        assert_eq!(
            fixture.slot().next_start - fixture.now,
            retry_backoff(count)
        );
        assert!(!fixture.slot().start_blocked);
        fixture.now = fixture.slot().next_start;
    }
    assert_eq!(retry_backoff(u32::MAX), Duration::from_secs(32));
    fixture.manager.slots.get_mut(ID).unwrap().failures = u32::MAX;
    fixture.tick();
    assert_eq!(fixture.slot().failures, u32::MAX);
    assert_eq!(
        fixture.slot().next_start - fixture.now,
        Duration::from_secs(32)
    );
}

#[test]
fn missing_dependencies_block_launch_until_the_catalog_changes() {
    let mut fixture = Fixture::new(0);
    fixture
        .manager
        .slots
        .get_mut(ID)
        .unwrap()
        .plugin
        .unavailable = vec!["missing-helper".into()];
    for _ in 0..4 {
        fixture.tick();
        fixture.now += Duration::from_secs(60);
    }
    assert!(fixture.slot().start_blocked);
    assert_eq!(fixture.slot().message, "Missing: missing-helper");
    assert_eq!(fixture.slot().failures, 0);
    assert_eq!(fixture.starts(), 0);
    let mut installed = fixture.slot().plugin.clone();
    installed.unavailable.clear();
    fixture.deliver_catalog(installed);
    fixture.wait_for_start(1);
    assert!(!fixture.slot().start_blocked);
    assert!(fixture.manager.statuses()[0].running);
}

#[test]
fn remote_input_is_granted_only_to_the_enabled_extensions_private_client() {
    let mut fixture = Fixture::new(0);
    fixture
        .manager
        .slots
        .get_mut(ID)
        .unwrap()
        .plugin
        .manifest
        .capabilities
        .push(Capability::RemoteInput);
    fixture.tick();
    fixture.wait_for_start(1);
    assert!(!fixture.slot().start_blocked);
    assert!(
        fixture
            .slot()
            .running
            .as_ref()
            .unwrap()
            .client
            .get_data::<ClientState>()
            .unwrap()
            .allow_remote_input
    );
    assert_eq!(fixture.slot().failures, 0);
    assert!(!ClientState::default().allow_remote_input);
}
