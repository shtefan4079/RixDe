use std::{
    collections::VecDeque,
    fs,
    hash::{Hash, Hasher},
    io,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::MetadataExt,
        io::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    },
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use crate::operations::{self, Progress, Report};

const MAX_ENTRIES: usize = 32;
const MAX_ACTIONS: usize = 512;
const MAX_NODES: usize = 100_000;
const MAX_PATH_BYTES: usize = 32_768;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    digest: u64,
    device: u64,
    inode: u64,
}
impl Snapshot {
    pub(crate) fn capture(path: &Path, cancel: &AtomicBool) -> io::Result<Self> {
        fn walk(
            path: &Path,
            relative: &Path,
            cancel: &AtomicBool,
            hash: &mut impl Hasher,
            nodes: &mut usize,
            depth: usize,
        ) -> io::Result<()> {
            check_cancel(cancel)?;
            *nodes += 1;
            if *nodes > MAX_NODES || depth > 256 || relative.as_os_str().len() > MAX_PATH_BYTES {
                return Err(io::Error::other(
                    "This folder exceeds the undo history limits",
                ));
            }
            let before = fs::symlink_metadata(path)?;
            relative.as_os_str().as_bytes().hash(hash);
            if before.is_dir() {

                (
                    before.dev(),
                    before.ino(),
                    before.mode(),
                    before.uid(),
                    before.gid(),
                    before.nlink(),
                )
                    .hash(hash);
            } else {
                metadata_key(&before).hash(hash);
            }
            if before.is_dir() {
                let mut names = Vec::new();
                for entry in fs::read_dir(path)? {
                    check_cancel(cancel)?;
                    if names.len() >= MAX_NODES {
                        return Err(io::Error::other(
                            "This folder exceeds the undo history limits",
                        ));
                    }
                    names.push(entry?.file_name());
                }
                names.sort();
                for name in names {
                    walk(
                        &path.join(&name),
                        &relative.join(name),
                        cancel,
                        hash,
                        nodes,
                        depth + 1,
                    )?;
                }
            } else if before.is_symlink() {
                fs::read_link(path)?.as_os_str().as_bytes().hash(hash);
            } else if !before.is_file() {
                return Err(io::Error::other(
                    "Undo is unavailable for devices, sockets and FIFOs",
                ));
            }
            if metadata_key(&before) != metadata_key(&fs::symlink_metadata(path)?) {
                return Err(io::Error::other(
                    "The file or folder changed while recording undo",
                ));
            }
            Ok(())
        }
        validate_path(path)?;
        let metadata = fs::symlink_metadata(path)?;
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        walk(path, Path::new(""), cancel, &mut hash, &mut 0, 0)?;
        Ok(Self {
            digest: hash.finish(),
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    fn verify(&self, path: &Path, cancel: &AtomicBool) -> io::Result<()> {
        if self != &Self::capture(path, cancel)? {
            return Err(io::Error::other(
                "The file or folder changed since the operation; it has been preserved",
            ));
        }
        Ok(())
    }
}
fn metadata_key(meta: &fs::Metadata) -> (u64, u64, u64, u32, u32, u32, u64, i64, i64, i64, i64) {
    (
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mode(),
        meta.uid(),
        meta.gid(),
        meta.nlink(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    )
}
fn check_cancel(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(io::ErrorKind::Interrupted.into())
    } else {
        Ok(())
    }
}
fn validate_path(path: &Path) -> io::Result<()> {
    if !path.is_absolute()
        || path.file_name().is_none()
        || path.as_os_str().len() > MAX_PATH_BYTES
        || path.components().any(|c| matches!(c, Component::ParentDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Undo requires an absolute non-root path",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Action {
    operation: Step,

    refresh: Vec<Refresh>,
}
#[derive(Clone, Debug)]
struct Refresh {
    location: PathBuf,
    before: Snapshot,
    after: Snapshot,
}
#[derive(Clone, Debug)]
enum Step {
    Move {
        current: PathBuf,
        target: PathBuf,
        expected: Snapshot,
    },
    Trash {
        path: PathBuf,
        expected: Snapshot,
    },
    Restore {
        item: trash::TrashItem,
        payload: PathBuf,
        expected: Snapshot,
    },
}
struct ApplyFailure {
    error: io::Error,
    completed: Option<PathBuf>,
}
impl Action {
    fn location(&self) -> PathBuf {
        match &self.operation {
            Step::Move { current, .. } => current.clone(),
            Step::Trash { path, .. } => path.clone(),
            Step::Restore { item, .. } => item.original_path(),
        }
    }
    fn refresh(&mut self, change: &Refresh) {
        match &mut self.operation {
            Step::Move {
                current, expected, ..
            } if *current == change.location && *expected == change.before => {
                *expected = change.after.clone()
            }
            Step::Trash { path, expected }
                if *path == change.location && *expected == change.before =>
            {
                *expected = change.after.clone()
            }
            Step::Restore { item, expected, .. }
                if item.original_path() == change.location && *expected == change.before =>
            {
                *expected = change.after.clone()
            }
            _ => {}
        }
    }
    fn apply(
        &self,
        cancel: &AtomicBool,
        progress: &mut impl FnMut(Progress),
    ) -> Result<(PathBuf, Action), ApplyFailure> {
        let mut committed = None;
        let result = (|| -> io::Result<(PathBuf, Action)> {
            check_cancel(cancel)?;
            match &self.operation {
                Step::Move {
                    current,
                    target,
                    expected,
                } => {
                    expected.verify(current, cancel)?;
                    operations::relocate(current, target, cancel, progress)?;
                    committed = Some(target.clone());
                    let after = Snapshot::capture(target, &AtomicBool::new(false))?;
                    Ok((
                        target.clone(),
                        Action {
                            operation: Step::Move {
                                current: target.clone(),
                                target: current.clone(),
                                expected: after.clone(),
                            },
                            refresh: vec![
                                Refresh {
                                    location: current.clone(),
                                    before: expected.clone(),
                                    after: after.clone(),
                                },
                                Refresh {
                                    location: target.clone(),
                                    before: expected.clone(),
                                    after,
                                },
                            ],
                        },
                    ))
                }
                Step::Trash { path, expected } => {
                    expected.verify(path, cancel)?;
                    let mut report = trash_path(path, cancel);
                    if !report.completed.is_empty() {
                        committed = Some(path.clone());
                    }
                    if let Some(action) = report.undo.pop() {
                        Ok((path.clone(), action))
                    } else if report.cancelled {
                        Err(io::ErrorKind::Interrupted.into())
                    } else {
                        Err(io::Error::other(report.errors.join("\n")))
                    }
                }
                Step::Restore {
                    item,
                    payload,
                    expected,
                } => {
                    let RestoreTarget { path, volume } = trusted_restore_target(item)?;
                    expected.verify(payload, cancel)?;

                    match volume {

                        Some((mount_root, device)) => {
                            relocate_verified(&mount_root, device, payload, &path, cancel)?
                        }
                        None => operations::relocate(payload, &path, cancel, progress)?,
                    }
                    committed = Some(path.clone());
                    let after = Snapshot::capture(&path, &AtomicBool::new(false))?;

                    let _ = fs::remove_file(&item.id);
                    Ok((
                        path.clone(),
                        Action {
                            operation: Step::Trash {
                                path: path.clone(),
                                expected: after.clone(),
                            },
                            refresh: vec![Refresh {
                                location: path,
                                before: expected.clone(),
                                after,
                            }],
                        },
                    ))
                }
            }
        })();
        result.map_err(|error| ApplyFailure {
            error,
            completed: committed,
        })
    }
}

#[derive(Debug, Default)]
pub struct History {
    undo: VecDeque<Vec<Action>>,
    redo: VecDeque<Vec<Action>>,
}
impl History {
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub fn record(&mut self, mut actions: Vec<Action>) {
        self.redo.clear();
        if actions.is_empty() {
            return;
        }
        for action in &actions {
            for change in &action.refresh {
                self.refresh(change);
            }
        }
        if actions.len() > MAX_ACTIONS {
            return;
        }
        actions.reverse();
        self.undo.push_back(actions);
        self.trim();
    }
    fn refresh(&mut self, change: &Refresh) {
        for action in self.undo.iter_mut().chain(self.redo.iter_mut()).flatten() {
            action.refresh(change);
        }
    }
    fn trim(&mut self) {
        while self.undo.len() + self.redo.len() > MAX_ENTRIES
            || self
                .undo
                .iter()
                .chain(&self.redo)
                .map(Vec::len)
                .sum::<usize>()
                > MAX_ACTIONS
        {
            if self.undo.len() > 1 {
                self.undo.pop_front();
            } else if self.redo.len() > 1 {
                self.redo.pop_front();
            } else {
                break;
            }
        }
    }
    pub fn undo(&mut self, cancel: &AtomicBool, progress: impl FnMut(Progress)) -> Report {
        self.apply(false, cancel, progress)
    }
    pub fn redo(&mut self, cancel: &AtomicBool, progress: impl FnMut(Progress)) -> Report {
        self.apply(true, cancel, progress)
    }
    fn apply(
        &mut self,
        redo: bool,
        cancel: &AtomicBool,
        mut progress: impl FnMut(Progress),
    ) -> Report {
        let Some(actions) = (if redo { &mut self.redo } else { &mut self.undo }).pop_back() else {
            return Report::default();
        };
        let total = actions.len() as u64;
        let mut report = Report::default();
        let mut inverse: Vec<Action> = Vec::new();
        let mut remainder = Vec::new();
        let mut iter = actions.into_iter();
        while let Some(action) = iter.next() {
            if cancel.load(Ordering::Relaxed) {
                report.cancelled = true;
                remainder.push(action);
                remainder.extend(iter);
                break;
            }
            match action.apply(cancel, &mut progress) {
                Ok((path, opposite)) => {
                    for change in &opposite.refresh {
                        self.refresh(change);
                        for pending in &mut remainder {
                            pending.refresh(change);
                        }
                        for earlier in &mut inverse {
                            earlier.refresh(change);
                        }
                    }
                    report.completed.push(path);
                    inverse.push(opposite);
                }
                Err(ApplyFailure {
                    error,
                    completed: Some(path),
                }) => {
                    report.completed.push(path);
                    report.undo_unavailable = true;
                    report.errors.push(format!(
                        "Operation completed, but the inverse could not be recorded: {error}"
                    ));
                }
                Err(ApplyFailure {
                    error,
                    completed: None,
                }) if error.kind() == io::ErrorKind::Interrupted => {
                    report.cancelled = true;
                    remainder.push(action);
                    remainder.extend(iter);
                    break;
                }
                Err(ApplyFailure {
                    error,
                    completed: None,
                }) => {
                    report
                        .errors
                        .push(format!("{}: {error}", action.location().display()));
                    remainder.push(action);
                }
            }
            progress(Progress {
                completed: report.completed.len() as u64,
                total,
                message: if redo {
                    "Redoing".into()
                } else {
                    "Undoing".into()
                },
            });
        }
        let (source, destination) = if redo {
            (&mut self.redo, &mut self.undo)
        } else {
            (&mut self.undo, &mut self.redo)
        };
        if !remainder.is_empty() {
            source.push_back(remainder);
        }
        if !inverse.is_empty() {
            inverse.reverse();
            destination.push_back(inverse);
        }
        self.trim();
        report
    }
}

pub(crate) fn created_action(path: &Path) -> io::Result<Action> {
    Ok(Action {
        operation: Step::Trash {
            path: path.to_owned(),
            expected: Snapshot::capture(path, &AtomicBool::new(false))?,
        },
        refresh: Vec::new(),
    })
}
pub(crate) fn moved_action(
    source: &Path,
    destination: &Path,
    before: Snapshot,
) -> io::Result<Action> {
    let after = Snapshot::capture(destination, &AtomicBool::new(false))?;
    Ok(Action {
        operation: Step::Move {
            current: destination.to_owned(),
            target: source.to_owned(),
            expected: after.clone(),
        },
        refresh: vec![Refresh {
            location: source.to_owned(),
            before,
            after,
        }],
    })
}
pub(crate) fn append_action(report: &mut Report, action: io::Result<Action>) {
    match action {
        Ok(action) if !report.undo_unavailable && report.undo.len() < MAX_ACTIONS => {
            report.undo.push(action)
        }
        Ok(_) if !report.undo_unavailable => {
            report.undo_unavailable = true;
            report.undo.clear();
            report
                .errors
                .push("Operation completed, but it exceeds the undo history limits".into());
        }
        Ok(_) => {}
        Err(error) => {
            report.undo_unavailable = true;
            report.undo.clear();
            report.errors.push(format!(
                "Operation completed, but undo is unavailable: {error}"
            ));
        }
    }
}
fn finish_simple(result: io::Result<PathBuf>, before: Option<(&Path, Snapshot)>) -> Report {
    let mut report = Report::default();
    match result {
        Ok(path) => {
            let action = if let Some((original, before)) = before {
                moved_action(original, &path, before)
            } else {
                created_action(&path)
            };
            report.completed.push(path);
            append_action(&mut report, action);
        }
        Err(error) => report.errors.push(error.to_string()),
    }
    report
}

pub fn created(path: PathBuf) -> Report {
    finish_simple(Ok(path), None)
}
pub fn create_directory(parent: &Path, name: &str, cancel: &AtomicBool) -> Report {
    if cancel.load(Ordering::Relaxed) {
        return Report {
            cancelled: true,
            ..Default::default()
        };
    }
    finish_simple(operations::create_directory(parent, name), None)
}
pub fn create_file(parent: &Path, name: &str, cancel: &AtomicBool) -> Report {
    if cancel.load(Ordering::Relaxed) {
        return Report {
            cancelled: true,
            ..Default::default()
        };
    }
    finish_simple(operations::create_file(parent, name), None)
}
pub fn rename(source: &Path, name: &str, cancel: &AtomicBool) -> Report {
    let before = Snapshot::capture(source, cancel);
    if before
        .as_ref()
        .is_err_and(|error| error.kind() == io::ErrorKind::Interrupted)
    {
        return Report {
            cancelled: true,
            ..Default::default()
        };
    }
    let result = operations::rename(source, name);
    if result.as_ref().is_ok_and(|path| path == source) {
        return Report {
            skipped: vec![source.to_owned()],
            ..Default::default()
        };
    }
    match before {
        Ok(before) => finish_simple(result, Some((source, before))),
        Err(error) => {
            let mut report = Report::default();
            match result {
                Ok(path) => {
                    report.completed.push(path);
                    append_action(&mut report, Err(error));
                }
                Err(error) => report.errors.push(error.to_string()),
            }
            report
        }
    }
}

fn trash_root(item: &trash::TrashItem) -> io::Result<PathBuf> {
    let info = Path::new(&item.id);
    validate_path(info)?;
    if info.extension() != Some(std::ffi::OsStr::new("trashinfo"))
        || info.parent().and_then(Path::file_name) != Some(std::ffi::OsStr::new("info"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid trash record",
        ));
    }
    let metadata = fs::symlink_metadata(info)?;
    if !metadata.is_file() || metadata.is_symlink() || metadata.uid() != unsafe { libc::getuid() } {
        return Err(io::Error::other(
            "The trash record is not a regular file owned by this user",
        ));
    }
    info.parent()
        .and_then(Path::parent)
        .map(Path::to_owned)
        .ok_or_else(|| io::Error::other("Invalid trash record"))
}
fn trash_payload(item: &trash::TrashItem) -> io::Result<PathBuf> {
    let root = trash_root(item)?;
    let stem = Path::new(&item.id)
        .file_stem()
        .ok_or_else(|| io::Error::other("Invalid trash record"))?;
    Ok(root.join("files").join(stem))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TrashLayout {
    Home,
    Volume,
}
fn home_trash_root() -> Option<PathBuf> {
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(data_home).join("Trash"));
    }
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(|home| PathBuf::from(home).join(".local/share/Trash"))
}

fn trash_layout(trash_root: &Path) -> Option<(TrashLayout, PathBuf)> {
    let uid = unsafe { libc::getuid() }.to_string();
    if trash_root.file_name() == Some(std::ffi::OsStr::new(&format!(".Trash-{uid}"))) {
        return Some((TrashLayout::Volume, trash_root.parent()?.to_owned()));
    }
    if trash_root.file_name() == Some(std::ffi::OsStr::new(&uid))
        && trash_root.parent()?.file_name() == Some(std::ffi::OsStr::new(".Trash"))
    {
        return Some((
            TrashLayout::Volume,
            trash_root.parent()?.parent()?.to_owned(),
        ));
    }
    if home_trash_root().as_deref() == Some(trash_root) {
        return Some((TrashLayout::Home, trash_root.to_owned()));
    }
    None
}

fn nearest_existing_device(path: &Path) -> io::Result<u64> {
    let mut candidate = path;
    loop {
        match fs::metadata(candidate) {
            Ok(metadata) => return Ok(metadata.dev()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                candidate = candidate.parent().ok_or(error)?;
            }
            Err(error) => return Err(error),
        }
    }
}

fn volume_restore_target_allowed(
    trash_root: &Path,
    mount_root: &Path,
    target: &Path,
) -> io::Result<u64> {
    let device = fs::symlink_metadata(trash_root)?.dev();
    if !target.starts_with(mount_root) || nearest_existing_device(target)? != device {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "The trash record points outside its storage device; the item was not restored",
        ));
    }
    if target.starts_with(trash_root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "The trash record points inside the Trash; the item was not restored",
        ));
    }
    Ok(device)
}

