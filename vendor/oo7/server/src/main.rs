#![deny(unsafe_code)]
#[cfg(target_os = "linux")]
mod capability;
mod collection;
mod error;
#[cfg(any(feature = "gnome_native_crypto", feature = "gnome_openssl_crypto"))]
mod gnome;
mod handoff;
mod item;
mod metadata;
mod migration;
#[cfg(test)]
mod pam_listener;
#[cfg(any(feature = "plasma_native_crypto", feature = "plasma_openssl_crypto"))]
mod plasma;
mod policy;
mod prompt;
mod service;
mod session;
#[cfg(test)]
mod tests;

use std::{
    io::{IsTerminal, Read, Write},
    path::PathBuf,
    sync::LazyLock,
};

use clap::Parser;
use service::Service;

use crate::error::Error;

const BINARY_NAME: &str = env!("CARGO_BIN_NAME");

static LOGIN_HELPER_SOCKET: LazyLock<PathBuf> = LazyLock::new(|| {
    let uid = rustix::process::getuid().as_raw();
    std::env::var_os("CLAVIS_LOGIN_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}/clavis-login.sock")))
});

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(
        short = 'l',
        long,
        default_value_t = false,
        help = "Read a password from stdin, and use it to unlock the login keyring."
    )]
    login: bool,
    #[arg(
        short = 'v',
        long = "verbose",
        help = "Print debug information during command processing."
    )]
    is_verbose: bool,
    /// Private child protocol; never reads any external named socket.
    #[arg(
        long,
        hide = true,
        conflicts_with = "login",
        requires = "login_parent_pid"
    )]
    inherited_login: bool,
    #[arg(long, hide = true, requires = "inherited_login")]
    login_parent_pid: Option<u32>,
}

async fn supervise_login_child(stream: std::os::unix::net::UnixStream) -> Result<(), Error> {
    // This controller never sees the master or signals a peer-supplied PID.
    // Closing its connection asks the real helper parent to stop/reap its child.
    stream.set_nonblocking(true)?;
    let mut stream = tokio::net::UnixStream::from_std(stream)?;
    let connection = zbus::Connection::session().await?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut byte = [0u8; 1];
    tokio::select! {
        _ = tokio::io::AsyncReadExt::read(&mut stream, &mut byte) => {},
        _ = connection.closed() => {},
        _ = tokio::signal::ctrl_c() => {},
        _ = sigterm.recv() => {},
    }
    drop(stream);
    Ok(())
}

async fn read_secret_from_credentials_directory() -> Option<oo7::Secret> {
    let credential_dir = std::env::var("CREDENTIALS_DIRECTORY").ok()?;
    let cred_path =
        std::path::Path::new(&credential_dir).join("clavis.keyring-encryption-password");

    match tokio::fs::File::open(&cred_path).await {
        Ok(cred_file) => {
            let mut contents = zeroize::Zeroizing::new(Vec::new());
            match tokio::io::AsyncReadExt::read_to_end(
                &mut tokio::io::AsyncReadExt::take(cred_file, 65_537),
                &mut contents,
            )
            .await
            {
                Ok(_) if !contents.is_empty() && contents.len() <= 65_536 => {
                    tracing::info!("Unlocking session keyring with systemd credential");
                    Some(oo7::Secret::from(std::mem::take(&mut *contents)))
                }
                Ok(_) => None,
                Err(e) => {
                    tracing::error!("Failed to read credential: {e}");
                    None
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            tracing::error!("Failed to open credential: {e}");
            None
        }
    }
}

async fn inner_main(args: Args) -> Result<(), Error> {
    #[cfg(target_os = "linux")]
    {
        if rustix::process::getuid().as_raw() == 0
            || rustix::process::geteuid() != rustix::process::getuid()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Clavis is a per-user service; root/setuid execution is refused",
            )
            .into());
        }
        capability::drop_unnecessary_capabilities()?;
        rustix::process::setrlimit(
            rustix::process::Resource::Core,
            rustix::process::Rlimit {
                current: Some(0),
                maximum: Some(0),
            },
        )?;
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)?;
        handoff::require_untraced()?;
        if args.inherited_login {
            rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::TERM))?;
            let parent = rustix::process::getppid().map(|pid| pid.as_raw_pid() as u32);
            if parent != args.login_parent_pid {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "Login helper parent disappeared",
                )
                .into());
            }
            rustix::thread::set_no_new_privs(true)?;
            std::io::stdout().write_all(handoff::READY)?;
            std::io::stdout().flush()?;
        }
    }

    if !args.login && !args.inherited_login {
        if let Ok(address) = std::env::var("DBUS_SESSION_BUS_ADDRESS") {
            match handoff::request_start(&LOGIN_HELPER_SOCKET, &address) {
                Ok(Some(stream)) => return supervise_login_child(stream).await,
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!("Login activation unavailable; using manual unlock: {error}")
                }
            }
        }
    }

    let secret = if args.inherited_login {
        let mut secret = zeroize::Zeroizing::new(Vec::new());
        std::io::stdin()
            .lock()
            .take(65_537)
            .read_to_end(&mut secret)?;
        if secret.is_empty() || secret.len() > 65_536 {
            return Err(Error::EmptyPassword);
        }
        Some(oo7::Secret::from(std::mem::take(&mut *secret)))
    } else if args.login {
        let stdin = std::io::stdin().lock();
        if stdin.is_terminal() {
            let password = rpassword::prompt_password("Enter the login password: ")?;
            if password.is_empty() {
                tracing::error!("Login password can't be empty.");
                return Err(Error::EmptyPassword);
            }
            Some(oo7::Secret::text(password))
        } else {
            let mut buff = zeroize::Zeroizing::new(Vec::new());
            stdin.take(65_537).read_to_end(&mut buff)?;
            if buff.is_empty() || buff.len() > 65_536 {
                return Err(Error::EmptyPassword);
            }
            Some(oo7::Secret::from(std::mem::take(&mut *buff)))
        }
    } else {
        read_secret_from_credentials_directory().await
    };

    tracing::info!("Starting {BINARY_NAME}");

    let connection = match Service::run(secret, false).await {
        Ok(connection) => connection,
        Err(Error::File(oo7::file::Error::IncorrectSecret)) if !args.login => {
            tracing::warn!("Failed to unlock session keyring: credential contains wrong password");
            return Ok(());
        }
        Err(Error::Zbus(zbus::Error::NameTaken)) => {
            tracing::error!(
                "Another Secret Service already owns this session; Clavis will not replace it."
            );
            return Err(Error::Zbus(zbus::Error::NameTaken));
        }
        Err(err) => return Err(err),
    };

    if args.inherited_login {
        std::io::stdout().write_all(handoff::RUNNING)?;
        std::io::stdout().flush()?;
    }
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    tokio::select! {
        _ = connection.closed() => tracing::info!("D-Bus connection lost, shutting down"),
        _ = tokio::signal::ctrl_c() => tracing::info!("Received SIGINT, shutting down"),
        _ = sigterm.recv() => tracing::info!("Received SIGTERM, shutting down"),
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let args = Args::parse();

    if args.is_verbose {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_max_level(tracing_subscriber::filter::LevelFilter::DEBUG)
            .init();
        tracing::debug!("Running in verbose mode");
    } else {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .init();
    }

    inner_main(args).await.inspect_err(|err| {
        tracing::error!("{err:#}");
    })
}

#[cfg(test)]
mod policy_tests;
