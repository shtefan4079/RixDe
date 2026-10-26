use crate::picker::{
    Choice, Filter, PickerMode, PickerRequest, PickerResponse, MAX_REQUEST_BYTES,
    MAX_RESPONSE_BYTES, PROTOCOL_VERSION,
};
use anyhow::{Context, Result};
use std::{
    collections::{HashMap, HashSet},
    future::poll_fn,
    path::PathBuf,
    pin::Pin,
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{watch, Mutex, Notify, Semaphore},
};
use zbus::{
    export::futures_core::Stream,
    fdo,
    message::Header,
    names::{OwnedUniqueName, UniqueName},
    zvariant::{OwnedObjectPath, OwnedValue, Value},
    Connection,
};

pub const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.rix.files";
pub const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const FRONTEND: &str = "org.freedesktop.portal.Desktop";
pub type Options = HashMap<String, OwnedValue>;
type Results = (u32, Options);
type SerializedFilter = (String, Vec<(u32, String)>);
type SerializedChoice = (String, String, Vec<(String, String)>, String);

async fn require_frontend(
    connection: &Connection,
    sender: Option<&UniqueName<'_>>,
) -> fdo::Result<()> {
    let sender = sender
        .ok_or_else(|| fdo::Error::AccessDenied("A desktop portal sender is required".into()))?;
    let owner = zbus::fdo::DBusProxy::new(connection)
        .await?
        .get_name_owner(FRONTEND.try_into().expect("constant bus name"))
        .await
        .map_err(|_| fdo::Error::AccessDenied("The public desktop portal is unavailable".into()))?;
    if owner.as_str() != sender.as_str() {
        return Err(fdo::Error::AccessDenied(
            "Only the public desktop portal may request a file chooser".into(),
        ));
    }
    Ok(())
}

#[derive(Clone)]
struct Request {
    owner: OwnedUniqueName,
    cancel: watch::Sender<bool>,
    stopped: Arc<Notify>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Request")]
impl Request {
    async fn close(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        require_frontend(connection, header.sender()).await?;
        if header
            .sender()
            .is_none_or(|sender| sender.as_str() != self.owner.as_str())
        {
            return Err(fdo::Error::AccessDenied(
                "File chooser request belongs to another frontend".into(),
            ));
        }
        let _ = self.cancel.send(true);

        let _ = tokio::time::timeout(Duration::from_secs(3), self.stopped.notified()).await;
        Ok(())
    }
}

struct ActiveRequest {
    handle: OwnedObjectPath,
    active: Arc<Mutex<HashSet<OwnedObjectPath>>>,
    connection: Connection,
    stopped: Arc<Notify>,
    finished: bool,
}

impl ActiveRequest {
    async fn finish(mut self) {
        self.stopped.notify_waiters();

        self.stopped.notify_one();
        self.active.lock().await.remove(&self.handle);
        let _ = self
            .connection
            .object_server()
            .remove::<Request, _>(&self.handle)
            .await;
        self.finished = true;
    }
}

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.stopped.notify_waiters();
        self.stopped.notify_one();
        let handle = self.handle.clone();
        let active = Arc::clone(&self.active);
        let connection = self.connection.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                active.lock().await.remove(&handle);
                let _ = connection
                    .object_server()
                    .remove::<Request, _>(&handle)
                    .await;
            });
        } else if let Ok(mut active) = self.active.try_lock() {

            active.remove(&self.handle);
        }
    }
}

#[derive(Clone)]
pub struct FileChooser {
    picker: PathBuf,
    active: Arc<Mutex<HashSet<OwnedObjectPath>>>,
    slots: Arc<Semaphore>,
}