struct RestoreTarget {
    path: PathBuf,
    volume: Option<(PathBuf, u64)>,
}

fn trusted_restore_target(item: &trash::TrashItem) -> io::Result<RestoreTarget> {
    let root = trash_root(item)?;
    let path = item.original_path();
    validate_path(&path)?;
    match trash_layout(&root) {
        Some((TrashLayout::Volume, mount_root)) => {
            let device = volume_restore_target_allowed(&root, &mount_root, &path)?;
            Ok(RestoreTarget {
                path,
                volume: Some((mount_root, device)),
            })
        }
        Some((TrashLayout::Home, _)) => Ok(RestoreTarget { path, volume: None }),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Unknown trash location; the item was not restored",
        )),
    }
}

fn open_directory_at(parent: RawFd, name: &std::ffi::OsStr) -> io::Result<OwnedFd> {
    let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "A trash path component contains a NUL byte",
        )
    })?;
    let descriptor = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!("A component of the restore path changed: {error}; the item was not restored"),
        ));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

fn directory_device(descriptor: RawFd) -> io::Result<u64> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(descriptor, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { stat.assume_init() }.st_dev)
}
fn ensure_directory_device(descriptor: RawFd, device: u64) -> io::Result<()> {
    if directory_device(descriptor)? == device {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "The trash record points outside its storage device; the item was not restored",
        ))
    }
}

