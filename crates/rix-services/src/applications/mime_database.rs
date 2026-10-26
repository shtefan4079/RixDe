use std::{
    os::unix::fs::MetadataExt,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock, RwLock,
    },
    time::{Duration, Instant},
};
use xdg_mime::SharedMimeInfo;

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const FILES: &[&str] = &[
    "",
    "aliases",
    "icons",
    "generic-icons",
    "subclasses",
    "globs",
    "globs2",
    "magic",
];
type Stamp = Vec<(PathBuf, Option<(u64, u64, i64, i64, i64, i64)>)>;

fn directories() -> Vec<PathBuf> {
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("/tmp/.local/share"));
    let others = std::env::var_os("XDG_DATA_DIRS")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    std::iter::once(home)
        .chain(std::env::split_paths(&others).filter(|p| p.is_absolute()))
        .collect()
}
fn stamp(roots: &[PathBuf]) -> Stamp {
    roots
        .iter()
        .flat_map(|root| FILES.iter().map(move |name| root.join("mime").join(name)))
        .map(|path| {
            let metadata = std::fs::metadata(&path).ok().map(|m| {
                (
                    m.ino(),
                    m.len(),
                    m.mtime(),
                    m.mtime_nsec(),
                    m.ctime(),
                    m.ctime_nsec(),
                )
            });
            (path, metadata)
        })
        .collect()
}

struct Cache {
    database: RwLock<Arc<SharedMimeInfo>>,
    fingerprint: Mutex<Stamp>,
    last_check: Mutex<Instant>,
    refreshing: AtomicBool,
}
impl Cache {
    fn new() -> Self {

        let fingerprint = stamp(&directories());
        let database = Arc::new(SharedMimeInfo::new());
        Self {
            database: RwLock::new(database),
            fingerprint: Mutex::new(fingerprint),
            last_check: Mutex::new(Instant::now()),
            refreshing: AtomicBool::new(false),
        }
    }
    fn snapshot(self: &Arc<Self>) -> Arc<SharedMimeInfo> {
        let database = self.database.read().unwrap().clone();
        let mut last = self.last_check.lock().unwrap();
        if last.elapsed() >= REFRESH_INTERVAL && !self.refreshing.swap(true, Ordering::AcqRel) {
            *last = Instant::now();
            let cache = self.clone();
            if std::thread::Builder::new()
                .name("rix-mime-refresh".into())
                .spawn(move || {
                    cache.refresh();
                    cache.refreshing.store(false, Ordering::Release);
                })
                .is_err()
            {
                self.refreshing.store(false, Ordering::Release);
            }
        }
        database
    }
    fn refresh(&self) {
        self.refresh_with(stamp(&directories()), SharedMimeInfo::new);
    }
    fn refresh_with(&self, fingerprint: Stamp, load: impl FnOnce() -> SharedMimeInfo) {
        if *self.fingerprint.lock().unwrap() == fingerprint {
            return;
        }

        let database = Arc::new(load());
        *self.database.write().unwrap() = database;
        *self.fingerprint.lock().unwrap() = fingerprint;
    }
}

pub(super) fn snapshot() -> Arc<SharedMimeInfo> {
    static CACHE: OnceLock<Arc<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Arc::new(Cache::new())).snapshot()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unchanged_database_reuses_snapshot_and_file_changes_refresh_old_snapshots_safely() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().to_owned();
        std::fs::create_dir_all(root.join("mime")).unwrap();
        let glob = root.join("mime/globs2");
        std::fs::write(&glob, "80:application/x-rix-first:*.rixfixture\n").unwrap();
        let roots = [root.clone()];
        let initial = Arc::new(SharedMimeInfo::new_for_directory(&root));
        let cache = Cache {
            database: RwLock::new(initial.clone()),
            fingerprint: Mutex::new(stamp(&roots)),
            last_check: Mutex::new(Instant::now()),
            refreshing: AtomicBool::new(false),
        };
        cache.refresh_with(stamp(&roots), || panic!("unchanged cache must not parse"));
        assert!(Arc::ptr_eq(&cache.database.read().unwrap(), &initial));
        std::fs::write(&glob, "80:application/x-rix-second:*.rixfixture\n").unwrap();
        cache.refresh_with(stamp(&roots), || SharedMimeInfo::new_for_directory(&root));
        let updated = cache.database.read().unwrap().clone();
        assert!(!Arc::ptr_eq(&updated, &initial));
        assert_eq!(
            initial.get_mime_types_from_file_name("test.rixfixture")[0].to_string(),
            "application/x-rix-first"
        );
        assert_eq!(
            updated.get_mime_types_from_file_name("test.rixfixture")[0].to_string(),
            "application/x-rix-second"
        );
    }
    #[test]
    fn missing_mime_root_creation_and_deletion_change_fingerprint() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("created-later");
        let before = stamp(std::slice::from_ref(&root));
        std::fs::create_dir_all(root.join("mime")).unwrap();
        let during = stamp(std::slice::from_ref(&root));
        assert_ne!(before, during);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(before, stamp(std::slice::from_ref(&root)));
    }
}
