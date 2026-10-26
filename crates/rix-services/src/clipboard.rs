use rix_config::desktop::ClipboardSettings;
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{mpsc, Arc},
    time::Duration,
};
pub const ITEM_BYTES: usize = 4 * 1024 * 1024;
const TOTAL_BYTES: usize = 16 * 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    pub mime: String,
    pub data: Vec<u8>,
    pub pinned: bool,
}
impl Entry {
    pub fn label(&self) -> String {
        if self.mime == "image/png" {
            return "Image".into();
        }
        let mut label = String::new();
        let mut space = false;
        for ch in String::from_utf8_lossy(&self.data).chars().take(512) {
            if ch.is_whitespace() {
                space = !label.is_empty();
                continue;
            }
            if space {
                label.push(' ');
                space = false;
            }
            label.push(ch);
            if label.chars().count() >= 120 {
                break;
            }
        }
        label
    }
}
#[derive(Clone, Default)]
pub struct Snapshot {
    pub entries: Vec<Entry>,
    pub error: String,
}
pub struct Update {
    pub snapshot: Arc<Snapshot>,
    pub selected: Option<(u64, Entry)>,
}
pub enum Job {
    Serve {
        fd: std::os::fd::OwnedFd,
        data: Vec<u8>,
    },
    Capture {
        generation: u64,
        mime: String,
        stream: UnixStream,
    },
    Delete(u64),
    Pin(u64, bool),
    Clear,
    Settings(ClipboardSettings),
}
pub struct Service {
    pub commands: mpsc::SyncSender<Job>,
    pub updates: mpsc::Receiver<Update>,
}
fn prune(entries: &mut Vec<Entry>, limit: usize) {
    while entries.len() > limit || entries.iter().map(|e| e.data.len()).sum::<usize>() > TOTAL_BYTES
    {
        let index = entries
            .iter()
            .rposition(|e| !e.pinned)
            .unwrap_or(entries.len() - 1);
        entries.remove(index);
    }
}
fn insert(
    entries: &mut Vec<Entry>,
    next_id: &mut u64,
    mime: String,
    data: Vec<u8>,
    limit: usize,
) -> Entry {
    let old = entries
        .iter()
        .position(|e| e.mime == mime && e.data == data)
        .map(|i| entries.remove(i));
    let id = old.as_ref().map(|e| e.id).unwrap_or_else(|| {
        let id = *next_id;
        *next_id = next_id.saturating_add(1);
        id
    });
    let entry = Entry {
        id,
        mime,
        data,
        pinned: old.is_some_and(|e| e.pinned),
    };
    entries.insert(0, entry.clone());
    prune(entries, limit);
    entry
}
fn valid(entry: &Entry) -> bool {
    !entry.data.is_empty()
        && entry.data.len() <= ITEM_BYTES
        && match entry.mime.as_str() {
            "text/plain;charset=utf-8" | "text/plain" | "UTF8_STRING" | "text/uri-list" => {
                std::str::from_utf8(&entry.data).is_ok()
            }
            "image/png" => entry.data.starts_with(b"\x89PNG\r\n\x1a\n"),
            _ => false,
        }
}
fn read(path: &std::path::Path) -> anyhow::Result<Vec<Entry>> {
    let file = std::fs::File::open(path)?;
    anyhow::ensure!(
        file.metadata()?.len() <= (TOTAL_BYTES * 5) as u64,
        "Clipboard history is too large"
    );
    let entries: Vec<Entry> = serde_json::from_reader(file)?;
    anyhow::ensure!(
        entries.len() <= 200 && entries.iter().all(valid),
        "Invalid clipboard history"
    );
    Ok(entries)
}
pub fn start(path: PathBuf, mut settings: ClipboardSettings) -> anyhow::Result<Service> {
    let (commands, jobs) = mpsc::sync_channel(16);
    let (publish, updates) = mpsc::sync_channel(2);
    std::thread::Builder::new()
        .name("rix-clipboard".into())
        .spawn(move || {
            let mut snapshot = Snapshot::default();
            if settings.enabled && settings.persist && path.exists() {
                match read(&path) {
                    Ok(entries) => snapshot.entries = entries,
                    Err(e) => snapshot.error = e.to_string(),
                }
            }
            prune(&mut snapshot.entries, settings.limit);
            let mut next_id = snapshot
                .entries
                .iter()
                .map(|e| e.id)
                .max()
                .unwrap_or(0)
                .saturating_add(1);
            let selected = snapshot.entries.first().cloned().map(|e| (0, e));
            let _ = publish.send(Update {
                snapshot: Arc::new(snapshot.clone()),
                selected,
            });
            while let Ok(job) = jobs.recv() {
                let mut selected = None;
                snapshot.error.clear();
                match job {
                    Job::Serve { fd, data } => {
                        let _ = serve(fd, &data);
                        continue;
                    }
                    Job::Capture {
                        generation,
                        mime,
                        stream,
                    } => {
                        if !settings.enabled || (mime == "image/png" && !settings.images) {
                            continue;
                        }
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let mut data = Vec::new();

                        match read_transfer(stream, &mut data) {
                            Ok(_) => {
                                let entry = Entry {
                                    id: 0,
                                    mime: mime.clone(),
                                    data: data.clone(),
                                    pinned: false,
                                };
                                if !valid(&entry) {
                                    continue;
                                }
                                selected = Some((
                                    generation,
                                    insert(
                                        &mut snapshot.entries,
                                        &mut next_id,
                                        mime,
                                        data,
                                        settings.limit,
                                    ),
                                ));
                            }
                            Err(error) => {
                                tracing::warn!(%error, "Could not read clipboard data");
                                snapshot.error = "Could not read clipboard data".into();
                            }
                        }
                    }
                    Job::Delete(id) => snapshot.entries.retain(|e| e.id != id),
                    Job::Pin(id, pinned) => {
                        if let Some(entry) = snapshot.entries.iter_mut().find(|e| e.id == id) {
                            entry.pinned = pinned;
                        }
                    }
                    Job::Clear => snapshot.entries.clear(),
                    Job::Settings(next) => {
                        settings = next;
                        if !settings.enabled {
                            snapshot.entries.clear();
                        }
                        if !settings.images {
                            snapshot.entries.retain(|e| e.mime != "image/png");
                        }
                        prune(&mut snapshot.entries, settings.limit);
                    }
                }
                let result = if settings.persist && settings.enabled {
                    serde_json::to_vec(&snapshot.entries)
                        .map_err(anyhow::Error::from)
                        .and_then(|bytes| {
                            rix_config::write_atomic(&path, &bytes).map_err(Into::into)
                        })
                } else if path.exists() {
                    std::fs::remove_file(&path).map_err(Into::into)
                } else {
                    Ok(())
                };
                if let Err(error) = result {
                    tracing::warn!(%error, "Could not persist clipboard history");
                    snapshot.error = "Could not save clipboard history".into();
                }
                if publish
                    .send(Update {
                        snapshot: Arc::new(snapshot.clone()),
                        selected,
                    })
                    .is_err()
                {
                    break;
                }
            }
        })?;
    Ok(Service { commands, updates })
}

