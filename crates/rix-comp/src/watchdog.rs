#[cfg(test)]
use std::fs;
use std::{
    fs::OpenOptions,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy)]
#[repr(u8)]
pub(crate) enum Phase {
    Startup,
    LoopStart,
    EventDispatch,
    WaylandDispatch,
    OutputConfiguration,
    CapturePoll,
    GpuImport,
    WindowCommands,
    DesktopCommands,
    LayoutRefresh,
    ClientFlush,
    PresentationControl,
    SceneCompose,
    CaptureCompose,
    GpuRender,
    PageFlipQueue,
    OutputRecovery,
    FrameCallbacks,
    LoopComplete,
    SeatInitialization,
    GpuInitialization,
    OutputInitialization,
}
impl Phase {
    fn name(value: u8) -> &'static str {
        match value {
            0 => "startup",
            1 => "loop_start",
            2 => "event_dispatch",
            3 => "wayland_dispatch",
            4 => "output_configuration",
            5 => "capture_poll",
            6 => "gpu_import",
            7 => "window_commands",
            8 => "desktop_commands",
            9 => "layout_refresh",
            10 => "client_flush",
            11 => "presentation_control",
            12 => "scene_compose",
            13 => "capture_compose",
            14 => "gpu_render",
            15 => "page_flip_queue",
            16 => "output_recovery",
            17 => "frame_callbacks",
            18 => "loop_complete",
            19 => "seat_initialization",
            20 => "gpu_initialization",
            21 => "output_initialization",
            _ => "unknown",
        }
    }
}

pub(crate) struct OutputDiagnostic {
    pub name: String,
    pub screen_off: bool,
    pub frame_queued: bool,
    pub pending_submission: bool,
    pub queue_failed: bool,
    pub last_render: Instant,
    pub presentations: u64,
}
struct SampledOutput {
    output: OutputDiagnostic,
    presentation_observed: Option<Instant>,
}
struct OutputSnapshot {
    backend: &'static str,
    active: bool,
    locked: bool,
    sampled: Instant,
    outputs: Vec<SampledOutput>,
}
struct DiagnosticState {
    started: Instant,

    phase: AtomicU64,
    phase_updates: AtomicU64,
    snapshot: Mutex<OutputSnapshot>,
    stopping: AtomicBool,
    wake: Condvar,
    wait: Mutex<()>,
}
impl DiagnosticState {
    fn mark(&self, phase: Phase) {
        let milliseconds = self
            .started
            .elapsed()
            .as_millis()
            .min(u64::MAX as u128 >> 8) as u64;
        self.phase
            .store((milliseconds << 8) | phase as u64, Ordering::Release);
        self.phase_updates.fetch_add(1, Ordering::Relaxed);
    }
    fn json(&self) -> serde_json::Value {
        let now = Instant::now();
        let phase = self.phase.load(Ordering::Acquire);
        let phase_age = self
            .started
            .elapsed()
            .as_millis()
            .saturating_sub(u128::from(phase >> 8));
        let snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let outputs: Vec<_> = snapshot.outputs.iter().map(|sample| {
            let output = &sample.output;
            serde_json::json!({
                "name": output.name,
                "screen_off": output.screen_off,
                "frame_queued": output.frame_queued,
                "pending_submission": output.pending_submission,
                "queue_failed": output.queue_failed,
                "last_render_age_ms": now.saturating_duration_since(output.last_render).as_millis() as u64,
                "presentations": output.presentations,

                "last_presentation_progress_age_ms": sample.presentation_observed.map(|last| now.saturating_duration_since(last).as_millis() as u64),
            })
        }).collect();
        serde_json::json!({
            "version": 1,
            "pid": std::process::id(),
            "timestamp_ms": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64,
            "backend": snapshot.backend,
            "phase": Phase::name(phase as u8),
            "phase_age_ms": phase_age as u64,
            "phase_updates": self.phase_updates.load(Ordering::Relaxed),
            "active": snapshot.active,
            "locked": snapshot.locked,
            "outputs_sample_age_ms": now.saturating_duration_since(snapshot.sampled).as_millis() as u64,
            "outputs": outputs,
        })
    }
}
struct Diagnostics {
    shared: Arc<DiagnosticState>,
    thread: Option<std::thread::JoinHandle<()>>,
    last_sample: Option<Instant>,
}
impl Diagnostics {
    fn start(path: &std::path::Path) -> std::io::Result<Self> {
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(".diagnostics");
        let path = PathBuf::from(sidecar);
        let now = Instant::now();
        let shared = Arc::new(DiagnosticState {
            started: now,
            phase: AtomicU64::new(Phase::Startup as u64),
            phase_updates: AtomicU64::new(0),
            snapshot: Mutex::new(OutputSnapshot {
                backend: "unknown",
                active: false,
                locked: false,
                sampled: now,
                outputs: Vec::new(),
            }),
            stopping: AtomicBool::new(false),
            wake: Condvar::new(),
            wait: Mutex::new(()),
        });
        let worker = shared.clone();
        let thread = std::thread::Builder::new()
            .name("rix-stall-diagnostics".into())
            .spawn(move || {
                let mut error_reported = false;
                loop {
                    if worker.stopping.load(Ordering::Acquire) {
                        break;
                    }
                    let result = write_diagnostics(&path, &worker.json());
                    if let Err(error) = result {
                        if !error_reported {
                            tracing::warn!(%error, "Could not write compositor stall diagnostics");
                        }
                        error_reported = true;
                    } else {
                        error_reported = false;
                    }
                    let guard = worker
                        .wait
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    let _ = worker
                        .wake
                        .wait_timeout_while(guard, Duration::from_secs(1), |_| {
                            !worker.stopping.load(Ordering::Acquire)
                        });
                }
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
            last_sample: None,
        })
    }
    fn outputs(
        &mut self,
        backend: &'static str,
        active: bool,
        locked: bool,
        sample: impl FnOnce() -> Vec<OutputDiagnostic>,
    ) {
        let now = Instant::now();
        if self
            .last_sample
            .is_some_and(|last| now.duration_since(last) < Duration::from_secs(1))
        {
            return;
        }
        let outputs = sample();
        let mut snapshot = self
            .shared
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let outputs = outputs
            .into_iter()
            .take(32)
            .map(|mut output| {
                output.name = output.name.chars().take(128).collect();
                let previous = snapshot
                    .outputs
                    .iter()
                    .find(|old| old.output.name == output.name);
                let presentation_observed = if output.presentations == 0 {
                    None
                } else if previous
                    .is_some_and(|old| old.output.presentations == output.presentations)
                {
                    previous.and_then(|old| old.presentation_observed)
                } else {
                    Some(now)
                };
                SampledOutput {
                    output,
                    presentation_observed,
                }
            })
            .collect();
        *snapshot = OutputSnapshot {
            backend,
            active,
            locked,
            sampled: now,
            outputs,
        };
        self.last_sample = Some(now);
    }
}
impl Drop for Diagnostics {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.wake.notify_all();
        if let Some(thread) = self.thread.take() {

            let deadline = Instant::now() + Duration::from_millis(100);
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if thread.is_finished() {
                let _ = thread.join();
            } else {
                tracing::warn!("Diagnostic worker did not stop within 100 ms; detaching it");
            }
        }
    }
}
fn write_diagnostics(path: &std::path::Path, snapshot: &serde_json::Value) -> std::io::Result<()> {
    let data = serde_json::to_vec(snapshot)?;

    if data.len() > 64 * 1024 {
        return Err(std::io::Error::other(
            "Diagnostic snapshot exceeds its limit",
        ));
    }
    let mut temporary = path.as_os_str().to_owned();
    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);
    temporary.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
    ));
    let temporary = PathBuf::from(temporary);

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(&data)?;
        std::fs::rename(&temporary, path)
    })();
    let _ = std::fs::remove_file(temporary);
    result
}

