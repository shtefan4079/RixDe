use clap::{Parser, Subcommand};
use rix_ipc::{IpcClient, IpcRequest, IpcResponse};

#[derive(Parser, Debug)]
#[command(name = "rix-ctl", about = "Control and query Rix Desktop Environment")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {

    Lock,

    Logout,

    Ping,

    EditMode {
        #[command(subcommand)]
        action: Option<EditModeAction>,
    },

    GetLayout,

    Screenshot {

        #[arg(short, long)]
        output: Option<String>,
    },

    Launcher {
        #[command(subcommand)]
        action: Option<LauncherAction>,
    },

    Output {
        #[command(subcommand)]
        action: Option<OutputAction>,
    },

    Windows {
        #[command(subcommand)]
        action: Option<WindowCliAction>,
    },
}

#[derive(Subcommand, Debug)]
enum WindowCliAction {
    List,
    Focus { id: u64 },
    Close { id: u64 },
    Minimize { id: u64 },
    Maximize { id: u64 },
}

#[derive(Subcommand, Debug)]
enum OutputAction {

    Flip,
}

#[derive(Subcommand, Debug)]
enum LauncherAction {

    Toggle,

    Open,

    Close,
}

#[derive(Subcommand, Debug)]
enum EditModeAction {

    Toggle,

    On,

    Off,

    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let mut client = IpcClient::connect().await.map_err(|e| {
        anyhow::anyhow!(
            "Failed to connect to Rix IPC socket. Is RixDe running? Error: {}",
            e
        )
    })?;

    match cli.command {
        Commands::Lock => match client.send(IpcRequest::LockSession).await? {
            IpcResponse::Ok => println!("Session locked"),
            IpcResponse::Error(error) => anyhow::bail!(error),
            other => anyhow::bail!("Unexpected lock response: {other:?}"),
        },
        Commands::Logout => match client.send(IpcRequest::LogoutSession).await? {
            IpcResponse::Ok => println!("Session closing"),
            other => anyhow::bail!("Unexpected logout response: {other:?}"),
        },
        Commands::Ping => {
            let res = client.send(IpcRequest::Ping).await?;
            if let IpcResponse::Pong = res {
                println!("pong! Rix IPC is alive.");
            }
        }
        Commands::EditMode { action } => match action.unwrap_or(EditModeAction::Status) {
            EditModeAction::Toggle => {
                let res = client.send(IpcRequest::ToggleEditMode).await?;
                if let IpcResponse::EditMode(active) = res {
                    println!(
                        "Edit Mode is now: {}",
                        if active { "ENABLED" } else { "DISABLED" }
                    );
                }
            }
            EditModeAction::On => {
                let res = client.send(IpcRequest::SetEditMode(true)).await?;
                if let IpcResponse::EditMode(_) = res {
                    println!("Edit Mode ENABLED");
                }
            }
            EditModeAction::Off => {
                let res = client.send(IpcRequest::SetEditMode(false)).await?;
                if let IpcResponse::EditMode(_) = res {
                    println!("Edit Mode DISABLED");
                }
            }
            EditModeAction::Status => {
                let res = client.send(IpcRequest::GetEditMode).await?;
                if let IpcResponse::EditMode(active) = res {
                    println!("Edit Mode: {}", if active { "ENABLED" } else { "DISABLED" });
                }
            }
        },
        Commands::GetLayout => {
            let res = client.send(IpcRequest::GetLayout).await?;
            if let IpcResponse::Layout(layout) = res {
                let json = serde_json::to_string_pretty(&layout)?;
                println!("{}", json);
            }
        }
        Commands::Windows { action } => {
            let request = match action.unwrap_or(WindowCliAction::List) {
                WindowCliAction::List => IpcRequest::ListWindows,
                WindowCliAction::Focus { id } => IpcRequest::FocusWindow(id),
                WindowCliAction::Close { id } => IpcRequest::CloseWindow(id),
                WindowCliAction::Minimize { id } => IpcRequest::MinimizeWindow(id),
                WindowCliAction::Maximize { id } => IpcRequest::MaximizeWindow(id),
            };
            match client.send(request).await? {
                IpcResponse::Windows(windows) => {
                    for window in windows {
                        let state = if window.is_minimized {
                            "minimized"
                        } else if window.is_active {
                            "active"
                        } else {
                            "open"
                        };
                        println!(
                            "{}\t{}\t{}\t{}",
                            window.id, state, window.app_id, window.title
                        );
                    }
                }
                IpcResponse::Ok => println!("Window command sent."),
                other => anyhow::bail!("Unexpected window response: {other:?}"),
            }
        }
        Commands::Screenshot { output } => {
            let res = client.send(IpcRequest::Screenshot { path: output }).await?;
            match res {
                IpcResponse::ScreenshotSaved(path) => {
                    println!(" Screenshot saved successfully to: {}", path);
                }
                IpcResponse::Error(err) => {
                    eprintln!(" Failed to capture screenshot: {}", err);
                    std::process::exit(1);
                }
                _ => {
                    eprintln!(" Unexpected response from compositor");
                    std::process::exit(1);
                }
            }
        }
        Commands::Launcher { action } => match action.unwrap_or(LauncherAction::Toggle) {
            LauncherAction::Toggle => {
                let _ = client.send(IpcRequest::ToggleLauncher).await?;
                println!("Application Launcher toggled");
            }
            LauncherAction::Open => {
                let _ = client.send(IpcRequest::SetLauncher(true)).await?;
                println!("Application Launcher opened");
            }
            LauncherAction::Close => {
                let _ = client.send(IpcRequest::SetLauncher(false)).await?;
                println!("Application Launcher closed");
            }
        },
        Commands::Output { action } => match action.unwrap_or(OutputAction::Flip) {
            OutputAction::Flip => {
                let res = client.send(IpcRequest::FlipOutput).await?;
                if let IpcResponse::Ok = res {
                    println!(" Output display orientation toggled (flipped)");
                } else if let IpcResponse::Error(e) = res {
                    eprintln!(" Failed to toggle output: {}", e);
                }
            }
        },
    }

    Ok(())
}