impl FileChooser {
    pub fn new(picker: PathBuf) -> Self {
        Self {
            picker,
            active: Arc::default(),
            slots: Arc::new(Semaphore::new(8)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn choose(
        &self,
        mode: PickerMode,
        handle: OwnedObjectPath,
        app_id: &str,
        parent_window: &str,
        title: &str,
        options: Options,
        header: Header<'_>,
        connection: &Connection,
    ) -> fdo::Result<Results> {
        require_frontend(connection, header.sender()).await?;
        if handle.as_str().len() > 1024
            || !handle
                .as_str()
                .starts_with("/org/freedesktop/portal/desktop/request/")
        {
            return Err(fdo::Error::InvalidArgs(
                "A desktop portal request path is required".into(),
            ));
        }
        let request = parse_request(mode, app_id, parent_window, title, &options)
            .map_err(|error| fdo::Error::InvalidArgs(error.to_string()))?;
        let _slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| fdo::Error::LimitsExceeded("Too many open file choosers".into()))?;
        if !self.active.lock().await.insert(handle.clone()) {
            return Err(fdo::Error::InvalidArgs(
                "File chooser request is already active".into(),
            ));
        }
        let (cancel, receiver) = watch::channel(false);
        let stopped = Arc::new(Notify::new());
        let owner: OwnedUniqueName = header.sender().expect("validated sender").to_owned().into();
        let request_object = Request {
            owner: owner.clone(),
            cancel,
            stopped: stopped.clone(),
        };
        let registration = ActiveRequest {
            handle: handle.clone(),
            active: self.active.clone(),
            connection: connection.clone(),
            stopped,
            finished: false,
        };
        if let Err(error) = connection.object_server().at(&handle, request_object).await {
            registration.finish().await;
            return Err(error.into());
        }
        let result = match run_picker(&self.picker, &request, receiver, connection, &owner).await {
            Ok(response) => to_results(response),
            Err(error) => {
                tracing::warn!("File chooser failed: {error:#}");
                Ok((2, Options::new()))
            }
        };
        registration.finish().await;
        result.map_err(|error| fdo::Error::Failed(error.to_string()))
    }
}

#[zbus::interface(name = "org.freedesktop.impl.portal.FileChooser")]
impl FileChooser {
    #[zbus(property(emits_changed_signal = "const"), name = "version")]
    fn version(&self) -> u32 {
        4
    }

    #[zbus(out_args("response", "results"))]
    #[allow(clippy::too_many_arguments)]
    async fn open_file(
        &self,
        handle: OwnedObjectPath,
        app_id: &str,
        parent_window: &str,
        title: &str,
        options: Options,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<Results> {
        self.choose(
            PickerMode::Open,
            handle,
            app_id,
            parent_window,
            title,
            options,
            header,
            connection,
        )
        .await
    }

    #[zbus(out_args("response", "results"))]
    #[allow(clippy::too_many_arguments)]
    async fn save_file(
        &self,
        handle: OwnedObjectPath,
        app_id: &str,
        parent_window: &str,
        title: &str,
        options: Options,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<Results> {
        self.choose(
            PickerMode::Save,
            handle,
            app_id,
            parent_window,
            title,
            options,
            header,
            connection,
        )
        .await
    }

    #[zbus(out_args("response", "results"))]
    #[allow(clippy::too_many_arguments)]
    async fn save_files(
        &self,
        handle: OwnedObjectPath,
        app_id: &str,
        parent_window: &str,
        title: &str,
        options: Options,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<Results> {
        self.choose(
            PickerMode::SaveFiles,
            handle,
            app_id,
            parent_window,
            title,
            options,
            header,
            connection,
        )
        .await
    }
}

fn get<T: TryFrom<OwnedValue>>(options: &Options, key: &str) -> Result<Option<T>> {
    options
        .get(key)
        .map(|value| {
            let value = value
                .try_clone()
                .context("Cannot read file chooser option")?;
            T::try_from(value)
                .map_err(|_| anyhow::anyhow!("File chooser option {key} has the wrong type"))
        })
        .transpose()
}

fn path_option(options: &Options, key: &str) -> Result<Option<Vec<u8>>> {
    get::<Vec<u8>>(options, key)?
        .map(|mut bytes| {
            anyhow::ensure!(
                bytes.last() == Some(&0),
                "File chooser path {key} must be NUL terminated"
            );
            bytes.pop();
            Ok(bytes)
        })
        .transpose()
}

pub fn parse_request(
    mode: PickerMode,
    app_id: &str,
    parent_window: &str,
    title: &str,
    options: &Options,
) -> Result<PickerRequest> {
    anyhow::ensure!(options.len() <= 64, "Too many file chooser options");
    let filters = get::<Vec<SerializedFilter>>(options, "filters")?
        .unwrap_or_default()
        .into_iter()
        .map(|(name, rules)| Filter { name, rules })
        .collect();
    let current_filter = get::<SerializedFilter>(options, "current_filter")?
        .map(|(name, rules)| Filter { name, rules });
    let choices = get::<Vec<SerializedChoice>>(options, "choices")?
        .unwrap_or_default()
        .into_iter()
        .map(|(id, label, options, selected)| Choice {
            id,
            label,
            options,
            selected,
        })
        .collect();
    let mut files = get::<Vec<Vec<u8>>>(options, "files")?.unwrap_or_default();

    if files
        .last()
        .is_some_and(|name| name.is_empty() || name == &[0])
    {
        files.pop();
    }
    for file in &mut files {
        anyhow::ensure!(
            file.last() == Some(&0),
            "SaveFiles filenames must be NUL terminated"
        );
        file.pop();
    }
    let request = PickerRequest {
        version: PROTOCOL_VERSION,
        mode,
        app_id: app_id.into(),
        title: title.into(),
        parent_window: parent_window.into(),
        accept_label: get(options, "accept_label")?.unwrap_or_default(),
        modal: get(options, "modal")?.unwrap_or(true),
        multiple: mode == PickerMode::Open && get(options, "multiple")?.unwrap_or(false),
        directory: mode == PickerMode::SaveFiles || get(options, "directory")?.unwrap_or(false),
        current_folder: path_option(options, "current_folder")?,
        current_file: path_option(options, "current_file")?,
        current_name: get(options, "current_name")?.unwrap_or_default(),
        files,
        filters,
        current_filter,
        choices,
    };
    request.validate()?;
    anyhow::ensure!(
        serde_json::to_vec(&request)?.len() <= MAX_REQUEST_BYTES,
        "File chooser request is too large"
    );
    Ok(request)
}

fn to_results(response: PickerResponse) -> Result<Results> {
    if response.response != 0 {
        return Ok((response.response, Options::new()));
    }
    let mut results = Options::from([
        ("uris".into(), Value::from(response.uris).try_to_owned()?),
        (
            "choices".into(),
            Value::from(response.choices).try_to_owned()?,
        ),
    ]);
    if let Some(Filter { name, rules }) = response.current_filter {
        results.insert(
            "current_filter".into(),
            Value::from((name, rules)).try_to_owned()?,
        );
    }
    Ok((0, results))
}

async fn stream_next<S: Stream>(mut stream: Pin<&mut S>) -> Option<S::Item> {
    poll_fn(|context| stream.as_mut().poll_next(context)).await
}

async fn frontend_disappeared(connection: &Connection, owner: &OwnedUniqueName) {
    let Ok(dbus) = zbus::fdo::DBusProxy::new(connection).await else {
        return;
    };

    let Ok(stream) = dbus
        .receive_name_owner_changed_with_args(&[(0, FRONTEND), (2, "")])
        .await
    else {
        return;
    };
    let mut stream = std::pin::pin!(stream);

    if require_frontend(connection, Some(owner)).await.is_err() {
        return;
    }

    let _ = stream_next(stream.as_mut()).await;
}

async fn cancelled(mut cancel: watch::Receiver<bool>) {
    if *cancel.borrow() {
        return;
    }
    while cancel.changed().await.is_ok() {
        if *cancel.borrow() {
            return;
        }
    }
}

async fn run_picker(
    picker: &std::path::Path,
    request: &PickerRequest,
    cancel: watch::Receiver<bool>,
    connection: &Connection,
    owner: &OwnedUniqueName,
) -> Result<PickerResponse> {
    let bytes = serde_json::to_vec(request)?;
    let mut child = tokio::process::Command::new(picker)
        .arg("--portal-picker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("Cannot start native RixFiles picker")?;
    let mut input = child.stdin.take().context("Picker input is unavailable")?;
    let mut output = child
        .stdout
        .take()
        .context("Picker output is unavailable")?;
    let interaction = async {
        input.write_all(&bytes).await?;
        input.shutdown().await?;
        drop(input);
        let mut bytes = Vec::new();
        (&mut output)
            .take((MAX_RESPONSE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await?;
        anyhow::ensure!(
            bytes.len() <= MAX_RESPONSE_BYTES,
            "Picker response is too large"
        );
        let status = child.wait().await?;
        anyhow::ensure!(status.success(), "Picker did not exit successfully");
        let response: PickerResponse =
            serde_json::from_slice(&bytes).context("Picker returned an invalid response")?;
        request.validate_response(&response)
    };

    let result = tokio::select! {
        _ = cancelled(cancel) => None,
        _ = frontend_disappeared(connection, owner) => None,
        result = tokio::time::timeout(Duration::from_secs(24 * 60 * 60), interaction) => Some(result),
    };
    match result {
        Some(Ok(Ok(response))) => Ok(response),
        other => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            match other {
                None => Ok(PickerResponse::cancelled()),
                Some(Ok(Err(error))) => Err(error),
                Some(Err(_)) => anyhow::bail!("File chooser expired"),
                Some(Ok(Ok(_))) => unreachable!(),
            }
        }
    }
}

pub async fn serve(picker: PathBuf) -> Result<()> {
    let _connection = zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, FileChooser::new(picker))?
        .build()
        .await?;
    tracing::info!("Native RixFiles FileChooser portal is ready");
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => (),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        process::{Child, Command},
    };

    struct Bus(Child);
    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    async fn private_connection() -> (Bus, Connection) {
        let mut bus = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut address = String::new();
        BufReader::new(bus.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let connection = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .build()
            .await
            .unwrap();
        (Bus(bus), connection)
    }

    fn handle() -> OwnedObjectPath {
        "/org/freedesktop/portal/desktop/request/fixture/guard"
            .try_into()
            .unwrap()
    }

    fn request_for(connection: &Connection) -> Request {
        let (cancel, _receiver) = watch::channel(false);
        Request {
            owner: connection.unique_name().unwrap().to_owned(),
            cancel,
            stopped: Arc::new(Notify::new()),
        }
    }

    fn registration(
        chooser: &FileChooser,
        connection: &Connection,
        handle: &OwnedObjectPath,
        stopped: Arc<Notify>,
    ) -> ActiveRequest {
        ActiveRequest {
            handle: handle.clone(),
            active: chooser.active.clone(),
            connection: connection.clone(),
            stopped,
            finished: false,
        }
    }

    async fn object_exists(connection: &Connection, handle: &OwnedObjectPath) -> bool {
        connection
            .object_server()
            .at(handle, request_for(connection))
            .await
            .map(|added| !added)
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn dropped_registration_releases_handle_and_request_object() {
        let (_bus, connection) = private_connection().await;
        let chooser = FileChooser::new(PathBuf::from("/nonexistent/rix-picker"));
        let handle = handle();
        assert!(chooser.active.lock().await.insert(handle.clone()));
        connection
            .object_server()
            .at(&handle, request_for(&connection))
            .await
            .unwrap();
        let stopped = Arc::new(Notify::new());
        let registration = registration(&chooser, &connection, &handle, stopped.clone());
        drop(registration);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), stopped.notified())
                .await
                .is_ok(),
            "Close waiters must be woken when the registration is abandoned"
        );
        let cleanup = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if chooser.active.lock().await.is_empty()
                    && !object_exists(&connection, &handle).await
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(cleanup.is_ok(), "the dropped registration leaked state");
    }

    #[tokio::test]
    async fn finished_registration_cleans_up_inline() {
        let (_bus, connection) = private_connection().await;
        let chooser = FileChooser::new(PathBuf::from("/nonexistent/rix-picker"));
        let handle = handle();
        assert!(chooser.active.lock().await.insert(handle.clone()));
        connection
            .object_server()
            .at(&handle, request_for(&connection))
            .await
            .unwrap();
        let stopped = Arc::new(Notify::new());
        let registration = registration(&chooser, &connection, &handle, stopped.clone());
        registration.finish().await;
        assert!(
            tokio::time::timeout(Duration::from_secs(1), stopped.notified())
                .await
                .is_ok()
        );
        assert!(chooser.active.lock().await.is_empty());
        assert!(!object_exists(&connection, &handle).await);
    }
}