pub(crate) fn mark(heartbeat: Option<&SessionHeartbeat>, phase: Phase) {
    if let Some(heartbeat) = heartbeat {
        heartbeat.phase(phase);
    }
}

pub(crate) struct SessionHeartbeat {
    path: Option<PathBuf>,
    last_write: Option<Instant>,
    write_error_reported: bool,
    diagnostics: Option<Diagnostics>,
}

impl SessionHeartbeat {
    pub(crate) fn from_environment() -> Self {
        let path = std::env::var_os("RIX_COMPOSITOR_HEARTBEAT").map(PathBuf::from);
        let diagnostics = path
            .as_ref()
            .and_then(|path| match Diagnostics::start(path) {
                Ok(diagnostics) => Some(diagnostics),
                Err(error) => {
                    tracing::warn!(%error, "Could not start compositor stall diagnostics");
                    None
                }
            });
        Self {
            path,
            last_write: None,
            write_error_reported: false,
            diagnostics,
        }
    }

    pub(crate) fn phase(&self, phase: Phase) {
        if let Some(diagnostics) = &self.diagnostics {
            diagnostics.shared.mark(phase);
        }
    }
    pub(crate) fn output_snapshot(
        &mut self,
        backend: &'static str,
        active: bool,
        locked: bool,
        sample: impl FnOnce() -> Vec<OutputDiagnostic>,
    ) {
        if let Some(diagnostics) = &mut self.diagnostics {
            diagnostics.outputs(backend, active, locked, sample);
        }
    }

