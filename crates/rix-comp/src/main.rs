pub mod autostart;
pub mod backend;
mod blur;
mod capture_app;
mod capture_policy;
mod clipboard;
mod data_control;
mod decoration;
mod display_protocols;
mod dmabuf;
mod drag_drop;
pub mod grabs;
mod greeter;
mod idle;
mod keyboard;
mod layer_layout;
mod lifecycle;
mod monitor;
mod output_control;
mod output_layout;
mod plugins;
mod presentation;
mod preview;
mod protocol_handlers;
mod readback;
mod remote_input;
mod render;
mod screencopy;
mod screenshot;
mod session_lock;
mod shell_routing;
pub mod state;
mod surface_effects;
mod switcher;
mod toplevel_observer;
mod watchdog;
mod window_capture;
mod window_manager;

use clap::Parser;
use tracing::info;

#[derive(Parser, Debug)]
#[command(
    name = "rix-comp",
    about = "Modern Smithay-based Wayland Compositor for RixDe"
)]
pub struct Args {

    #[arg(long, conflicts_with = "drm")]
    pub nested: bool,

    #[arg(long, conflicts_with = "nested")]
    pub drm: bool,

    #[arg(long, requires = "drm")]
    pub experimental_kms: bool,

    #[arg(long, default_value = "")]
    pub socket: String,

    #[arg(long, requires = "greeter_command", conflicts_with = "socket")]
    pub greeter: bool,

    #[arg(long, requires = "greeter")]
    pub greeter_command: Option<std::path::PathBuf>,
}

pub enum BackendMode {
    Nested,
    Drm,
}

fn determine_backend(args: &Args) -> BackendMode {
    if args.drm {
        BackendMode::Drm
    } else if args.nested {
        BackendMode::Nested
    } else if std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
    {
        info!("Detected active display server in environment. Defaulting to Nested mode.");
        BackendMode::Nested
    } else {
        info!("No host display server detected. Defaulting to native DRM/KMS mode.");
        BackendMode::Drm
    }
}

const SESSION_CONFLICT_EXIT_CODE: u8 = 78;

fn failure_exit_code(error: &anyhow::Error) -> std::process::ExitCode {
    if error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists)
    {
        std::process::ExitCode::from(SESSION_CONFLICT_EXIT_CODE)
    } else {
        std::process::ExitCode::FAILURE
    }
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:?}");
            failure_exit_code(&error)
        }
    }
}

fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();
    rix_services::resources::desktop_priority();
    let greeter_command = args.greeter_command.as_deref();
    if let Some(command) = greeter_command {
        greeter::validate_command(command)?;
    }

    let mode = determine_backend(&args);
    match mode {
        BackendMode::Nested => {
            info!(
                "Initializing RixDe Compositor in [NESTED] mode on socket '{}'...",
                args.socket
            );
            backend::run_nested(&args.socket, greeter_command)?;
        }
        BackendMode::Drm => {
            if !args.experimental_kms {
                anyhow::bail!(
                    "Native DRM mode has a KMS/input implementation but has not passed hardware testing. Use --nested for normal use, or --drm --experimental-kms from a spare TTY to test it."
                );
            }
            backend::run_drm(&args.socket, greeter_command)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod startup_failure_tests {
    use super::*;

    #[test]
    fn session_conflict_retains_its_exit_code_through_context() {
        let error = anyhow::Error::from(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "IPC server already running",
        ))
        .context("Cannot initialize desktop IPC");
        assert_eq!(
            failure_exit_code(&error),
            std::process::ExitCode::from(SESSION_CONFLICT_EXIT_CODE)
        );
    }

    #[test]
    fn other_failures_remain_recoverable_by_the_watchdog() {
        for error in [
            anyhow::anyhow!("GPU initialization failed"),
            std::io::Error::from(std::io::ErrorKind::PermissionDenied).into(),
        ] {
            assert_eq!(failure_exit_code(&error), std::process::ExitCode::FAILURE);
        }
    }
}
