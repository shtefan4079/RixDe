use crate::state::RixState;
use rix_services::clipboard::{Entry, Job, Service, Snapshot};
use smithay::{
    input::Seat,
    reexports::wayland_server::Resource,
    wayland::selection::{SelectionHandler, SelectionSource, SelectionTarget},
};
use std::{os::fd::OwnedFd, os::unix::net::UnixStream, sync::Arc};
#[derive(Default)]
pub struct Clipboard {
    png_worker: Option<PngWorker>,
    service: Option<Service>,
    snapshot: Arc<Snapshot>,
    generation: u64,
    pending: Option<String>,
    owner: Option<smithay::reexports::wayland_server::Client>,
    retained: Option<Entry>,
    settings: Option<rix_config::desktop::ClipboardSettings>,
}
impl RixState {
    pub fn tick_clipboard(&mut self, config: &rix_config::ConfigManager) {
        self.poll_png_clipboard();
        if self.lock.active || self.greeter_mode {
            return;
        }
        let settings = config.get_desktop().clipboard;
        if self.clipboard.settings.is_none() {
            match rix_services::clipboard::start(
                config.config_dir().join("clipboard-history.json"),
                settings.clone(),
            ) {
                Ok(service) => self.clipboard.service = Some(service),
                Err(error) => tracing::warn!(%error, "Clipboard worker unavailable"),
            }
            self.clipboard.settings = Some(settings.clone());
        }
        if self.clipboard.settings.as_ref() != Some(&settings)
            && self
                .clipboard
                .submit(Job::Settings(settings.clone()))
                .is_ok()
        {
            if !settings.enabled {
                self.clipboard.generation = self.clipboard.generation.wrapping_add(1);
                self.clipboard.pending = None;
                self.clipboard.owner = None;
                self.clipboard.retained = None;
            }
            self.clipboard.settings = Some(settings);
        }
        if let Some(mime) = self.clipboard.pending.take() {
            if let Ok((reader, writer)) = UnixStream::pair() {
                if self
                    .clipboard
                    .submit(Job::Capture {
                        generation: self.clipboard.generation,
                        mime: mime.clone(),
                        stream: reader,
                    })
                    .is_ok()
                {
                    let _ = smithay::wayland::selection::data_device::request_data_device_client_selection(&self.seat, mime, writer.into());
                }
            }
        }
        loop {
            let update = self
                .clipboard
                .service
                .as_ref()
                .and_then(|s| s.updates.try_recv().ok());
            let Some(update) = update else { break };
            self.clipboard.snapshot = update.snapshot;
            if let Some((generation, entry)) = update.selected {
                if generation == self.clipboard.generation
                    && self.clipboard.settings.as_ref().is_some_and(|s| s.enabled)
                {
                    if generation == 0 {
                        self.own_clipboard(entry);
                    } else {
                        self.clipboard.retained = Some(entry);
                    }
                }
            }
            if let Some(sender) = &self.ipc_event_sender {
                let _ = sender.send(rix_ipc::IpcEvent::ClipboardChanged);
            }
        }
        if self
            .clipboard
            .owner
            .as_ref()
            .is_some_and(|owner| owner.get_credentials(&self.display_handle).is_err())
        {
            self.clipboard.owner = None;
            if let Some(entry) = self.clipboard.retained.take() {
                self.own_clipboard(entry);
            }
        }
    }
    fn own_clipboard(&mut self, entry: Entry) {
        self.clipboard.owner = None;
        self.clipboard.retained = None;
        let mimes = if matches!(
            entry.mime.as_str(),
            "text/plain" | "text/plain;charset=utf-8" | "UTF8_STRING"
        ) {
            vec![
                "text/plain;charset=utf-8".into(),
                "text/plain".into(),
                "UTF8_STRING".into(),
            ]
        } else {
            vec![entry.mime.clone()]
        };
        smithay::wayland::selection::data_device::set_data_device_selection(
            &self.display_handle,
            &self.seat,
            mimes,
            Arc::new(entry),
        );
    }
    pub(crate) fn set_png_clipboard(&mut self, data: Vec<u8>, responder: PngReply) {
        if self.clipboard.png_worker.is_none() {
            match PngWorker::start() {
                Ok(worker) => self.clipboard.png_worker = Some(worker),
                Err(error) => {
                    let _ = responder.send(Err(error.to_string()));
                    return;
                }
            }
        }
        if let Err(error) = self
            .clipboard
            .png_worker
            .as_ref()
            .unwrap()
            .sender
            .try_send((data, responder))
        {
            let (std::sync::mpsc::TrySendError::Full((_, responder))
            | std::sync::mpsc::TrySendError::Disconnected((_, responder))) = error;
            let _ = responder.send(Err("PNG clipboard worker is busy".into()));
        }
    }
    fn poll_png_clipboard(&mut self) {
        while let Some((result, responder)) = self
            .clipboard
            .png_worker
            .as_ref()
            .and_then(|worker| worker.receiver.try_recv().ok())
        {
            if responder.is_closed() {
                continue;
            }
            if self.lock.active || self.greeter_mode {
                let _ = responder.send(Err("Session is locked".into()));
                continue;
            }
            let result = result.and_then(|data| {
                if self.clipboard.service.is_none() {
                    return Err("Clipboard service is unavailable".into());
                }
                self.clipboard.generation = self.clipboard.generation.wrapping_add(1);
                self.clipboard.pending = None;
                self.own_clipboard(Entry {
                    id: 0,
                    mime: "image/png".into(),
                    data,
                    pinned: false,
                });
                Ok(rix_ipc::IpcResponse::Ok)
            });
            let _ = responder.send(result);
        }
    }
    pub fn clipboard_request(
        &mut self,
        request: rix_ipc::IpcRequest,
    ) -> Result<rix_ipc::IpcResponse, String> {
        use rix_ipc::{IpcRequest as R, IpcResponse as P};
        match request {
            R::GetClipboard => {
                return Ok(P::Clipboard {
                    entries: self
                        .clipboard
                        .snapshot
                        .entries
                        .iter()
                        .map(|e| rix_ipc::ClipboardEntry {
                            id: e.id,
                            label: e.label(),
                            mime: e.mime.clone(),
                            pinned: e.pinned,
                        })
                        .collect(),
                    error: self.clipboard.snapshot.error.clone(),
                })
            }
            R::ClipboardSelect(id) => {
                if !self
                    .clipboard
                    .settings
                    .as_ref()
                    .is_some_and(|settings| settings.enabled)
                {
                    return Err("Clipboard history is disabled".into());
                }
                let entry = self
                    .clipboard
                    .snapshot
                    .entries
                    .iter()
                    .find(|e| e.id == id)
                    .cloned()
                    .ok_or("Clipboard entry was removed")?;
                self.clipboard.generation = self.clipboard.generation.wrapping_add(1);
                self.own_clipboard(entry);
            }
            R::ClipboardDelete(id) => self.clipboard.submit(Job::Delete(id))?,
            R::ClipboardPin { id, pinned } => self.clipboard.submit(Job::Pin(id, pinned))?,
            R::ClipboardClear => {
                self.clipboard.pending = None;
                self.clipboard.owner = None;
                self.clipboard.retained = None;
                self.clipboard.generation = self.clipboard.generation.wrapping_add(1);
                self.clipboard.submit(Job::Clear)?;
                smithay::wayland::selection::data_device::clear_data_device_selection(
                    &self.display_handle,
                    &self.seat,
                );
            }
            _ => return Err("Invalid clipboard action".into()),
        }
        Ok(P::Ok)
    }
}
type PngReply = tokio::sync::oneshot::Sender<Result<rix_ipc::IpcResponse, String>>;
struct PngWorker {
    sender: std::sync::mpsc::SyncSender<(Vec<u8>, PngReply)>,
    receiver: std::sync::mpsc::Receiver<(Result<Vec<u8>, String>, PngReply)>,
}
impl PngWorker {
    fn start() -> std::io::Result<Self> {
        let (sender, work) = std::sync::mpsc::sync_channel::<(Vec<u8>, PngReply)>(2);
        let (publish, receiver) = std::sync::mpsc::sync_channel(2);
        std::thread::Builder::new()
            .name("rix-png-clipboard".into())
            .spawn(move || {
                for (data, responder) in work {
                    if responder.is_closed() {
                        continue;
                    }
                    let result = validate_png(&data).map(|()| data);
                    if publish.send((result, responder)).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self { sender, receiver })
    }
}
fn validate_png(data: &[u8]) -> Result<(), String> {
    let validate = || -> anyhow::Result<()> {
        anyhow::ensure!(
            !data.is_empty() && data.len() <= rix_ipc::CLIPBOARD_PNG_MAX_BYTES,
            "PNG exceeds the transfer limit"
        );
        let reader =
            image::ImageReader::with_format(std::io::Cursor::new(data), image::ImageFormat::Png);
        let (width, height) = reader.into_dimensions()?;
        anyhow::ensure!(
            width > 0
                && height > 0
                && width as u64 * height as u64 <= rix_ipc::CLIPBOARD_PNG_MAX_PIXELS,
            "PNG dimensions exceed the capture limit"
        );
        let mut reader =
            image::ImageReader::with_format(std::io::Cursor::new(data), image::ImageFormat::Png);
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(rix_ipc::CLIPBOARD_PNG_MAX_PIXELS * 8 + 1024 * 1024);
        reader.limits(limits);
        reader.decode()?;
        Ok(())
    };
    validate().map_err(|error| format!("Invalid PNG: {error}"))
}

impl Clipboard {
    fn submit(&self, job: Job) -> Result<(), String> {
        self.service
            .as_ref()
            .ok_or("Clipboard service is unavailable")?
            .commands
            .try_send(job)
            .map_err(|_| "Clipboard service is busy".into())
    }
}
impl SelectionHandler for RixState {
    type SelectionUserData = Arc<Entry>;
    fn allow_selection_receive(&self) -> bool {
        !self.lock.active && !self.greeter_mode
    }
    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _: Seat<Self>,
    ) {
        if self.lock.active || self.greeter_mode {
            return;
        }
        if ty != SelectionTarget::Clipboard {
            return;
        }
        self.clipboard.generation = self.clipboard.generation.wrapping_add(1);
        self.clipboard.pending = None;
        self.clipboard.retained = None;
        self.clipboard.owner = self
            .seat
            .get_keyboard()
            .and_then(|k| k.current_focus())
            .and_then(|s| s.client());
        if !self.clipboard.settings.as_ref().is_some_and(|s| s.enabled) {
            return;
        }
        let Some(source) = source else { return };
        let types = source.mime_types();
        if types.iter().any(|mime| {
            matches!(
                mime.as_str(),
                "x-kde-passwordManagerHint" | "application/x-kde-passwordManagerHint"
            )
        }) {
            return;
        }
        let Some(mime) = [
            "image/png",
            "text/uri-list",
            "text/plain;charset=utf-8",
            "text/plain",
            "UTF8_STRING",
        ]
        .into_iter()
        .find(|mime| {
            types.iter().any(|t| t == mime)
                && (*mime != "image/png"
                    || self.clipboard.settings.as_ref().is_some_and(|s| s.images))
        }) else {
            return;
        };
        self.clipboard.pending = Some(mime.into());
    }
    fn send_selection(
        &mut self,
        _: SelectionTarget,
        _: String,
        fd: OwnedFd,
        _: Seat<Self>,
        entry: &Arc<Entry>,
    ) {
        if self.lock.active {
            return;
        }
        let _ = self.clipboard.submit(Job::Serve {
            fd,
            data: entry.data.clone(),
        });
    }
}
pub fn focus(
    state: &mut RixState,
    seat: &Seat<RixState>,
    surface: Option<&smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>,
) {
    smithay::wayland::selection::data_device::set_data_device_focus(
        &state.display_handle,
        seat,
        if state.lock.active || state.greeter_mode {
            None
        } else {
            surface.and_then(Resource::client)
        },
    );
}

#[cfg(test)]
mod png_tests {
    use super::*;
    fn png() -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(4, 4, image::Rgba([15, 40, 90, 255]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }
    #[test]
    fn png_decoder_rejects_truncated_data_and_huge_transfers() {
        let data = png();
        assert!(validate_png(&data).is_ok());
        assert!(validate_png(&data[..16]).is_err());
        assert!(validate_png(b"arbitrary text").is_err());
        assert!(validate_png(&vec![0; rix_ipc::CLIPBOARD_PNG_MAX_BYTES + 1]).is_err());
    }
    #[test]
    fn png_selection_survives_without_history_and_lock_cancels_pending_updates() {
        let fixture = tempfile::tempdir().unwrap();
        let config = rix_config::ConfigManager::init_in(fixture.path().join("config")).unwrap();
        let mut desktop = config.get_desktop();
        desktop.clipboard.enabled = false;
        config.save_desktop(desktop).unwrap();
        let mut display = wayland_server::Display::<RixState>::new().unwrap();
        let mut state = RixState::new(&mut display, "png-clipboard-test".into());
        state.tick_clipboard(&config);
        let data = png();
        let (sender, mut reply) = tokio::sync::oneshot::channel();
        state.set_png_clipboard(data.clone(), sender);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            state.tick_clipboard(&config);
            if let Ok(result) = reply.try_recv() {
                assert_eq!(result.unwrap(), rix_ipc::IpcResponse::Ok);
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(
            smithay::wayland::selection::data_device::current_data_device_selection_userdata(
                &state.seat
            )
            .unwrap()
            .data,
            data
        );
        assert!(state.clipboard.snapshot.entries.is_empty());
        let (sender, mut reply) = tokio::sync::oneshot::channel();
        state.set_png_clipboard(png(), sender);
        state.lock.active = true;
        loop {
            state.tick_clipboard(&config);
            if let Ok(result) = reply.try_recv() {
                assert!(result.unwrap_err().contains("locked"));
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}