    pub(crate) fn beat(&mut self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        if self
            .last_write
            .is_some_and(|last| last.elapsed() < Duration::from_secs(1))
        {
            return;
        }

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let result = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
            .and_then(|mut file| writeln!(file, "{timestamp}"));
        match result {
            Ok(()) => {
                self.last_write = Some(Instant::now());
                self.write_error_reported = false;
            }
            Err(error) if !self.write_error_reported => {
                tracing::warn!(path = %path.display(), %error, "Could not update compositor heartbeat");
                self.write_error_reported = true;
            }
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn heartbeat_file_records_event_loop_progress() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("heartbeat");
        let mut heartbeat = SessionHeartbeat {
            path: Some(path.clone()),
            last_write: None,
            write_error_reported: false,
            diagnostics: None,
        };

        heartbeat.beat();

        let timestamp = fs::read_to_string(path).unwrap();
        assert!(timestamp.trim().parse::<u64>().is_ok());
    }

    fn output(name: &str) -> OutputDiagnostic {
        OutputDiagnostic {
            name: name.into(),
            screen_off: false,
            frame_queued: true,
            pending_submission: true,
            queue_failed: true,
            last_render: Instant::now() - Duration::from_secs(3),
            presentations: 17,
        }
    }

    #[test]
    fn diagnostics_continue_without_reporting_stalled_loop_as_healthy() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("heartbeat");
        let sidecar = directory.path().join("heartbeat.diagnostics");
        let mut heartbeat = SessionHeartbeat {
            path: Some(path.clone()),
            last_write: None,
            write_error_reported: false,
            diagnostics: Some(Diagnostics::start(&path).unwrap()),
        };
        heartbeat.beat();
        let original_heartbeat = fs::read(&path).unwrap();
        heartbeat.output_snapshot("drm", true, true, || vec![output("DP-1")]);
        heartbeat.phase(Phase::SceneCompose);

        let deadline = Instant::now() + Duration::from_secs(4);
        let snapshot = loop {
            if let Ok(data) = fs::read(&sidecar) {
                let snapshot: serde_json::Value = serde_json::from_slice(&data).unwrap();
                if snapshot["phase"] == "scene_compose"
                    && snapshot["phase_age_ms"].as_u64().unwrap() >= 50
                {
                    break snapshot;
                }
            }
            assert!(
                Instant::now() < deadline,
                "Diagnostic worker did not report the stalled stage"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(fs::read(&path).unwrap(), original_heartbeat);
        assert_eq!(snapshot["pid"], std::process::id());
        assert_eq!(snapshot["backend"], "drm");
        assert_eq!(snapshot["active"], true);
        assert_eq!(snapshot["locked"], true);
        let captured = &snapshot["outputs"][0];
        assert_eq!(captured["name"], "DP-1");
        assert_eq!(captured["frame_queued"], true);
        assert_eq!(captured["pending_submission"], true);
        assert_eq!(captured["queue_failed"], true);
        assert!(captured["last_render_age_ms"].as_u64().unwrap() >= 3000);
        assert_eq!(captured["presentations"], 17);
        assert_eq!(
            fs::metadata(&sidecar).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(fs::metadata(&sidecar).unwrap().len() <= 64 * 1024);

        drop(heartbeat);
        fs::remove_file(&sidecar).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert!(!sidecar.exists());
    }

    #[test]
    fn output_sampling_is_bounded_and_rate_limited() {
        let directory = tempfile::tempdir().unwrap();
        let mut diagnostics = Diagnostics::start(&directory.path().join("heartbeat")).unwrap();
        diagnostics.outputs("drm", true, false, || {
            (0..100)
                .map(|index| output(&format!("{index}-{}", "x".repeat(1000))))
                .collect()
        });
        diagnostics.outputs("nested", false, true, || {
            panic!("Output sampling must be throttled")
        });
        let snapshot = diagnostics.shared.json();
        assert_eq!(snapshot["backend"], "drm");
        assert_eq!(snapshot["outputs"].as_array().unwrap().len(), 32);
        assert!(snapshot["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|output| output["name"].as_str().unwrap().chars().count() <= 128));
        assert!(serde_json::to_vec(&snapshot).unwrap().len() <= 64 * 1024);
    }

    #[test]
    fn diagnostic_file_is_atomically_replaced_and_oversize_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("heartbeat.diagnostics");
        let first = serde_json::json!({"phase": "wayland_dispatch"});
        write_diagnostics(&path, &first).unwrap();
        let replacement = serde_json::json!({"phase": "gpu_render"});
        write_diagnostics(&path, &replacement).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(&path).unwrap()).unwrap(),
            replacement
        );
        let oversized = serde_json::json!({"unexpected": "x".repeat(64 * 1024)});
        assert!(write_diagnostics(&path, &oversized).is_err());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(&path).unwrap()).unwrap(),
            replacement
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn blocked_diagnostic_worker_cannot_block_compositor_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let mut diagnostics = Diagnostics::start(&directory.path().join("heartbeat")).unwrap();

        diagnostics.shared.stopping.store(true, Ordering::Release);
        diagnostics.shared.wake.notify_all();
        diagnostics.thread.take().unwrap().join().unwrap();
        let (release, blocked) = std::sync::mpsc::channel();
        let (finished, observed) = std::sync::mpsc::channel();
        diagnostics.thread = Some(std::thread::spawn(move || {
            let _ = blocked.recv();
            let _ = finished.send(());
        }));
        let started = Instant::now();
        drop(diagnostics);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "Diagnostic shutdown exceeded its bounded wait"
        );
        release.send(()).unwrap();
        observed.recv_timeout(Duration::from_secs(1)).unwrap();
    }
}