fn read_transfer(mut stream: UnixStream, data: &mut Vec<u8>) -> std::io::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut bytes = [0u8; 16384];
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        stream.set_read_timeout(Some(remaining))?;
        let n = stream.read(&mut bytes)?;
        if n == 0 {
            return Ok(());
        }
        if data.len() + n > ITEM_BYTES {
            return Err(std::io::ErrorKind::FileTooLarge.into());
        }
        data.extend_from_slice(&bytes[..n]);
    }
}
fn serve(fd: std::os::fd::OwnedFd, data: &[u8]) -> std::io::Result<()> {
    use std::{io::Write, os::fd::AsRawFd};
    let mut file = std::fs::File::from(fd);
    let raw = file.as_raw_fd();

    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFL);
        if flags < 0 || libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut offset = 0;
    while offset < data.len() {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        let mut poll = libc::pollfd {
            fd: raw,
            events: libc::POLLOUT,
            revents: 0,
        };

        if unsafe { libc::poll(&mut poll, 1, remaining.as_millis().max(1) as i32) } <= 0 {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        match file.write(&data[offset..]) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => offset += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_moves_to_front_and_preserves_pin_with_bounded_eviction() {
        let mut entries = Vec::new();
        let mut next_id = 1;
        let a = insert(
            &mut entries,
            &mut next_id,
            "text/plain".into(),
            b"first".to_vec(),
            2,
        );
        entries[0].pinned = true;
        insert(
            &mut entries,
            &mut next_id,
            "text/plain".into(),
            b"second".to_vec(),
            2,
        );
        insert(
            &mut entries,
            &mut next_id,
            "text/plain".into(),
            b"third".to_vec(),
            2,
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].id, a.id);
        let again = insert(
            &mut entries,
            &mut next_id,
            "text/plain".into(),
            b"first".to_vec(),
            2,
        );
        assert_eq!(again.id, a.id);
        assert!(again.pinned);
        let last_id = entries.iter().map(|e| e.id).max().unwrap();
        entries.clear();
        let fresh = insert(
            &mut entries,
            &mut next_id,
            "text/plain".into(),
            b"fresh".to_vec(),
            2,
        );
        assert!(
            fresh.id > last_id,
            "Stale menu actions must not target a newer entry"
        );
    }
    #[test]
    fn persistent_history_is_private_and_can_be_disabled() {
        use std::{io::Write, os::unix::fs::PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clipboard.json");
        let service = start(path.clone(), ClipboardSettings::default()).unwrap();
        service.updates.recv().unwrap();
        let (reader, mut writer) = UnixStream::pair().unwrap();
        service
            .commands
            .send(Job::Capture {
                generation: 1,
                mime: "text/plain".into(),
                stream: reader,
            })
            .unwrap();
        writer.write_all(b"fixture clipboard").unwrap();
        drop(writer);
        let update = service
            .updates
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        assert_eq!(update.snapshot.entries[0].data, b"fixture clipboard");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(service);
        let service = start(path.clone(), ClipboardSettings::default()).unwrap();
        assert_eq!(
            service.updates.recv().unwrap().selected.unwrap().1.data,
            b"fixture clipboard"
        );
        service
            .commands
            .send(Job::Settings(ClipboardSettings {
                persist: false,
                ..Default::default()
            }))
            .unwrap();
        service.updates.recv().unwrap();
        assert!(!path.exists());
    }
}
