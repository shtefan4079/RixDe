use std::os::unix::fs::PermissionsExt;
use std::{
    cmp::Ordering,
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
    time::{Duration, Instant, SystemTime},
};

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub kind: String,
    pub icon_name: String,
    pub icon_names: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SortBy {
    #[default]
    Name,
    Size,
    Modified,
    Kind,
}

fn entry(path: PathBuf, icons: &crate::opening::MimeDetector) -> io::Result<Entry> {
    let metadata = fs::symlink_metadata(&path)?;
    let is_symlink = metadata.is_symlink();
    let is_dir = metadata.is_dir() || (is_symlink && path.is_dir());
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    let (kind, icon_names) = if is_dir {
        ("Folder".into(), vec!["folder".into()])
    } else if is_symlink {
        (
            "Symbolic link".into(),
            vec!["emblem-symbolic-link".into(), "text-x-generic".into()],
        )
    } else {
        let mime_icons = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| icons.icon_names_for_filename(name))
            .unwrap_or_default();
        (
            if extension.is_empty() {
                "File".into()
            } else {
                format!("{} file", extension.to_uppercase())
            },
            crate::file_icons::names_with_mime(
                &path,
                false,
                metadata.permissions().mode() & 0o111 != 0,
                mime_icons,
            ),
        )
    };
    Ok(Entry {
        name: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        path,
        is_dir,
        is_symlink,
        size: metadata.len(),
        modified: metadata.modified().ok(),
        kind,
        icon_name: icon_names
            .first()
            .cloned()
            .unwrap_or_else(|| "text-x-generic".into()),
        icon_names,
    })
}

fn hidden(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    path.file_name()
        .is_some_and(|s| s.as_bytes().starts_with(b"."))
}

fn check_cancelled(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(AtomicOrdering::Relaxed) {
        Err(io::ErrorKind::Interrupted.into())
    } else {
        Ok(())
    }
}

pub fn read_directory(
    path: &Path,
    show_hidden: bool,
    sort: SortBy,
    descending: bool,
    cancel: &AtomicBool,
) -> io::Result<Vec<Entry>> {
    check_cancelled(cancel)?;
    let icons = crate::opening::MimeDetector::new();
    let mut entries = Vec::new();
    for item in fs::read_dir(path)? {
        check_cancelled(cancel)?;
        let path = match item {
            Ok(item) => item.path(),
            Err(_) => continue,
        };
        if !show_hidden && hidden(&path) {
            continue;
        }
        match entry(path, &icons) {
            Ok(item) => entries.push(item),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => continue,
        }
    }
    check_cancelled(cancel)?;
    sort_entries(&mut entries, sort, descending);
    Ok(entries)
}

pub const SEARCH_LIMIT: usize = 10_000;

pub const SEARCH_DEADLINE: Duration = Duration::from_secs(10);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchStop {
    Complete,
    ResultLimit,
    Deadline,
}
#[derive(Debug)]
pub struct SearchOutcome {
    pub entries: Vec<Entry>,

    pub stop: SearchStop,
}
pub fn search_directory(
    path: &Path,
    query: &str,
    show_hidden: bool,
    cancel: &AtomicBool,
) -> io::Result<Vec<Entry>> {
    search_directory_with_status(path, query, show_hidden, cancel).map(|outcome| outcome.entries)
}

pub fn search_directory_with_status(
    path: &Path,
    query: &str,
    show_hidden: bool,
    cancel: &AtomicBool,
) -> io::Result<SearchOutcome> {
    search_with_deadline(path, query, show_hidden, cancel, SEARCH_DEADLINE)
}
fn search_with_deadline(
    path: &Path,
    query: &str,
    show_hidden: bool,
    cancel: &AtomicBool,
    deadline: Duration,
) -> io::Result<SearchOutcome> {
    check_cancelled(cancel)?;

    let root = fs::read_dir(path)?;
    let icons = crate::opening::MimeDetector::new();
    let mut pending = vec![(path.to_path_buf(), 0usize, Some(root))];
    let query = query.to_lowercase();
    let mut results = Vec::new();
    let started = Instant::now();
    let expired = |started: Instant| started.elapsed() >= deadline;
    let finish = |results: &mut Vec<Entry>, stop: SearchStop| {
        sort_entries(results, SortBy::Name, false);
        SearchOutcome {
            entries: std::mem::take(results),
            stop,
        }
    };
    while let Some((directory, depth, ready)) = pending.pop() {
        check_cancelled(cancel)?;
        if expired(started) {
            return Ok(finish(&mut results, SearchStop::Deadline));
        }
        let iter = match ready {
            Some(iter) => iter,
            None => match fs::read_dir(directory) {
                Ok(iter) => iter,
                Err(_) => continue,
            },
        };
        for item in iter {
            check_cancelled(cancel)?;
            if expired(started) {
                return Ok(finish(&mut results, SearchStop::Deadline));
            }
            let Ok(item) = item else { continue };
            let path = item.path();
            if !show_hidden && hidden(&path) {
                continue;
            }
            let Ok(item) = entry(path, &icons) else {
                continue;
            };
            if item.is_dir && !item.is_symlink && depth < 64 {
                pending.push((item.path.clone(), depth + 1, None));
            }
            if item.name.to_lowercase().contains(&query) {
                results.push(item);
                if results.len() == SEARCH_LIMIT {
                    return Ok(finish(&mut results, SearchStop::ResultLimit));
                }
            }
        }
    }
    check_cancelled(cancel)?;
    Ok(finish(&mut results, SearchStop::Complete))
}

