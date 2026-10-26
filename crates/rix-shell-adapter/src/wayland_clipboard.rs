use super::{Message, Proxy, WaylandState};
use smithay_client_toolkit::data_device_manager::{
    data_device::{DataDevice, DataDeviceHandler},
    data_offer::{DataOfferHandler, DragOffer, SelectionOffer},
    data_source::{CopyPasteSource, DataSourceHandler},
    DataDeviceManagerState, WritePipe,
};
use std::{
    cell::RefCell,
    fs::File,
    io::{self, Read, Write},
    os::fd::{AsRawFd, OwnedFd},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};
use wayland_client::{
    protocol::{
        wl_data_device::WlDataDevice, wl_data_device_manager::DndAction,
        wl_data_source::WlDataSource, wl_seat::WlSeat, wl_surface::WlSurface,
    },
    Connection, QueueHandle,
};

const LIMIT: usize = 4 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(2);
const MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain;charset=UTF-8",
    "UTF8_STRING",
    "text/plain",
];

struct ReadJob {
    file: File,
    generation: u64,
    deadline: Instant,
}
struct LatestRead {
    job: Mutex<Option<ReadJob>>,
    ready: Condvar,
    closed: AtomicBool,
}
struct WriteJob {
    file: File,
    text: Arc<str>,
    deadline: Instant,
}
struct Transfers {
    reader: Arc<LatestRead>,
    writer: mpsc::SyncSender<WriteJob>,
    generation: Arc<AtomicU64>,
}
impl Transfers {
    fn new(proxy: Proxy, generation: Arc<AtomicU64>) -> io::Result<Self> {
        let reader = Arc::new(LatestRead {
            job: Mutex::new(None),
            ready: Condvar::new(),
            closed: AtomicBool::new(false),
        });
        let reads = reader.clone();
        let current = generation.clone();
        std::thread::Builder::new()
            .name("rix-clipboard-read".into())
            .spawn(move || loop {
                let job = {
                    let mut job = reads.job.lock().unwrap();
                    while job.is_none() && !reads.closed.load(Ordering::Acquire) {
                        job = reads.ready.wait(job).unwrap();
                    }
                    if reads.closed.load(Ordering::Acquire) {
                        break;
                    }
                    job.take().unwrap()
                };
                let text = read_text(job.file, job.deadline, || {
                    current.load(Ordering::Acquire) != job.generation
                })
                .ok();
                let _ = proxy.send(Message::ClipboardReady(job.generation, text));
            })?;
        let (writer, writes) = mpsc::sync_channel::<WriteJob>(4);
        if let Err(error) = std::thread::Builder::new()
            .name("rix-clipboard-write".into())
            .spawn(move || {
                while let Ok(job) = writes.recv() {
                    let _ = write_text(job.file, &job.text, job.deadline);
                }
            })
        {
            let _job = reader.job.lock().unwrap();
            reader.closed.store(true, Ordering::Release);
            reader.ready.notify_one();
            return Err(error);
        }
        Ok(Self {
            reader,
            writer,
            generation,
        })
    }
    fn receive(&self, file: File, generation: u64) {

        *self.reader.job.lock().unwrap() = Some(ReadJob {
            file,
            generation,
            deadline: Instant::now() + TIMEOUT,
        });
        self.reader.ready.notify_one();
    }
    fn send(&self, file: File, text: Arc<str>) {

        let _ = self.writer.try_send(WriteJob {
            file,
            text,
            deadline: Instant::now() + TIMEOUT,
        });
    }
}
impl Drop for Transfers {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.reader.closed.store(true, Ordering::Release);
        self.reader.job.lock().unwrap().take();
        self.reader.ready.notify_one();
    }
}
fn nonblocking(file: &File) -> io::Result<()> {
    let fd = file.as_raw_fd();

    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn wait(
    file: &File,
    events: i16,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
) -> io::Result<()> {
    loop {
        if cancelled() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(io::ErrorKind::TimedOut)?;
        let mut fd = libc::pollfd {
            fd: file.as_raw_fd(),
            events,
            revents: 0,
        };

        let timeout = remaining.as_millis().clamp(1, 50) as i32;
        let result = unsafe { libc::poll(&mut fd, 1, timeout) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        } else if result > 0 {
            if fd.revents & libc::POLLNVAL != 0 {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            return Ok(());
        }
    }
}
fn read_text(
    mut file: File,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
) -> io::Result<String> {
    nonblocking(&file)?;
    let mut text = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        wait(&file, libc::POLLIN, deadline, &cancelled)?;
        match file.read(&mut buffer) {
            Ok(0) => return String::from_utf8(text).map_err(|_| io::ErrorKind::InvalidData.into()),
            Ok(length) => {
                if text.len() + length > LIMIT {
                    return Err(io::ErrorKind::FileTooLarge.into());
                }
                text.extend_from_slice(&buffer[..length]);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
    }
}
fn write_text(mut file: File, text: &str, deadline: Instant) -> io::Result<()> {
    nonblocking(&file)?;
    let mut bytes = text.as_bytes();
    while !bytes.is_empty() {
        wait(&file, libc::POLLOUT, deadline, || false)?;
        match file.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(length) => bytes = &bytes[length..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(super) struct Clipboard {
    pub serial: Option<u32>,
    pub pending: bool,
    pub device: Option<(WlSeat, DataDevice)>,
    pub manager: Option<DataDeviceManagerState>,
    text: Option<String>,
    publish: Option<Arc<str>>,
    source: Option<(CopyPasteSource, Arc<str>)>,
    offer: Option<SelectionOffer>,
    generation: Arc<AtomicU64>,
    transfers: Option<Transfers>,
}
impl Clipboard {
    pub fn new(manager: Option<DataDeviceManagerState>, proxy: Proxy) -> Rc<RefCell<Self>> {
        let generation = Arc::new(AtomicU64::new(0));
        let transfers = manager
            .as_ref()
            .and_then(|_| Transfers::new(proxy, generation.clone()).ok());
        Rc::new(RefCell::new(Self {
            manager,
            transfers,
            generation,
            serial: None,
            pending: false,
            device: None,
            text: None,
            publish: None,
            source: None,
            offer: None,
        }))
    }
    pub fn text(&self) -> Option<String> {
        self.text.clone()
    }
    pub fn set_text(&mut self, text: &str) {
        if text.len() > LIMIT || self.transfers.is_none() {
            return;
        }
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.pending = false;
        self.text = Some(text.into());
        self.publish = Some(Arc::from(text));
    }
    pub fn publish(&mut self, qh: &QueueHandle<WaylandState>) {
        let (Some(manager), Some((_, device)), Some(serial)) =
            (&self.manager, &self.device, self.serial)
        else {
            return;
        };
        if let Some(text) = self.publish.take() {
            let source = manager.create_copy_paste_source(qh, MIMES.iter().copied());
            source.set_selection(device, serial);
            self.source = Some((source, text));
        }
    }
    pub fn ready(&mut self, generation: u64, text: Option<String>) -> bool {
        if generation != self.generation.load(Ordering::Acquire) {
            return false;
        }
        self.pending = false;
        self.text = text;
        true
    }
    pub fn reconcile(&mut self) {

        if self.offer.is_some()
            && self
                .device
                .as_ref()
                .is_some_and(|(_, device)| device.data().selection_offer().is_none())
        {
            self.offer = None;
            if self.publish.is_none() {
                self.generation.fetch_add(1, Ordering::AcqRel);
                self.pending = false;
                self.text = None;
            }
        }
    }
    pub fn leave(&mut self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.pending = false;
        self.text = None;
        self.publish = None;
        self.serial = None;
        self.offer = None;
    }
}
impl DataDeviceHandler for WaylandState {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataDevice,
        _: f64,
        _: f64,
        _: &WlSurface,
    ) {
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64) {}
    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, device: &WlDataDevice) {
        let mut clipboard = self.clipboard.borrow_mut();
        let offer = clipboard
            .device
            .as_ref()
            .filter(|(_, d)| d.inner() == device)
            .and_then(|(_, d)| d.data().selection_offer());
        let generation = clipboard.generation.fetch_add(1, Ordering::AcqRel) + 1;
        clipboard.text = None;
        clipboard.pending = false;
        clipboard.offer = offer;
        if let Some(offer) = &clipboard.offer {
            let mime = offer.with_mime_types(|types| {
                MIMES
                    .iter()
                    .find(|mime| types.iter().any(|t| t == **mime))
                    .map(|m| (*m).to_owned())
            });
            if let (Some(mime), Some(transfers)) = (mime, &clipboard.transfers) {
                if let Ok(pipe) = offer.receive(mime) {
                    transfers.receive(File::from(OwnedFd::from(pipe)), generation);
                    clipboard.pending = true;
                }
            }
        }
        let ready = !clipboard.pending;
        drop(clipboard);
        if ready {
            self.complete_deferred_keys();
        }
    }
}
impl DataOfferHandler for WaylandState {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
}
impl DataSourceHandler for WaylandState {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: Option<String>,
    ) {
    }
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        mime: String,
        pipe: WritePipe,
    ) {
        let clipboard = self.clipboard.borrow();
        if !MIMES.contains(&mime.as_str()) {
            return;
        }
        if let (Some((owned, text)), Some(transfers)) = (&clipboard.source, &clipboard.transfers) {
            if owned.inner() == source {
                transfers.send(File::from(OwnedFd::from(pipe)), text.clone());
            }
        }
    }
    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        let mut clipboard = self.clipboard.borrow_mut();
        if clipboard
            .source
            .as_ref()
            .is_some_and(|(owned, _)| owned.inner() == source)
        {
            clipboard.source.take();
            if clipboard.publish.is_none() && clipboard.offer.is_none() {
                clipboard.generation.fetch_add(1, Ordering::AcqRel);
                clipboard.pending = false;
                clipboard.text = None;
            }
        }
    }
    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;
    fn pipe() -> (File, File) {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
    }
    fn proxy() -> (Proxy, mpsc::Receiver<Message>) {
        let (_read, write) = std::os::unix::net::UnixStream::pair().unwrap();
        write.set_nonblocking(true).unwrap();
        let (sender, receiver) = mpsc::sync_channel(crate::wayland::MESSAGE_QUEUE_CAPACITY);
        (
            Proxy {
                sender,
                wake: Arc::new(Mutex::new(write)),
                mailbox: Arc::new(crate::wayland::Mailbox::default()),
                loop_thread: std::thread::current().id(),
            },
            receiver,
        )
    }
    #[test]
    fn self_owned_clipboard_transfer_does_not_deadlock_the_read_worker() {
        let (proxy, receiver) = proxy();
        let generation = Arc::new(AtomicU64::new(7));
        let transfers = Transfers::new(proxy, generation).unwrap();
        let (read, write) = pipe();
        transfers.receive(read, 7);
        transfers.send(write, Arc::from("own clipboard"));
        match receiver.recv_timeout(Duration::from_secs(1)).unwrap() {
            Message::ClipboardReady(7, Some(text)) => assert_eq!(text, "own clipboard"),
            _ => panic!("Expected completed clipboard transfer"),
        }
    }
    #[test]
    fn stale_reads_cannot_restore_a_previous_selection_after_focus_or_owner_changes() {
        let (proxy, _receiver) = proxy();
        let clipboard = Clipboard::new(None, proxy);
        let mut clipboard = clipboard.borrow_mut();
        clipboard.generation.store(2, Ordering::Release);
        clipboard.pending = true;
        assert!(!clipboard.ready(1, Some("obsolete".into())));
        assert!(clipboard.pending);
        assert_eq!(clipboard.text(), None);
        assert!(clipboard.ready(2, Some("current".into())));
        assert_eq!(clipboard.text().as_deref(), Some("current"));
        clipboard.leave();
        assert!(!clipboard.ready(2, Some("current".into())));
        assert_eq!(clipboard.text(), None);
    }
    #[test]
    fn text_transfer_preserves_unicode_and_bounds_slow_peers_and_cancellation() {
        let (read, mut write) = pipe();
        write.write_all("Hello\n世界".as_bytes()).unwrap();
        drop(write);
        assert_eq!(
            read_text(read, Instant::now() + TIMEOUT, || false).unwrap(),
            "Hello\n世界"
        );
        let (read, _held_writer) = pipe();
        let start = Instant::now();
        assert_eq!(
            read_text(read, start + Duration::from_millis(20), || false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        let (read, _held_writer) = pipe();
        assert_eq!(
            read_text(read, Instant::now() + TIMEOUT, || true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
    }
    #[test]
    fn oversized_and_invalid_utf8_offers_are_not_cached_or_pasted() {
        let (read, mut write) = pipe();
        write.write_all(&[0xff]).unwrap();
        drop(write);
        assert_eq!(
            read_text(read, Instant::now() + TIMEOUT, || false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        let (read, write) = pipe();
        let sender = std::thread::spawn(move || {
            write_text(write, &"a".repeat(LIMIT + 1), Instant::now() + TIMEOUT)
        });
        assert_eq!(
            read_text(read, Instant::now() + TIMEOUT, || false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::FileTooLarge
        );
        let _ = sender.join().unwrap();
    }
    #[test]
    fn outgoing_clipboard_never_waits_forever_for_a_receiver() {
        let (_held_reader, write) = pipe();
        let start = Instant::now();
        assert_eq!(
            write_text(
                write,
                &"a".repeat(128 * 1024),
                start + Duration::from_millis(20)
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
