use std::time::{Duration, Instant};

pub fn start() -> slint::Timer {
    let timer = slint::Timer::default();
    if std::env::var("RIX_PROFILE_FRAMES").as_deref() != Ok("1") {
        return timer;
    }
    let mut previous = (0u64, Duration::ZERO);
    let mut previous_phases = [Duration::ZERO; 3];
    let mut since = Instant::now();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(1),
        move || {
            let current = crate::SHELL_SURFACE.with(|s| {
                s.borrow()
                    .as_ref()
                    .map(|s| s.frame_stats())
                    .unwrap_or_default()
            });
            let frames = current.0 - previous.0;
            let phases = crate::SHELL_SURFACE.with(|s| {
                s.borrow()
                    .as_ref()
                    .map(|s| s.paint_phases())
                    .unwrap_or_default()
            });
            if frames > 0 {
                tracing::info!(
                    target: "rix_shell::frames",
                    fps = frames as f64 / since.elapsed().as_secs_f64(),
                paint_ms = (current.1 - previous.1).as_secs_f64() * 1000. / frames as f64,
                render_ms = (phases[0] - previous_phases[0]).as_secs_f64() * 1000. / frames as f64,
                copy_ms = (phases[1] - previous_phases[1]).as_secs_f64() * 1000. / frames as f64,
                region_ms = (phases[2] - previous_phases[2]).as_secs_f64() * 1000. / frames as f64,
                    "Shell submitted frames (includes idle time)"
                );
            }
            previous = current;
            previous_phases = phases;
            since = Instant::now();
        },
    );
    timer
}