pub fn sort_entries(entries: &mut [Entry], sort: SortBy, descending: bool) {
    entries.sort_by(|a, b| {
        let folders = b.is_dir.cmp(&a.is_dir);
        if folders != Ordering::Equal {
            return folders;
        }
        let primary = match sort {
            SortBy::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortBy::Size => a.size.cmp(&b.size),
            SortBy::Modified => a.modified.cmp(&b.modified),
            SortBy::Kind => a.kind.cmp(&b.kind),
        }
        .then_with(|| a.path.cmp(&b.path));
        if descending {
            primary.reverse()
        } else {
            primary
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn directory_icons_cover_exe_apk_office_and_unreadable_special_files() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        for name in [
            "Setup.EXE",
            "game.APK",
            "report.docx",
            "custom.unknown",
            "run",
        ] {
            fs::write(root.path().join(name), b"not opened for icon lookup").unwrap();
        }
        fs::set_permissions(root.path().join("run"), fs::Permissions::from_mode(0o700)).unwrap();
        let pipe = root.path().join("live.apk");
        let name = std::ffi::CString::new(pipe.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let entries = read_directory(
            root.path(),
            false,
            SortBy::Name,
            false,
            &AtomicBool::new(false),
        )
        .unwrap();
        for (name, expected) in [
            ("Setup.EXE", "application-x-executable"),
            ("game.APK", "package-x-generic"),
            ("report.docx", "x-office-document"),
            ("custom.unknown", "text-x-generic"),
            ("run", "application-x-executable"),
            ("live.apk", "package-x-generic"),
        ] {
            let entry = entries.iter().find(|entry| entry.name == name).unwrap();
            assert!(
                entry.icon_names.iter().any(|name| name == expected),
                "{name}: {:?}",
                entry.icon_names
            );
            assert!(!entry.icon_name.is_empty());
        }
    }
    #[test]
    fn cancelled_empty_directory_does_not_install_a_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(true);
        assert_eq!(
            read_directory(root.path(), false, SortBy::Name, false, &cancel)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(
            search_directory(root.path(), "", false, &cancel)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn search_deadline_returns_partial_results_with_a_limited_status() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("match.txt"), "content").unwrap();
        let cancel = AtomicBool::new(false);
        let outcome =
            search_with_deadline(root.path(), "match", false, &cancel, Duration::ZERO).unwrap();
        assert_eq!(outcome.stop, SearchStop::Deadline);
        assert!(outcome.entries.is_empty());
        let outcome = search_directory_with_status(root.path(), "match", false, &cancel).unwrap();
        assert_eq!(outcome.stop, SearchStop::Complete);
        assert_eq!(outcome.entries.len(), 1);
    }

    #[test]
    fn search_reports_root_errors_and_handles_empty_roots() {
        let root = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        assert!(search_directory(root.path(), "file", false, &cancel)
            .unwrap()
            .is_empty());
        let missing = root.path().join("missing");
        assert_eq!(
            search_directory(&missing, "file", false, &cancel)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        let file = root.path().join("file");
        fs::write(&file, "content").unwrap();
        assert_eq!(
            search_directory(&file, "file", false, &cancel)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotADirectory
        );
    }

    #[test]
    fn names_paths_hidden_sort_and_symlink_search() {
        use std::os::unix::{ffi::OsStringExt, fs::symlink};
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("match-secret"), "secret").unwrap();
        fs::create_dir(root.path().join("z-folder")).unwrap();
        fs::write(root.path().join(".hidden"), "hidden").unwrap();
        fs::write(root.path().join("a-file"), "test").unwrap();
        let raw = root
            .path()
            .join(std::ffi::OsString::from_vec(vec![b'a', 255]));
        fs::write(&raw, "raw").unwrap();
        symlink(outside.path(), root.path().join("outside-link")).unwrap();
        let list = read_directory(
            root.path(),
            false,
            SortBy::Name,
            false,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(list[0].is_dir);
        assert!(list.iter().any(|e| e.path == raw));
        assert!(!list.iter().any(|e| e.name == ".hidden"));
        assert!(
            search_directory(root.path(), "match", false, &AtomicBool::new(false))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            read_directory(
                root.path(),
                true,
                SortBy::Name,
                false,
                &AtomicBool::new(true)
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::Interrupted
        );
    }
}
