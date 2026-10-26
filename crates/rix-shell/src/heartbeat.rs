use std::{
    io::Write,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

pub fn start() -> Option<slint::Timer> {
    let path = std::env::var_os("RIX_SHELL_HEARTBEAT").map(PathBuf::from)?;
    let timer = slint::Timer::default();
    let mut write_error_reported = false;
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_secs(1),
        move || {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let result = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&path)
                .and_then(|mut file| writeln!(file, "{timestamp}"));
            match result {
                Ok(()) => write_error_reported = false,
                Err(error) if !write_error_reported => {
                    tracing::warn!(path = %path.display(), %error, "Could not update shell heartbeat");
                    write_error_reported = true;
                }
                Err(_) => {}
            }
        },
    );
    Some(timer)
}
