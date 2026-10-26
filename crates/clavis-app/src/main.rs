use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use clavis_client::{credential_attributes, Client, Interaction, Secret, MAX_SECRET_BYTES};
use std::io::{Read, Write};
use zeroize::Zeroizing;

#[derive(Parser)]
#[command(name = "clavis", version, about = "Clavis — encrypted desktop secrets")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {

    Gui,

    PromptService,

    Status,

    List,

    Store {
        application: String,
        key: String,
        #[arg(long)]
        label: String,
    },

    Get { application: String, key: String },

    Delete { application: String, key: String },

    Lock,

    Unlock,

    Create { label: String },

    Backup { destination: std::path::PathBuf },

    Restore { source: std::path::PathBuf },

    Import {
        #[arg(long)]
        source_bus: String,
    },
}

fn main() -> Result<()> {

    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
    }
    let args = Args::parse();
    match args.command {
        None | Some(Command::Gui) => clavis_app::ui::run_manager(),
        Some(Command::PromptService) => clavis_app::prompt::run_service(),
        Some(command) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(run_command(command))
        }
    }
}
async fn run_command(command: Command) -> Result<()> {
    if let Command::Restore { source } = command {

        let connection = zbus::Connection::session().await?;
        let dbus = zbus::fdo::DBusProxy::new(&connection).await?;
        anyhow::ensure!(
            !dbus
                .name_has_owner("org.freedesktop.secrets".try_into()?)
                .await?,
            "Stop Clavis before restoring a backup"
        );
        connection
            .request_name_with_flags(
                "org.freedesktop.secrets",
                zbus::fdo::RequestNameFlags::DoNotQueue.into(),
            )
            .await?;
        clavis_app::backup::restore(&source)?;
        println!("Encrypted backup restored");
        return Ok(());
    }
    let client = Client::connect(Interaction::Allow).await?;
    match command {
        Command::Status => println!(
            "{}",
            serde_json::to_string_pretty(&client.collections().await?)?
        ),
        Command::List => {
            let default = client
                .collections()
                .await?
                .into_iter()
                .find(|c| c.is_default)
                .context("Create a default vault first")?;
            println!(
                "{}",
                serde_json::to_string_pretty(&client.items(&default.path).await?)?
            );
        }
        Command::Store {
            application,
            key,
            label,
        } => {
            let attrs = credential_attributes(&application, &key)?;
            let mut value = Zeroizing::new(Vec::new());
            std::io::stdin()
                .take((MAX_SECRET_BYTES + 1) as u64)
                .read_to_end(&mut value)?;
            anyhow::ensure!(value.len() <= MAX_SECRET_BYTES, "Secret is too large");
            client
                .set(&label, &attrs, Secret::blob(std::mem::take(&mut *value)))
                .await?;
            println!("Stored and verified");
        }
        Command::Get { application, key } => {
            let secret = client
                .get(&credential_attributes(&application, &key)?)
                .await?
                .context("Credential not found")?;
            std::io::stdout().write_all(secret.as_bytes())?;
        }
        Command::Delete { application, key } => println!(
            "Deleted {}",
            client
                .delete(&credential_attributes(&application, &key)?)
                .await?
        ),
        Command::Lock => {
            client.lock_all().await?;
            println!("Locked");
        }
        Command::Unlock => {
            let default = client
                .collections()
                .await?
                .into_iter()
                .find(|c| c.is_default)
                .context("Create a default vault first")?;
            client.unlock(&default.path).await?;
            println!("Unlocked");
        }
        Command::Create { label } => {
            let make_default = !client.collections().await?.iter().any(|c| c.is_default);
            client.create(&label, make_default).await?;
            println!("Created");
        }
        Command::Backup { destination } => {
            let control = zbus::Proxy::new(
                client.connection(),
                "org.freedesktop.secrets",
                "/org/clavis/Control",
                "org.clavis.Control",
            )
            .await
            .map_err(clavis_client::Error::from)?;
            let _: String = control
                .get_property("Version")
                .await
                .map_err(clavis_client::Error::from)
                .context("The active provider must be Clavis before creating a Clavis backup")?;
            client.lock_all().await?;
            clavis_app::backup::save(&destination)?;
            println!("Encrypted backup saved");
        }
        Command::Import { source_bus } => {
            anyhow::ensure!(
                source_bus.starts_with("unix:"),
                "Import only supports a local Unix session bus"
            );
            let control = zbus::Proxy::new(
                client.connection(),
                "org.freedesktop.secrets",
                "/org/clavis/Control",
                "org.clavis.Control",
            )
            .await?;
            let _: String = control
                .get_property("Version")
                .await
                .map_err(clavis_client::Error::from)
                .context("The destination must be Clavis")?;
            let source_connection = zbus::connection::Builder::address(source_bus.as_str())?
                .build()
                .await?;
            let source_bus = zbus::fdo::DBusProxy::new(&source_connection).await?;
            let target_bus = zbus::fdo::DBusProxy::new(client.connection()).await?;
            anyhow::ensure!(
                source_bus.get_id().await? != target_bus.get_id().await?,
                "Source and destination must use different session buses"
            );
            let source = Client::with_connection(source_connection, Interaction::Allow).await?;
            let source_vault = source
                .service()
                .with_alias("default")
                .await
                .map_err(clavis_client::Error::from)?
                .context("The source has no default vault")?;
            source_vault
                .unlock(None)
                .await
                .map_err(clavis_client::Error::from)?;
            let target_vault = match client
                .service()
                .with_alias("default")
                .await
                .map_err(clavis_client::Error::from)?
            {
                Some(vault) => vault,
                None => client
                    .service()
                    .create_collection("Personal", Some("default"), None)
                    .await
                    .map_err(clavis_client::Error::from)?,
            };
            target_vault
                .unlock(None)
                .await
                .map_err(clavis_client::Error::from)?;
            let mut copied = 0;
            let mut identical = 0;
            for item in source_vault
                .items()
                .await
                .map_err(clavis_client::Error::from)?
            {
                let attributes = item
                    .attributes()
                    .await
                    .map_err(clavis_client::Error::from)?;
                let label = item.label().await.map_err(clavis_client::Error::from)?;
                let secret = item.secret().await.map_err(clavis_client::Error::from)?;
                anyhow::ensure!(
                    secret.as_bytes().len() <= MAX_SECRET_BYTES,
                    "Source credential exceeds the size limit"
                );
                let matching = if attributes.is_empty() {
                    let mut matching = Vec::new();
                    for existing in target_vault
                        .items()
                        .await
                        .map_err(clavis_client::Error::from)?
                    {
                        if existing
                            .attributes()
                            .await
                            .map_err(clavis_client::Error::from)?
                            .is_empty()
                            && existing.label().await.map_err(clavis_client::Error::from)? == label
                        {
                            matching.push(existing);
                        }
                    }
                    matching
                } else {
                    target_vault
                        .search_items(&attributes)
                        .await
                        .map_err(clavis_client::Error::from)?
                };
                if !matching.is_empty() {
                    let mut same = matching.len() == 1;
                    for existing in matching {
                        let saved = existing
                            .secret()
                            .await
                            .map_err(clavis_client::Error::from)?;
                        same &= saved.as_bytes() == secret.as_bytes()
                            && saved.content_type() == secret.content_type();
                    }
                    anyhow::ensure!(same, "A conflicting destination credential exists; no existing entry was overwritten");
                    identical += 1;
                    continue;
                }
                let stored = target_vault
                    .create_item(&label, &attributes, secret.clone(), false, None)
                    .await
                    .map_err(clavis_client::Error::from)?;
                let read_back = stored.secret().await.map_err(clavis_client::Error::from)?;
                anyhow::ensure!(
                    read_back.as_bytes() == secret.as_bytes()
                        && read_back.content_type() == secret.content_type(),
                    "Imported credential verification failed"
                );
                copied += 1;
            }
            println!("Copied and verified {copied} credentials; {identical} already matched. Source preserved.");
        }
        _ => unreachable!(),
    }
    Ok(())
}
