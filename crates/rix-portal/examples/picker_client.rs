use anyhow::{Context, Result};
use clap::Parser;
use rix_portal::{
    backend::{Options, BUS_NAME, OBJECT_PATH},
    picker::{read_request, write_response, Filter, PickerMode, PickerResponse},
};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use zbus::{
    zvariant::{OwnedObjectPath, OwnedValue, Value},
    Proxy,
};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    request: PathBuf,
    #[arg(long)]
    response: PathBuf,
    #[arg(long)]
    ready: PathBuf,
    #[arg(long)]
    close_file: PathBuf,
    #[arg(long)]
    token: String,
}
fn value<T: Into<Value<'static>>>(value: T) -> OwnedValue {
    value.into().try_to_owned().expect("fixture value")
}
fn within(root: &Path, file: &Path) -> Result<()> {
    anyhow::ensure!(
        file.is_absolute()
            && file
                .parent()
                .context("File parent")?
                .canonicalize()?
                .starts_with(root)
            && !file.is_symlink(),
        "Fixture paths must remain in their private directory"
    );
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let root = PathBuf::from(
        std::env::var_os("RIX_FILES_PRIVATE_FIXTURE")
            .context("Private fixture guard is missing")?,
    )
    .canonicalize()?;
    let home = std::env::var_os("HOME").context("Private HOME is missing")?;
    let expected_home = root.join("home");
    anyhow::ensure!(
        Path::new(&home) == expected_home.as_path(),
        "This fixture cannot run on the host desktop"
    );
    for path in [&args.request, &args.response, &args.ready, &args.close_file] {
        within(&root, path)?;
    }
    anyhow::ensure!(
        args.token.len() <= 80
            && args
                .token
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_'),
        "Invalid request token"
    );
    let request = read_request(std::fs::File::open(&args.request)?)?;
    let mut options = Options::from([
        ("accept_label".into(), value(request.accept_label)),
        ("modal".into(), value(request.modal)),
        ("multiple".into(), value(request.multiple)),
        ("directory".into(), value(request.directory)),
        ("current_name".into(), value(request.current_name)),
        (
            "filters".into(),
            value(
                request
                    .filters
                    .into_iter()
                    .map(|filter| (filter.name, filter.rules))
                    .collect::<Vec<_>>(),
            ),
        ),
        (
            "choices".into(),
            value(
                request
                    .choices
                    .into_iter()
                    .map(|choice| (choice.id, choice.label, choice.options, choice.selected))
                    .collect::<Vec<_>>(),
            ),
        ),
    ]);
    for (key, path) in [
        ("current_folder", request.current_folder),
        ("current_file", request.current_file),
    ] {
        if let Some(mut path) = path {
            path.push(0);
            options.insert(key.into(), value(path));
        }
    }
    if let Some(Filter { name, rules }) = request.current_filter {
        options.insert("current_filter".into(), value((name, rules)));
    }
    if request.mode == PickerMode::SaveFiles {
        options.insert(
            "files".into(),
            value(
                request
                    .files
                    .into_iter()
                    .map(|mut name| {
                        name.push(0);
                        name
                    })
                    .collect::<Vec<_>>(),
            ),
        );
    }
    let connection = zbus::connection::Builder::session()?
        .name("org.freedesktop.portal.Desktop")?
        .build()
        .await?;
    let handle: OwnedObjectPath = format!(
        "/org/freedesktop/portal/desktop/request/native_fixture/{}",
        args.token
    )
    .try_into()?;
    let proxy = Proxy::new(
        &connection,
        BUS_NAME,
        OBJECT_PATH,
        "org.freedesktop.impl.portal.FileChooser",
    )
    .await?;
    std::fs::write(&args.ready, b"ready\n")?;
    let close = async {
        loop {
            if args.close_file.exists() {
                let request = Proxy::new(
                    &connection,
                    BUS_NAME,
                    &handle,
                    "org.freedesktop.impl.portal.Request",
                )
                .await?;
                return request.call::<_, _, ()>("Close", &()).await;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let body = (
        handle.clone(),
        request.app_id,
        request.parent_window,
        request.title,
        options,
    );
    let operation = proxy.call::<_, _, (u32, Options)>(
        match request.mode {
            PickerMode::Open => "OpenFile",
            PickerMode::Save => "SaveFile",
            PickerMode::SaveFiles => "SaveFiles",
        },
        &body,
    );
    tokio::pin!(operation, close);
    let (code, results) = tokio::select! {
        result = &mut operation => result?,
        result = &mut close => { result?; operation.await? },
    };
    let response = PickerResponse {
        response: code,
        uris: results
            .get("uris")
            .map(|v| Vec::<String>::try_from(v.try_clone()?))
            .transpose()?
            .unwrap_or_default(),
        choices: results
            .get("choices")
            .map(|v| Vec::<(String, String)>::try_from(v.try_clone()?))
            .transpose()?
            .unwrap_or_default(),
        current_filter: results
            .get("current_filter")
            .map(|v| <(String, Vec<(u32, String)>)>::try_from(v.try_clone()?))
            .transpose()?
            .map(|(name, rules)| Filter { name, rules }),
    };
    write_response(std::fs::File::create(&args.response)?, &response)
}