fn open_verified_parent(
    mount_root: &Path,
    path: &Path,
    device: u64,
) -> io::Result<(OwnedFd, std::ffi::OsString)> {
    use std::os::unix::fs::OpenOptionsExt;
    let relative = path.strip_prefix(mount_root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "The trash record points outside its storage device; the item was not restored",
        )
    })?;
    let name = relative
        .file_name()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "The trash record has no file name; the item was not restored",
            )
        })?
        .to_owned();
    let mut directory: OwnedFd = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(mount_root)?
        .into();
    ensure_directory_device(directory.as_raw_fd(), device)?;
    for component in relative.parent().unwrap_or(Path::new("")).components() {
        match component {
            Component::CurDir => continue,
            Component::Normal(part) => {
                let next = open_directory_at(directory.as_raw_fd(), part)?;
                ensure_directory_device(next.as_raw_fd(), device)?;
                directory = next;
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "The trash record contains an invalid path; the item was not restored",
                ));
            }
        }
    }
    Ok((directory, name))
}

fn relocate_verified(
    mount_root: &Path,
    device: u64,
    payload: &Path,
    target: &Path,
    cancel: &AtomicBool,
) -> io::Result<()> {
    check_cancel(cancel)?;
    let (source_directory, source_name) = open_verified_parent(mount_root, payload, device)?;
    let (target_directory, target_name) = open_verified_parent(mount_root, target, device)?;
    check_cancel(cancel)?;
    let source = std::ffi::CString::new(source_name.as_bytes())?;
    let destination = std::ffi::CString::new(target_name.as_bytes())?;
    let status = unsafe {
        libc::renameat2(
            source_directory.as_raw_fd(),
            source.as_ptr(),
            target_directory.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
fn percent_decode(value: &[u8]) -> Option<Vec<u8>> {
    if value.contains(&0) {
        return None;
    }
    let mut decoded = Vec::with_capacity(value.len());
    let mut cursor = 0;
    while cursor < value.len() {
        if value[cursor] == b'%' {
            let byte = u8::from_str_radix(
                std::str::from_utf8(value.get(cursor + 1..cursor + 3)?).ok()?,
                16,
            )
            .ok()?;
            if byte == 0 {
                return None;
            }
            decoded.push(byte);
            cursor += 3;
        } else {
            decoded.push(value[cursor]);
            cursor += 1;
        }
    }
    Some(decoded)
}
fn original_from_record(trash_root: &Path, encoded: &[u8]) -> Option<PathBuf> {
    let decoded = PathBuf::from(std::ffi::OsString::from_vec(percent_decode(encoded)?));
    if decoded
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return None;
    }

    let (layout, mount_root) = trash_layout(trash_root)?;
    let path = match layout {
        TrashLayout::Home => {
            if !decoded.is_absolute() {
                return None;
            }
            decoded
        }
        TrashLayout::Volume => {
            let path = if decoded.is_absolute() {
                decoded
            } else {
                mount_root.join(decoded)
            };
            volume_restore_target_allowed(trash_root, &mount_root, &path).ok()?;
            path
        }
    };
    validate_path(&path).ok()?;
    Some(path.components().collect())
}
fn matching_trash(original: &Path, cancel: &AtomicBool) -> io::Result<Vec<trash::TrashItem>> {
    use std::io::Read;
    let roots =
        trash::os_limited::trash_folders().map_err(|error| io::Error::other(error.to_string()))?;
    let mut found = Vec::new();
    let mut count = 0;
    for root in roots {
        let Ok(entries) = fs::read_dir(root.join("info")) else {
            continue;
        };
        for entry in entries {
            check_cancel(cancel)?;
            count += 1;
            if count > MAX_NODES {
                return Err(io::Error::other(
                    "The trash exceeds the undo history limits",
                ));
            }
            let entry = entry?;
            let metadata = entry.metadata()?;
            if !metadata.is_file()
                || metadata.is_symlink()
                || metadata.uid() != unsafe { libc::getuid() }
                || metadata.len() > 65536
            {
                continue;
            }
            let path = entry.path();
            if path.extension() != Some(std::ffi::OsStr::new("trashinfo")) {
                continue;
            }
            use std::os::unix::fs::OpenOptionsExt;
            let Ok(file) = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)
            else {
                continue;
            };
            let mut bytes = Vec::new();
            file.take(65537).read_to_end(&mut bytes)?;
            if bytes.len() > 65536 {
                continue;
            }
            let Some(value) = bytes
                .split(|b| *b == b'\n')
                .find_map(|line| line.strip_prefix(b"Path="))
            else {
                continue;
            };
            let Some(decoded) =
                original_from_record(&root, value.strip_suffix(b"\r").unwrap_or(value))
            else {
                continue;
            };
            if decoded != original {
                continue;
            }
            found.push(trash::TrashItem {
                id: path.into_os_string(),
                name: original.file_name().unwrap().to_owned(),
                original_parent: original.parent().unwrap().to_owned(),
                time_deleted: 0,
            });
        }
    }
    Ok(found)
}
pub(crate) fn trash_path(path: &Path, cancel: &AtomicBool) -> Report {
    let mut report = Report::default();
    let before = match Snapshot::capture(path, cancel) {
        Ok(before) => Ok(before),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
            report.cancelled = true;
            return report;
        }
        Err(error) => Err(error),
    };
    let original = match path.parent().map(fs::canonicalize).transpose() {
        Ok(Some(parent)) => parent.join(path.file_name().unwrap()),
        Ok(None) => {
            report
                .errors
                .push("Cannot trash the filesystem root".into());
            return report;
        }
        Err(error) => {
            report.errors.push(error.to_string());
            return report;
        }
    };
    let prior = match matching_trash(&original, cancel) {
        Ok(items) => items,
        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
            report.cancelled = true;
            return report;
        }

        Err(_) => Vec::new(),
    };
    if let Err(error) = check_cancel(cancel) {
        report.cancelled = error.kind() == io::ErrorKind::Interrupted;
        return report;
    }
    match trash::delete(&original) {
        Ok(()) => report.completed.push(path.to_owned()),
        Err(error) => {
            report.errors.push(error.to_string());
            return report;
        }
    }
    let action = (|| {
        let before = before?;
        let candidates: Vec<_> = matching_trash(&original, &AtomicBool::new(false))?
            .into_iter()
            .filter(|item| !prior.iter().any(|old| old.id == item.id))
            .collect();
        if candidates.len() != 1 {
            return Err(io::Error::other("Cannot identify the exact trashed entry"));
        }
        let item = candidates.into_iter().next().unwrap();
        let payload = trash_payload(&item)?;
        let after = Snapshot::capture(&payload, &AtomicBool::new(false))?;
        Ok(Action {
            operation: Step::Restore {
                item,
                payload,
                expected: after.clone(),
            },
            refresh: vec![Refresh {
                location: path.to_owned(),
                before,
                after,
            }],
        })
    })();
    append_action(&mut report, action);
    report
}
pub fn restore(
    items: Vec<trash::TrashItem>,
    cancel: &AtomicBool,
    mut progress: impl FnMut(Progress),
) -> Report {
    let mut report = Report::default();
    let total = items.len() as u64;
    for item in items {
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }
        let path = item.original_path();
        let action = (|| -> Result<(PathBuf, Action), ApplyFailure> {
            let payload = trash_payload(&item).map_err(|error| ApplyFailure {
                error,
                completed: None,
            })?;
            let expected = Snapshot::capture(&payload, cancel).map_err(|error| ApplyFailure {
                error,
                completed: None,
            })?;
            Action {
                operation: Step::Restore {
                    item,
                    payload,
                    expected,
                },
                refresh: Vec::new(),
            }
            .apply(cancel, &mut progress)
        })();
        match action {
            Ok((path, action)) => {
                report.completed.push(path);
                append_action(&mut report, Ok(action));
            }
            Err(ApplyFailure {
                error,
                completed: Some(path),
            }) => {
                report.completed.push(path);
                append_action(&mut report, Err(error));
            }
            Err(ApplyFailure {
                error,
                completed: None,
            }) if error.kind() == io::ErrorKind::Interrupted => {
                report.cancelled = true;
                break;
            }
            Err(ApplyFailure {
                error,
                completed: None,
            }) => report.errors.push(format!("{}: {error}", path.display())),
        }
        progress(Progress {
            completed: report.completed.len() as u64,
            total,
            message: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        });
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsString, os::unix::fs::symlink};
    const PRIVATE: &str = "RIX_HISTORY_FIXTURE_ROOT";
    fn isolated(name: &str, test: impl FnOnce(&Path)) {
        if let Some(root) = std::env::var_os(PRIVATE) {
            let root = PathBuf::from(root);
            assert_eq!(
                std::env::var_os("HOME"),
                Some(root.clone().into_os_string())
            );
            assert!(root
                .file_name()
                .unwrap()
                .as_bytes()
                .starts_with(b"rix-history-test-"));
            test(&root);
            return;
        }
        let root = tempfile::Builder::new()
            .prefix("rix-history-test-")
            .tempdir()
            .unwrap();
        fs::create_dir_all(root.path().join("data/Trash/files")).unwrap();
        fs::create_dir_all(root.path().join("data/Trash/info")).unwrap();
        fs::create_dir(root.path().join("temp")).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("history::tests::{name}"), "--nocapture"])
            .env(PRIVATE, root.path())
            .env("HOME", root.path())
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("TMPDIR", root.path().join("temp"))
            .status()
            .unwrap();
        assert!(status.success(), "isolated history test failed: {name}");
    }
    fn checked(report: Report) -> Vec<Action> {
        assert!(!report.cancelled);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!report.completed.is_empty());
        report.undo
    }
    fn cancel() -> AtomicBool {
        AtomicBool::new(false)
    }
    fn apply(history: &mut History, redo: bool) {
        let report = if redo {
            history.redo(&cancel(), |_| {})
        } else {
            history.undo(&cancel(), |_| {})
        };
        assert!(
            !report.completed.is_empty(),
            "direction redo={redo}, errors={:?}, history={history:?}",
            report.errors
        );
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!report.cancelled);
    }
    fn request(
        sources: Vec<PathBuf>,
        destination: &Path,
        kind: operations::Kind,
    ) -> operations::Request {
        operations::Request {
            kind,
            sources,
            destination: Some(destination.to_owned()),
            conflict: operations::ConflictPolicy::KeepBoth,
        }
    }
    #[test]
    fn copy_tree_undo_redo_keeps_source_and_never_clobbers_a_replacement() {
        isolated(
            "copy_tree_undo_redo_keeps_source_and_never_clobbers_a_replacement",
            |root| {
                let source = root.join("source");
                let dest = root.join("dest");
                fs::create_dir(&source).unwrap();
                fs::create_dir(&dest).unwrap();
                fs::write(source.join("data"), "contents").unwrap();
                symlink(&source, source.join("backlink")).unwrap();
                let mut history = History::default();
                history.record(checked(operations::execute(
                    request(vec![source.clone()], &dest, operations::Kind::Copy),
                    &cancel(),
                    |_| {},
                )));
                let copied = dest.join("source");
                apply(&mut history, false);
                assert!(!copied.exists());
                assert!(source.join("data").exists());
                fs::create_dir(&copied).unwrap();
                fs::write(copied.join("replacement"), "keep").unwrap();
                let report = history.redo(&cancel(), |_| {});
                assert!(report.completed.is_empty());
                assert!(!report.errors.is_empty());
                assert_eq!(
                    fs::read_to_string(copied.join("replacement")).unwrap(),
                    "keep"
                );
                fs::remove_dir_all(&copied).unwrap();
                apply(&mut history, true);
                assert_eq!(fs::read_to_string(copied.join("data")).unwrap(), "contents");
                assert_eq!(fs::read_link(copied.join("backlink")).unwrap(), source);
            },
        );
    }
    #[test]
    fn changed_copy_or_added_directory_child_is_preserved() {
        isolated(
            "changed_copy_or_added_directory_child_is_preserved",
            |root| {
                let source = root.join("source");
                let dest = root.join("dest");
                fs::create_dir(&source).unwrap();
                fs::create_dir(&dest).unwrap();
                fs::write(source.join("data"), "old").unwrap();
                let mut history = History::default();
                history.record(checked(operations::execute(
                    request(vec![source], &dest, operations::Kind::Copy),
                    &cancel(),
                    |_| {},
                )));
                let copied = dest.join("source");
                fs::write(copied.join("new"), "must survive").unwrap();
                let report = history.undo(&cancel(), |_| {});
                assert!(report.completed.is_empty());
                assert!(!report.errors.is_empty());
                assert_eq!(
                    fs::read_to_string(copied.join("new")).unwrap(),
                    "must survive"
                );
                fs::remove_file(copied.join("new")).unwrap();
                fs::write(copied.join("data"), "edited").unwrap();
                let report = history.undo(&cancel(), |_| {});
                assert!(report.completed.is_empty());
                assert_eq!(fs::read_to_string(copied.join("data")).unwrap(), "edited");
            },
        );
    }
    #[test]
    fn create_rename_and_child_cycles_keep_older_history_valid() {
        isolated(
            "create_rename_and_child_cycles_keep_older_history_valid",
            |root| {
                let mut history = History::default();
                let folder = root.join("folder");
                let file = folder.join("file");
                history.record(checked(create_directory(root, "folder", &cancel())));
                history.record(checked(create_file(&folder, "file", &cancel())));
                history.record(checked(rename(&file, "renamed", &cancel())));
                apply(&mut history, false);
                assert!(file.exists());
                apply(&mut history, false);
                assert!(!file.exists());
                apply(&mut history, false);
                assert!(!folder.exists());
                assert!(!history.can_undo());
                apply(&mut history, true);
                apply(&mut history, true);
                apply(&mut history, true);
                assert!(folder.join("renamed").exists());
                assert!(!file.exists());
                assert!(!history.can_redo());
                apply(&mut history, false);
                apply(&mut history, false);
                apply(&mut history, false);
                assert!(!folder.exists());
            },
        );
    }
    #[test]
    fn move_and_rename_restore_raw_names_without_overwriting() {
        isolated(
            "move_and_rename_restore_raw_names_without_overwriting",
            |root| {
                let source = root.join(OsString::from_vec(b"raw-\xff".to_vec()));
                let dest = root.join("dest");
                fs::create_dir(&dest).unwrap();
                fs::write(&source, "raw path").unwrap();
                let mut history = History::default();
                history.record(checked(rename(&source, "renamed", &cancel())));
                let renamed = root.join("renamed");
                history.record(checked(operations::execute(
                    request(vec![renamed.clone()], &dest, operations::Kind::Move),
                    &cancel(),
                    |_| {},
                )));
                fs::write(&renamed, "replacement").unwrap();
                let report = history.undo(&cancel(), |_| {});
                assert!(report.completed.is_empty());
                assert_eq!(fs::read_to_string(&renamed).unwrap(), "replacement");
                fs::remove_file(&renamed).unwrap();
                apply(&mut history, false);
                apply(&mut history, false);
                assert_eq!(fs::read_to_string(&source).unwrap(), "raw path");
                apply(&mut history, true);
                apply(&mut history, true);
                assert_eq!(
                    fs::read_to_string(dest.join("renamed")).unwrap(),
                    "raw path"
                );
            },
        );
    }
    #[test]
    fn cancelled_batch_keeps_completed_inverses_and_remainder() {
        isolated(
            "cancelled_batch_keeps_completed_inverses_and_remainder",
            |root| {
                let first = root.join("first");
                let second = root.join("second");
                let mut history = History::default();
                let mut actions = checked(create_file(root, "first", &cancel()));
                actions.extend(checked(create_file(root, "second", &cancel())));
                history.record(actions);
                let stop = cancel();
                let report = history.undo(&stop, |progress| {
                    if progress.completed == 1 {
                        stop.store(true, Ordering::Relaxed);
                    }
                });
                assert_eq!(report.completed.len(), 1);
                assert!(report.cancelled);
                assert!(first.exists());
                assert!(!second.exists());
                assert!(history.can_undo() && history.can_redo());
                apply(&mut history, false);
                assert!(!first.exists());
                apply(&mut history, true);
                apply(&mut history, true);
                assert!(first.exists() && second.exists());
                let stopped = history.undo(&AtomicBool::new(true), |_| {});
                assert!(stopped.cancelled);
                assert!(stopped.completed.is_empty());
                assert!(first.exists() && second.exists());
            },
        );
    }
    #[test]
    fn operation_partial_failure_journals_only_published_files() {
        isolated(
            "operation_partial_failure_journals_only_published_files",
            |root| {
                let source = root.join("first");
                let unsupported = root.join("pipe");
                let dest = root.join("dest");
                fs::create_dir(&dest).unwrap();
                fs::write(&source, "first").unwrap();
                let name = std::ffi::CString::new(unsupported.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                let report = operations::execute(
                    request(
                        vec![source.clone(), unsupported.clone()],
                        &dest,
                        operations::Kind::Copy,
                    ),
                    &cancel(),
                    |_| {},
                );
                assert_eq!(report.completed, vec![dest.join("first")]);
                assert_eq!(report.undo.len(), 1);
                assert_eq!(report.errors.len(), 1);
                let mut history = History::default();
                history.record(report.undo);
                apply(&mut history, false);
                assert!(!dest.join("first").exists());
                assert!(source.exists() && unsupported.exists());
                apply(&mut history, true);
                assert_eq!(fs::read_to_string(dest.join("first")).unwrap(), "first");
            },
        );
    }
    #[test]
    fn trash_roundtrip_selects_exact_item_despite_duplicate_original_paths() {
        isolated(
            "trash_roundtrip_selects_exact_item_despite_duplicate_original_paths",
            |root| {
                let source = root.join("same-name");
                let mut history = History::default();
                fs::write(&source, "old").unwrap();
                history.record(checked(trash_path(&source, &cancel())));
                fs::write(&source, "new").unwrap();
                history.record(checked(trash_path(&source, &cancel())));
                apply(&mut history, false);
                assert_eq!(fs::read_to_string(&source).unwrap(), "new");
                let report = history.undo(&cancel(), |_| {});
                assert!(report.completed.is_empty());
                assert!(!report.errors.is_empty());
                assert_eq!(fs::read_to_string(&source).unwrap(), "new");
                fs::rename(&source, root.join("keep-new")).unwrap();
                apply(&mut history, false);
                assert_eq!(fs::read_to_string(&source).unwrap(), "old");
                assert_eq!(fs::read_to_string(root.join("keep-new")).unwrap(), "new");
            },
        );
    }
    #[test]
    fn trash_restore_symlinks_and_non_utf8_names_never_follow_targets() {
        isolated(
            "trash_restore_symlinks_and_non_utf8_names_never_follow_targets",
            |root| {
                let target = root.join("target");
                fs::create_dir(&target).unwrap();
                fs::write(target.join("keep"), "contents").unwrap();
                let link = root.join(OsString::from_vec(b"link-\xfe".to_vec()));
                symlink(&target, &link).unwrap();
                let mut report = trash_path(&link, &cancel());
                assert!(report.errors.is_empty(), "{:?}", report.errors);
                let item = match report.undo.pop().unwrap().operation {
                    Step::Restore { item, .. } => item,
                    _ => panic!("expected restore"),
                };
                let mut history = History::default();
                history.record(checked(restore(vec![item], &cancel(), |_| {})));
                assert_eq!(fs::read_link(&link).unwrap(), target);
                apply(&mut history, false);
                assert!(fs::symlink_metadata(&link).is_err());
                apply(&mut history, true);
                assert_eq!(fs::read_link(&link).unwrap(), target);
                assert_eq!(fs::read_to_string(target.join("keep")).unwrap(), "contents");
            },
        );
    }
    #[test]
    fn history_bounds_and_new_irreversible_work_invalidate_redo() {
        isolated(
            "history_bounds_and_new_irreversible_work_invalidate_redo",
            |root| {
                let mut history = History::default();
                for index in 0..40 {
                    history.record(checked(create_file(
                        root,
                        &format!("file-{index}"),
                        &cancel(),
                    )));
                }
                assert_eq!(history.undo.len(), MAX_ENTRIES);
                apply(&mut history, false);
                assert!(history.can_redo());
                history.record(Vec::new());
                assert!(!history.can_redo());
                assert!(history.can_undo());
                assert!(root.join("file-0").exists());
            },
        );
    }
    #[test]
    fn stale_inode_modified_creation_and_dangling_restore_collision_are_preserved() {
        isolated(
            "stale_inode_modified_creation_and_dangling_restore_collision_are_preserved",
            |root| {
                let path = root.join("created");
                let mut history = History::default();
                history.record(checked(create_file(root, "created", &cancel())));
                fs::write(&path, "edited").unwrap();
                assert!(history.undo(&cancel(), |_| {}).completed.is_empty());
                assert_eq!(fs::read_to_string(&path).unwrap(), "edited");
                fs::remove_file(&path).unwrap();
                fs::write(&path, "replacement").unwrap();
                assert!(history.undo(&cancel(), |_| {}).completed.is_empty());
                assert_eq!(fs::read_to_string(&path).unwrap(), "replacement");
                let source = root.join("restore");
                fs::write(&source, "restore data").unwrap();
                let mut h = History::default();
                h.record(checked(trash_path(&source, &cancel())));
                symlink(root.join("absent"), &source).unwrap();
                let failed = h.undo(&cancel(), |_| {});
                assert!(failed.completed.is_empty());
                assert!(!failed.errors.is_empty());
                assert!(fs::symlink_metadata(&source).unwrap().is_symlink());
                assert!(!root.join("absent").exists());
                fs::remove_file(&source).unwrap();
                apply(&mut h, false);
                assert_eq!(fs::read_to_string(&source).unwrap(), "restore data");
            },
        );
    }
    #[test]
    fn cross_filesystem_move_undo_uses_exact_names_and_cancels_without_partial_publication() {
        isolated(
            "cross_filesystem_move_undo_uses_exact_names_and_cancels_without_partial_publication",
            |root| {
                let Ok(destination) = tempfile::Builder::new()
                    .prefix("rix-history-test-cross-")
                    .tempdir_in("/dev/shm")
                else {
                    return;
                };
                if fs::metadata(root).unwrap().dev()
                    == fs::metadata(destination.path()).unwrap().dev()
                {
                    return;
                }
                let source = root.join("folder");
                fs::create_dir(&source).unwrap();
                fs::write(source.join("data"), vec![7; 3 * 1024 * 1024]).unwrap();
                symlink("data", source.join("link")).unwrap();
                fs::create_dir(destination.path().join("folder")).unwrap();
                fs::write(destination.path().join("folder/keep"), "existing").unwrap();
                let mut history = History::default();
                history.record(checked(operations::execute(
                    request(
                        vec![source.clone()],
                        destination.path(),
                        operations::Kind::Move,
                    ),
                    &cancel(),
                    |_| {},
                )));
                let moved = destination.path().join("folder (copy 1)");
                assert!(moved.exists());
                assert!(!source.exists());
                let stop = cancel();
                let report = history.undo(&stop, |_| stop.store(true, Ordering::Relaxed));
                assert!(report.cancelled);
                assert!(report.completed.is_empty());
                assert!(!source.exists());
                assert!(moved.exists());
                assert!(!fs::read_dir(root)
                    .unwrap()
                    .filter_map(Result::ok)
                    .any(|entry| entry.file_name().as_bytes().starts_with(b".rix-move-")));
                apply(&mut history, false);
                assert!(source.exists());
                assert!(!moved.exists());
                assert_eq!(
                    fs::read_link(source.join("link")).unwrap(),
                    Path::new("data")
                );
                assert_eq!(
                    fs::read(source.join("data")).unwrap(),
                    vec![7; 3 * 1024 * 1024]
                );
                apply(&mut history, true);
                assert!(!source.exists());
                assert!(moved.exists());
                apply(&mut history, false);
                assert!(source.exists());
                assert_eq!(
                    fs::read_to_string(destination.path().join("folder/keep")).unwrap(),
                    "existing"
                );
            },
        );
    }
    #[test]
    fn volume_trash_records_resolve_both_layouts_and_preserve_raw_bytes() {
        isolated(
            "volume_trash_records_resolve_both_layouts_and_preserve_raw_bytes",
            |root| {
                let volume = root.join("volume");
                let uid = unsafe { libc::getuid() }.to_string();
                let expected = volume
                    .join("nested")
                    .join(OsString::from_vec(b"raw-\xff.txt".to_vec()));
                for trash_root in [
                    volume.join(format!(".Trash-{uid}")),
                    volume.join(".Trash").join(&uid),
                ] {
                    fs::create_dir_all(trash_root.join("info")).unwrap();
                    fs::create_dir_all(trash_root.join("files")).unwrap();
                    let metadata = trash_root.join("info/fixture.trashinfo");
                    fs::write(&metadata, b"[Trash Info]\nPath=nested/raw-%FF.txt\nDeletionDate=2026-10-07T12:00:00\n").unwrap();
                    fs::write(trash_root.join("files/fixture"), "mounted record").unwrap();
                    let bytes = fs::read(metadata).unwrap();
                    let value = bytes
                        .split(|byte| *byte == b'\n')
                        .find_map(|line| line.strip_prefix(b"Path="))
                        .unwrap();
                    assert_eq!(
                        original_from_record(&trash_root, value),
                        Some(expected.clone())
                    );
                    assert!(original_from_record(&trash_root, b"../outside").is_none());
                    assert!(original_from_record(&trash_root, b"nested/%2E%2E/outside").is_none());
                    assert!(original_from_record(&trash_root, b"nul-%00").is_none());
                    assert!(original_from_record(&trash_root, b"bad-%ZZ").is_none());
                    assert!(original_from_record(&trash_root, b"/").is_none());
                }
                let home_trash = root.join("data/Trash");
                assert!(original_from_record(&home_trash, b"nested/file").is_none());
                assert_eq!(
                    original_from_record(&home_trash, expected.as_os_str().as_bytes()),
                    Some(expected)
                );
            },
        );
    }

    fn other_device_path(reference: &Path) -> PathBuf {
        let device = fs::symlink_metadata(reference).unwrap().dev();
        ["/proc", "/sys", "/dev/shm", "/"]
            .iter()
            .map(PathBuf::from)
            .find(|candidate| {
                fs::symlink_metadata(candidate).is_ok_and(|metadata| metadata.dev() != device)
            })
            .expect("this test needs a second mounted filesystem")
    }
    fn volume_fixture(root: &Path) -> PathBuf {
        let volume = root.join("volume");
        let uid = unsafe { libc::getuid() }.to_string();
        let trash_root = volume.join(format!(".Trash-{uid}"));
        fs::create_dir_all(trash_root.join("info")).unwrap();
        fs::create_dir_all(trash_root.join("files")).unwrap();
        trash_root
    }
    #[test]
    fn volume_trash_absolute_records_must_stay_on_the_volume_device() {
        isolated(
            "volume_trash_absolute_records_must_stay_on_the_volume_device",
            |root| {
                let volume = root.join("volume");
                let uid = unsafe { libc::getuid() }.to_string();
                let expected = volume
                    .join("nested")
                    .join(OsString::from_vec(b"raw-\xff.txt".to_vec()));
                for trash_root in [
                    volume.join(format!(".Trash-{uid}")),
                    volume.join(".Trash").join(&uid),
                ] {
                    fs::create_dir_all(trash_root.join("info")).unwrap();
                    fs::create_dir_all(trash_root.join("files")).unwrap();
                    let metadata = trash_root.join("info/fixture.trashinfo");
                    fs::write(&metadata, b"[Trash Info]\nPath=nested/raw-%FF.txt\nDeletionDate=2026-10-07T12:00:00\n").unwrap();
                    let bytes = fs::read(metadata).unwrap();
                    let value = bytes
                        .split(|byte| *byte == b'\n')
                        .find_map(|line| line.strip_prefix(b"Path="))
                        .unwrap();

                    assert_eq!(
                        original_from_record(&trash_root, value),
                        Some(expected.clone())
                    );

                    assert_eq!(
                        original_from_record(&trash_root, expected.as_os_str().as_bytes()),
                        Some(expected.clone())
                    );

                    let foreign = other_device_path(&trash_root).join("autostart/agent.desktop");
                    assert!(
                        original_from_record(&trash_root, foreign.as_os_str().as_bytes()).is_none()
                    );
                    let inside = trash_root.join("files/injected");
                    assert!(
                        original_from_record(&trash_root, inside.as_os_str().as_bytes()).is_none()
                    );
                }
            },
        );
    }
    #[test]
    fn volume_trash_restore_rejects_foreign_and_inside_trash_records() {
        isolated(
            "volume_trash_restore_rejects_foreign_and_inside_trash_records",
            |root| {
                let trash_root = volume_fixture(root);
                let foreign = other_device_path(&trash_root).join("autostart/agent.desktop");
                fs::write(
                    trash_root.join("info/foreign.trashinfo"),
                    b"[Trash Info]\nPath=/home/user/.config/autostart/agent.desktop\nDeletionDate=2026-10-07T12:00:00\n",
                )
                .unwrap();
                fs::write(trash_root.join("files/foreign"), "payload").unwrap();
                let report = restore(
                    vec![trash::TrashItem {
                        id: trash_root.join("info/foreign.trashinfo").into_os_string(),
                        name: "agent.desktop".into(),
                        original_parent: foreign.parent().unwrap().to_owned(),
                        time_deleted: 0,
                    }],
                    &cancel(),
                    |_| {},
                );
                assert!(report.completed.is_empty());
                assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
                assert!(
                    report.errors[0].contains("storage device"),
                    "{:?}",
                    report.errors
                );
                assert!(fs::symlink_metadata(&foreign).is_err());
                assert!(trash_root.join("files/foreign").exists());

                let inside = trash_root.join("files/restored-inside");
                fs::write(
                    trash_root.join("info/inside.trashinfo"),
                    b"[Trash Info]\nPath=/volume/.Trash-1000/files/restored-inside\nDeletionDate=2026-10-07T12:00:00\n",
                )
                .unwrap();
                fs::write(trash_root.join("files/inside"), "payload").unwrap();
                let report = restore(
                    vec![trash::TrashItem {
                        id: trash_root.join("info/inside.trashinfo").into_os_string(),
                        name: "restored-inside".into(),
                        original_parent: trash_root.join("files"),
                        time_deleted: 0,
                    }],
                    &cancel(),
                    |_| {},
                );
                assert!(report.completed.is_empty());
                assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
                assert!(
                    report.errors[0].contains("inside the Trash"),
                    "{:?}",
                    report.errors
                );
                assert!(fs::symlink_metadata(&inside).is_err());
                assert!(trash_root.join("files/inside").exists());
            },
        );
    }
    #[test]
    fn volume_trash_relative_restore_publishes_onto_the_volume() {
        isolated(
            "volume_trash_relative_restore_publishes_onto_the_volume",
            |root| {
                let trash_root = volume_fixture(root);
                let volume = trash_root.parent().unwrap().to_owned();
                fs::create_dir_all(volume.join("nested")).unwrap();
                let target = volume.join("nested/restored.txt");
                fs::write(
                    trash_root.join("info/relative.trashinfo"),
                    b"[Trash Info]\nPath=nested/restored.txt\nDeletionDate=2026-10-07T12:00:00\n",
                )
                .unwrap();
                fs::write(trash_root.join("files/relative"), "restored").unwrap();
                let report = restore(
                    vec![trash::TrashItem {
                        id: trash_root.join("info/relative.trashinfo").into_os_string(),
                        name: "restored.txt".into(),
                        original_parent: volume.join("nested"),
                        time_deleted: 0,
                    }],
                    &cancel(),
                    |_| {},
                );
                assert!(report.errors.is_empty(), "{:?}", report.errors);
                assert_eq!(report.completed, vec![target.clone()]);
                assert_eq!(fs::read_to_string(&target).unwrap(), "restored");
                assert!(!trash_root.join("files/relative").exists());
            },
        );
    }
    #[test]
    fn volume_trash_restore_refuses_a_symlinked_destination_component() {
        isolated(
            "volume_trash_restore_refuses_a_symlinked_destination_component",
            |root| {
                let trash_root = volume_fixture(root);
                let volume = trash_root.parent().unwrap().to_owned();
                let victim = root.join("victim");
                fs::create_dir(&victim).unwrap();

                symlink(&victim, volume.join("redirected")).unwrap();
                let target = volume.join("redirected/item.txt");
                assert_eq!(
                    original_from_record(&trash_root, b"redirected/item.txt"),
                    Some(target.clone())
                );
                fs::write(
                    trash_root.join("info/symlinked.trashinfo"),
                    b"[Trash Info]\nPath=redirected/item.txt\nDeletionDate=2026-10-07T12:00:00\n",
                )
                .unwrap();
                fs::write(trash_root.join("files/symlinked"), "payload").unwrap();
                let report = restore(
                    vec![trash::TrashItem {
                        id: trash_root.join("info/symlinked.trashinfo").into_os_string(),
                        name: "item.txt".into(),
                        original_parent: volume.join("redirected"),
                        time_deleted: 0,
                    }],
                    &cancel(),
                    |_| {},
                );
                assert!(report.completed.is_empty());
                assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
                assert!(report.errors[0].contains("changed"), "{:?}", report.errors);
                assert!(fs::symlink_metadata(victim.join("item.txt")).is_err());
                assert!(trash_root.join("files/symlinked").exists());
            },
        );
    }
    #[test]
    fn home_trash_absolute_records_restore_without_device_checks() {
        isolated(
            "home_trash_absolute_records_restore_without_device_checks",
            |root| {
                let home_trash = root.join("data/Trash");
                let parent = root.join("restored-home");
                fs::create_dir(&parent).unwrap();
                let target = parent.join("item.txt");
                fs::write(
                    home_trash.join("info/home.trashinfo"),
                    b"[Trash Info]\nPath=/restored-home/item.txt\nDeletionDate=2026-10-07T12:00:00\n",
                )
                .unwrap();
                fs::write(home_trash.join("files/home"), "home contents").unwrap();
                assert_eq!(
                    original_from_record(&home_trash, target.as_os_str().as_bytes()),
                    Some(target.clone())
                );
                let report = restore(
                    vec![trash::TrashItem {
                        id: home_trash.join("info/home.trashinfo").into_os_string(),
                        name: "item.txt".into(),
                        original_parent: parent,
                        time_deleted: 0,
                    }],
                    &cancel(),
                    |_| {},
                );
                assert!(report.errors.is_empty(), "{:?}", report.errors);
                assert_eq!(report.completed, vec![target.clone()]);
                assert_eq!(fs::read_to_string(&target).unwrap(), "home contents");
            },
        );
    }
}
