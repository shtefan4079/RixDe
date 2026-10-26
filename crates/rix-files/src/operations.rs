use std::{
    collections::HashMap,
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::fd::AsRawFd,
    os::unix::{
        ffi::OsStrExt,
        fs::{symlink, DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        OnceLock,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Default)]
struct CopyState {
    preserve_owner: bool,
    hardlinks: HashMap<(u64, u64), PathBuf>,
}

struct CopyContext<'a> {
    cancel: &'a AtomicBool,
    completed: &'a mut u64,
    total: u64,
    progress: &'a mut dyn FnMut(Progress),
    state: &'a mut CopyState,
}

enum Attributes<'a> {
    File(&'a File),
    Link(CString),
}
impl Attributes<'_> {
    fn names(&self) -> io::Result<Vec<CString>> {
        for _ in 0..4 {
            let size = unsafe {
                match self {
                    Self::File(file) => libc::flistxattr(file.as_raw_fd(), std::ptr::null_mut(), 0),
                    Self::Link(path) => libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0),
                }
            };
            if size < 0 {
                let error = io::Error::last_os_error();
                return if error.raw_os_error() == Some(libc::ENOTSUP) {
                    Ok(Vec::new())
                } else {
                    Err(error)
                };
            }
            let mut names = vec![0u8; size as usize];
            let count = unsafe {
                match self {
                    Self::File(file) => {
                        libc::flistxattr(file.as_raw_fd(), names.as_mut_ptr().cast(), names.len())
                    }
                    Self::Link(path) => {
                        libc::llistxattr(path.as_ptr(), names.as_mut_ptr().cast(), names.len())
                    }
                }
            };
            if count >= 0 && count as usize > names.len() {
                continue;
            }
            if count >= 0 {
                names.truncate(count as usize);
                return names
                    .split(|byte| *byte == 0)
                    .filter(|name| !name.is_empty())
                    .map(|name| CString::new(name).map_err(Into::into))
                    .collect();
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ERANGE) {
                return Err(error);
            }
        }
        Err(io::Error::other("Source attributes changed while copying"))
    }
    fn get(&self, name: &CString) -> io::Result<Vec<u8>> {
        for _ in 0..4 {
            let size = unsafe {
                match self {
                    Self::File(file) => {
                        libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), std::ptr::null_mut(), 0)
                    }
                    Self::Link(path) => {
                        libc::lgetxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0)
                    }
                }
            };
            if size < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut value = vec![0u8; size as usize];
            let count = unsafe {
                match self {
                    Self::File(file) => libc::fgetxattr(
                        file.as_raw_fd(),
                        name.as_ptr(),
                        value.as_mut_ptr().cast(),
                        value.len(),
                    ),
                    Self::Link(path) => libc::lgetxattr(
                        path.as_ptr(),
                        name.as_ptr(),
                        value.as_mut_ptr().cast(),
                        value.len(),
                    ),
                }
            };
            if count >= 0 && count as usize > value.len() {
                continue;
            }
            if count >= 0 {
                value.truncate(count as usize);
                return Ok(value);
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ERANGE) {
                return Err(error);
            }
        }
        Err(io::Error::other("Source attributes changed while copying"))
    }
    fn set(&self, name: &CString, value: &[u8]) -> io::Result<()> {
        let result = unsafe {
            match self {
                Self::File(file) => libc::fsetxattr(
                    file.as_raw_fd(),
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                    0,
                ),
                Self::Link(path) => libc::lsetxattr(
                    path.as_ptr(),
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                    0,
                ),
            }
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    fn remove(&self, name: &CString) -> io::Result<()> {
        let result = unsafe {
            match self {
                Self::File(file) => libc::fremovexattr(file.as_raw_fd(), name.as_ptr()),
                Self::Link(path) => libc::lremovexattr(path.as_ptr(), name.as_ptr()),
            }
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}
fn preserve_attributes(source: Attributes<'_>, destination: Attributes<'_>) -> io::Result<()> {
    let mut names = source.names()?;

    names.sort_by_key(|name| name.as_bytes() == b"system.posix_acl_access");

    for name in destination.names()? {
        if matches!(
            name.as_bytes(),
            b"system.posix_acl_access" | b"system.posix_acl_default"
        ) && !names.contains(&name)
        {
            destination.remove(&name)?;
        }
    }
    for name in names {
        let value = source.get(&name)?;
        destination.set(&name, &value).map_err(|error| {
            io::Error::other(format!(
                "Cannot preserve attribute {}: {error}; original preserved",
                name.to_string_lossy()
            ))
        })?;
        if destination.get(&name)? != value {
            return Err(io::Error::other(
                "Destination changed file attributes; original preserved",
            ));
        }
    }
    Ok(())
}
fn preserve_file_metadata(
    source: &File,
    destination: &File,
    metadata: &fs::Metadata,
    owner: bool,
) -> io::Result<()> {
    let actual = destination.metadata()?;
    let uid = if owner { metadata.uid() } else { actual.uid() };
    let mut gid = metadata.gid();
    if (actual.uid(), actual.gid()) != (uid, gid)
        && unsafe { libc::fchown(destination.as_raw_fd(), uid, gid) } != 0
    {
        let error = io::Error::last_os_error();
        if !owner && error.raw_os_error() == Some(libc::EPERM) {

            gid = actual.gid();
        } else {
            return Err(io::Error::other(format!(
                "Cannot preserve file ownership: {error}; original preserved"
            )));
        }
    }

    preserve_attributes(Attributes::File(source), Attributes::File(destination))?;
    destination.set_permissions(metadata.permissions())?;
    destination.set_times(
        fs::FileTimes::new()
            .set_accessed(metadata.accessed()?)
            .set_modified(metadata.modified()?),
    )?;
    let actual = destination.metadata()?;
    if actual.mode() & 0o7777 != metadata.mode() & 0o7777
        || (actual.uid(), actual.gid()) != (uid, gid)
        || (
            actual.atime(),
            actual.atime_nsec(),
            actual.mtime(),
            actual.mtime_nsec(),
        ) != (
            metadata.atime(),
            metadata.atime_nsec(),
            metadata.mtime(),
            metadata.mtime_nsec(),
        )
    {
        return Err(io::Error::other(
            "Destination cannot preserve permissions or ownership; original preserved",
        ));
    }
    Ok(())
}
fn preserve_link_metadata(
    source: &Path,
    destination: &Path,
    metadata: &fs::Metadata,
    owner: bool,
) -> io::Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    let actual = fs::symlink_metadata(Path::new(std::ffi::OsStr::from_bytes(
        destination.as_bytes(),
    )))?;
    let uid = if owner { metadata.uid() } else { actual.uid() };
    let mut gid = metadata.gid();
    if (actual.uid(), actual.gid()) != (uid, metadata.gid())
        && unsafe { libc::lchown(destination.as_ptr(), uid, metadata.gid()) } != 0
    {
        let error = io::Error::last_os_error();
        if owner || error.raw_os_error() != Some(libc::EPERM) {
            return Err(io::Error::other(format!(
                "Cannot preserve link ownership: {error}; original preserved"
            )));
        }
        gid = actual.gid();
    }
    let times = [
        libc::timespec {
            tv_sec: metadata.atime(),
            tv_nsec: metadata.atime_nsec(),
        },
        libc::timespec {
            tv_sec: metadata.mtime(),
            tv_nsec: metadata.mtime_nsec(),
        },
    ];
    if unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            destination.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    preserve_attributes(
        Attributes::Link(source),
        Attributes::Link(destination.clone()),
    )?;
    let actual = fs::symlink_metadata(Path::new(std::ffi::OsStr::from_bytes(
        destination.as_bytes(),
    )))?;
    if (actual.uid(), actual.gid()) != (uid, gid)
        || (
            actual.atime(),
            actual.atime_nsec(),
            actual.mtime(),
            actual.mtime_nsec(),
        ) != (
            metadata.atime(),
            metadata.atime_nsec(),
            metadata.mtime(),
            metadata.mtime_nsec(),
        )
    {
        return Err(io::Error::other(
            "Destination cannot preserve link metadata; original preserved",
        ));
    }
    Ok(())
}
fn copy_file_bytes(
    input: &mut File,
    output: &mut File,
    length: u64,
    cancel: &AtomicBool,
    mut progress: impl FnMut(),
) -> io::Result<()> {
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut offset = 0u64;
    let mut sparse = true;
    while offset < length {
        interrupted(cancel)?;
        let mut end = length;
        if sparse {
            let data =
                unsafe { libc::lseek(input.as_raw_fd(), offset as libc::off_t, libc::SEEK_DATA) };
            if data < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ENXIO) {
                    break;
                }
                if error.raw_os_error() == Some(libc::EINVAL)
                    || error.raw_os_error() == Some(libc::ENOTSUP)
                {
                    sparse = false;
                } else {
                    return Err(error);
                }
            } else {
                offset = (data as u64).min(length);
                let hole = unsafe { libc::lseek(input.as_raw_fd(), data, libc::SEEK_HOLE) };
                if hole < 0 {
                    return Err(io::Error::last_os_error());
                }
                end = (hole as u64).min(length);
                if end <= offset && offset < length {
                    return Err(io::Error::other("Invalid source sparse extent"));
                }
            }
        }
        input.seek(SeekFrom::Start(offset))?;
        output.seek(SeekFrom::Start(offset))?;
        while offset < end {
            interrupted(cancel)?;
            let size = buffer.len().min((end - offset) as usize);
            input.read_exact(&mut buffer[..size])?;
            if !sparse && buffer[..size].iter().all(|byte| *byte == 0) {
                output.seek(SeekFrom::Current(size as i64))?;
            } else {
                output.write_all(&buffer[..size])?;
            }
            offset += size as u64;
            progress();
        }
    }
    output.set_len(length)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Copy,
    Move,
    Trash,
    Delete,
}
#[derive(Clone, Copy, Debug)]
pub enum ConflictPolicy {
    Skip,
    KeepBoth,
}
#[derive(Clone, Debug)]
pub struct Request {
    pub kind: Kind,
    pub sources: Vec<PathBuf>,
    pub destination: Option<PathBuf>,
    pub conflict: ConflictPolicy,
}
#[derive(Clone, Debug)]
pub struct Progress {
    pub completed: u64,
    pub total: u64,
    pub message: String,
}
#[derive(Default, Debug)]
pub struct Report {
    pub completed: Vec<PathBuf>,
    pub skipped: Vec<PathBuf>,
    pub errors: Vec<String>,

    pub warnings: Vec<String>,
    pub cancelled: bool,

    pub undo: Vec<crate::history::Action>,
    pub undo_unavailable: bool,
}

fn interrupted(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(io::ErrorKind::Interrupted.into())
    } else {
        Ok(())
    }
}
fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}
fn name_valid(name: &str) -> io::Result<()> {
    let mut components = Path::new(name).components();
    if name.is_empty()
        || name.contains('\0')
        || name.contains('/')
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Enter a single file or folder name",
        ));
    }
    Ok(())
}
fn no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    let src = CString::new(source.as_os_str().as_bytes())?;
    let dst = CString::new(destination.as_os_str().as_bytes())?;
    let status = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            src.as_ptr(),
            libc::AT_FDCWD,
            dst.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
pub fn create_directory(parent: &Path, name: &str) -> io::Result<PathBuf> {
    name_valid(name)?;
    let path = parent.join(name);
    fs::create_dir(&path)?;
    Ok(path)
}
pub fn create_file(parent: &Path, name: &str) -> io::Result<PathBuf> {
    name_valid(name)?;
    let path = parent.join(name);

    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    Ok(path)
}
pub fn rename(source: &Path, name: &str) -> io::Result<PathBuf> {
    name_valid(name)?;
    let source = normalized_source(source)?;
    let parent = source.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Cannot rename the filesystem root",
        )
    })?;
    let destination = parent.join(name);
    if source == destination {
        return Ok(destination);
    }
    no_replace(&source, &destination)?;
    Ok(destination)
}
fn destination_for(
    source: &Path,
    directory: &Path,
    conflict: ConflictPolicy,
) -> io::Result<Option<PathBuf>> {
    let name = source.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Cannot copy the filesystem root",
        )
    })?;
    let destination = directory.join(name);
    if !exists(&destination) {
        return Ok(Some(destination));
    }
    if matches!(conflict, ConflictPolicy::Skip) {
        return Ok(None);
    }
    let stem = source.file_stem().unwrap_or(name);
    let ext = if source.is_dir() {
        None
    } else {
        source.extension()
    };
    for index in 1..=100_000 {
        let mut candidate = if source.is_dir() {
            name.to_os_string()
        } else {
            stem.to_os_string()
        };
        candidate.push(format!(" (copy {index})"));
        if let Some(ext) = ext {
            candidate.push(".");
            candidate.push(ext);
        }
        let candidate = directory.join(candidate);
        if !exists(&candidate) {
            return Ok(Some(candidate));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "Too many conflicting names",
    ))
}
fn copy_tree(
    source: &Path,
    destination: &Path,
    depth: usize,
    context: &mut CopyContext<'_>,
) -> io::Result<()> {
    interrupted(context.cancel)?;
    if depth > 256 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Folder nesting exceeds 256 levels",
        ));
    }
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_symlink() {
        symlink(fs::read_link(source)?, destination)?;
        preserve_link_metadata(source, destination, &metadata, context.state.preserve_owner)?;
    } else if metadata.is_dir() {
        let input = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(source)?;
        if (input.metadata()?.dev(), input.metadata()?.ino()) != (metadata.dev(), metadata.ino()) {
            return Err(io::Error::other("Source changed while copying"));
        }
        fs::create_dir(destination)?;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o700))?;
        for item in fs::read_dir(source)? {
            let item = item?;
            copy_tree(
                &item.path(),
                &destination.join(item.file_name()),
                depth + 1,
                context,
            )?;
        }
        let output = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(destination)?;
        preserve_file_metadata(&input, &output, &metadata, context.state.preserve_owner)?;
        output.sync_all()?;
    } else if metadata.is_file() {

        let mut input = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(source)?;
        let actual = input.metadata()?;
        if actual.dev() != metadata.dev() || actual.ino() != metadata.ino() {
            return Err(io::Error::other("Source changed while copying"));
        }
        let identity = (metadata.dev(), metadata.ino());
        if let Some(first) = context.state.hardlinks.get(&identity) {
            fs::hard_link(first, destination)?;
        } else {
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(destination)?;
            copy_file_bytes(
                &mut input,
                &mut output,
                metadata.len(),
                context.cancel,
                || {
                    (context.progress)(Progress {
                        completed: *context.completed,
                        total: context.total,
                        message: format!(
                            "Copying {}",
                            source.file_name().unwrap_or_default().to_string_lossy()
                        ),
                    });
                },
            )?;
            preserve_file_metadata(&input, &output, &metadata, context.state.preserve_owner)?;
            output.sync_all()?;
            if metadata.nlink() > 1 {
                context
                    .state
                    .hardlinks
                    .insert(identity, destination.to_owned());
            }
        }
        let after = input.metadata()?;
        if (
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec(),
        ) != (
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        ) {
            return Err(io::Error::other(
                "Source changed while copying; original preserved",
            ));
        }
    } else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Sockets, devices and FIFOs cannot be copied",
        ));
    }
    *context.completed += 1;
    (context.progress)(Progress {
        completed: *context.completed,
        total: context.total,
        message: source
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    });
    Ok(())
}
fn count_tree(path: &Path, cancel: &AtomicBool, depth: usize) -> io::Result<u64> {
    interrupted(cancel)?;
    let meta = fs::symlink_metadata(path)?;
    let mut count = 1;
    if meta.is_dir() && depth < 256 {
        for entry in fs::read_dir(path)? {
            count += count_tree(&entry?.path(), cancel, depth + 1)?;
        }
    }
    Ok(count)
}

type Fingerprint = Vec<(PathBuf, u64, u64, u64, i64, i64, i64, i64)>;
fn fingerprint(
    path: &Path,
    cancel: &AtomicBool,
    depth: usize,
    out: &mut Fingerprint,
) -> io::Result<()> {
    interrupted(cancel)?;
    if depth > 256 {
        return Err(io::Error::other("Folder nesting exceeds 256 levels"));
    }
    let meta = fs::symlink_metadata(path)?;
    out.push((
        path.to_owned(),
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    ));
    if meta.is_dir() {
        let mut children = fs::read_dir(path)?
            .map(|e| e.map(|e| e.path()))
            .collect::<io::Result<Vec<_>>>()?;
        children.sort();
        for child in children {
            fingerprint(&child, cancel, depth + 1, out)?;
        }
    }
    Ok(())
}
fn remove(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

const ORPHAN_MIN_AGE: Duration = Duration::from_secs(30 * 60);
const MAX_JOURNAL_BYTES: u64 = 4 * 1024 * 1024;
const STAGING_PREFIXES: [&str; 3] = [".rix-move-", ".rix-copy-", ".rix-extract-"];
static STAGING_SWEPT: OnceLock<()> = OnceLock::new();
#[derive(serde::Serialize, serde::Deserialize)]
struct StagingRecord {
    pid: u32,
    path: String,
    created: u64,
}
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct StagingJournal {
    entries: Vec<StagingRecord>,
}
fn cache_directory() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("XDG_CACHE_HOME") {
        let root = PathBuf::from(root);
        if root.is_absolute() {
            return Some(root);
        }
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache"))
}
fn journal_path(cache: &Path) -> PathBuf {
    cache.join("rix-files/operations.json")
}
fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}
fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if pid == std::process::id() {
        return true;
    }
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn update_journal<T>(cache: &Path, change: impl FnOnce(&mut StagingJournal) -> T) -> io::Result<T> {
    let path = journal_path(cache);
    if let Some(parent) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_JOURNAL_BYTES)
        .read_to_end(&mut bytes)?;
    let mut journal: StagingJournal = serde_json::from_slice(&bytes).unwrap_or_default();
    let result = change(&mut journal);
    let bytes = serde_json::to_vec(&journal)?;
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(result)
}
fn register_staging_at(cache: &Path, path: &Path) {
    let record = StagingRecord {
        pid: std::process::id(),
        path: path.to_string_lossy().into_owned(),
        created: now_seconds(),
    };
    let _ = update_journal(cache, |journal| {
        journal.entries.retain(|entry| entry.path != record.path);
        journal.entries.push(record);
    });
}
fn unregister_staging_at(cache: &Path, path: &Path) {
    let path = path.to_string_lossy();
    let _ = update_journal(cache, |journal| {
        journal
            .entries
            .retain(|entry| entry.path.as_str() != path.as_ref());
    });
}

fn discard_staging(path: &Path) -> bool {
    let expected = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            STAGING_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix))
        });
    if !expected {
        return true;
    }
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return true;
    };
    if !metadata.is_dir() || metadata.is_symlink() {
        return true;
    }
    make_tree_writable(path);
    fs::remove_dir_all(path).is_ok()
}
fn sweep_staging_at(cache: &Path, now: u64) -> io::Result<Vec<PathBuf>> {
    let mut candidates = Vec::new();
    update_journal(cache, |journal| {
        journal.entries.retain(|entry| {
            let path = PathBuf::from(&entry.path);
            let exists = path.symlink_metadata().is_ok();
            if !exists {
                return false;
            }
            let orphan = entry.pid != std::process::id()
                && !process_alive(entry.pid)
                && now.saturating_sub(entry.created) >= ORPHAN_MIN_AGE.as_secs();
            if orphan {
                candidates.push(path);
            }
            true
        });
    })?;
    let mut removed = Vec::new();
    for path in candidates {
        if discard_staging(&path) {
            let name = path.to_string_lossy();
            let _ = update_journal(cache, |journal| {
                journal
                    .entries
                    .retain(|entry| entry.path.as_str() != name.as_ref());
            });
            removed.push(path);
        }
    }
    Ok(removed)
}

pub fn sweep_orphaned_staging() -> io::Result<Vec<PathBuf>> {
    let Some(cache) = cache_directory() else {
        return Ok(Vec::new());
    };
    sweep_staging_at(&cache, now_seconds())
}
fn sweep_staging_once() {
    STAGING_SWEPT.get_or_init(|| {
        let _ = sweep_orphaned_staging();
    });
}

fn make_tree_writable(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if !metadata.is_dir() {
        return;
    }
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(metadata.mode() | 0o700));
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            make_tree_writable(&entry.path());
        }
    }
}
pub(crate) struct StagingDirectory(pub(crate) tempfile::TempDir);
impl StagingDirectory {

    pub(crate) fn new(prefix: &str, parent: &Path) -> io::Result<Self> {
        sweep_staging_once();
        let temporary = tempfile::Builder::new().prefix(prefix).tempdir_in(parent)?;
        if let Some(cache) = cache_directory() {
            register_staging_at(&cache, temporary.path());
        }
        Ok(Self(temporary))
    }
}
impl std::ops::Deref for StagingDirectory {
    type Target = tempfile::TempDir;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl Drop for StagingDirectory {
    fn drop(&mut self) {
        make_tree_writable(self.0.path());

        if fs::remove_dir_all(self.0.path()).is_ok() {
            if let Some(cache) = cache_directory() {
                unregister_staging_at(&cache, self.0.path());
            }
        }
    }
}
fn safe_source(path: &Path) -> io::Result<()> {
    if !path.is_absolute()
        || path.file_name().is_none()
        || path.components().any(|c| matches!(c, Component::ParentDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Operations require an absolute non-root path",
        ));
    }
    Ok(())
}
fn normalized_source(path: &Path) -> io::Result<PathBuf> {
    safe_source(path)?;

    Ok(path.components().collect())
}

pub(crate) fn relocate(
    source: &Path,
    destination: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(Progress),
) -> io::Result<()> {
    let source = normalized_source(source)?;
    safe_source(destination)?;
    interrupted(cancel)?;
    if source == destination {
        return Ok(());
    }
    match no_replace(&source, destination) {
        Ok(()) => return Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {}
        Err(error) => return Err(error),
    }
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("Choose a destination folder"))?;
    let mut before = Vec::new();
    fingerprint(&source, cancel, 0, &mut before)?;
    let total = count_tree(&source, cancel, 0)?;
    let temporary = StagingDirectory::new(".rix-move-", parent)?;
    let staging = temporary.0.path().join("payload");
    let mut completed = 0;
    let mut state = CopyState {
        preserve_owner: true,
        ..Default::default()
    };
    let mut context = CopyContext {
        cancel,
        completed: &mut completed,
        total,
        progress: &mut progress,
        state: &mut state,
    };
    copy_tree(&source, &staging, 0, &mut context)?;
    interrupted(cancel)?;
    let mut after = Vec::new();
    fingerprint(&source, cancel, 0, &mut after)?;
    if before != after {
        return Err(io::Error::other(
            "Source changed during the move; original preserved",
        ));
    }
    no_replace(&staging, destination)?;
    remove(&source).map_err(|error| {
        io::Error::other(format!(
            "Copy completed at {}, but the original could not be removed: {error}",
            destination.display()
        ))
    })
}

pub fn execute(
    mut request: Request,
    cancel: &AtomicBool,
    mut progress: impl FnMut(Progress),
) -> Report {
    let mut report = Report::default();
    let mut sources = Vec::new();
    for path in &request.sources {
        match normalized_source(path) {
            Ok(path) if !sources.contains(&path) => sources.push(path),
            Ok(_) => {}
            Err(e) => {
                report.errors.push(format!("{}: {e}", path.display()));
                return report;
            }
        }
    }
    let nested: Vec<_> = sources
        .iter()
        .filter(|source| {
            sources.iter().any(|other| {
                other != *source
                    && source.starts_with(other)
                    && fs::symlink_metadata(other).is_ok_and(|m| m.is_dir())
            })
        })
        .cloned()
        .collect();
    sources.retain(|source| !nested.contains(source));
    report.skipped.extend(nested);
    request.sources = sources;
    let mut total = 0;
    for source in &request.sources {
        if let Err(error) = safe_source(source) {
            report.errors.push(error.to_string());
            return report;
        }
        let count = if matches!(request.kind, Kind::Copy | Kind::Move) {
            count_tree(source, cancel, 0)
        } else {
            fs::symlink_metadata(source).map(|_| 1)
        };
        match count {
            Ok(count) => total += count,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                report.cancelled = true;
                return report;
            }
            Err(e) => {
                report.errors.push(format!("{}: {e}", source.display()));
                return report;
            }
        }
    }
    let mut completed = 0;
    for source in &request.sources {
        if interrupted(cancel).is_err() {
            report.cancelled = true;
            break;
        }
        let result = (|| -> io::Result<Option<PathBuf>> {
            match request.kind {
                Kind::Trash => {
                    let outcome = crate::history::trash_path(source, cancel);
                    if outcome.undo_unavailable {
                        report.undo.clear();
                        report.undo_unavailable = true;
                    }
                    for action in outcome.undo {
                        crate::history::append_action(&mut report, Ok(action));
                    }
                    if !outcome.completed.is_empty() {
                        report.errors.extend(outcome.errors);
                        completed += 1;
                        Ok(Some(source.clone()))
                    } else if outcome.cancelled {
                        Err(io::ErrorKind::Interrupted.into())
                    } else {
                        Err(io::Error::other(outcome.errors.join("\n")))
                    }
                }
                Kind::Delete => {
                    remove(source)?;
                    completed += 1;
                    Ok(Some(source.clone()))
                }
                Kind::Copy | Kind::Move => {
                    let directory = request.destination.as_ref().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "Choose a destination folder")
                    })?;
                    let directory = fs::canonicalize(directory)?;
                    if !directory.is_dir() {
                        return Err(io::Error::new(
                            io::ErrorKind::NotADirectory,
                            "Destination is not a folder",
                        ));
                    }
                    let meta = fs::symlink_metadata(source)?;
                    if meta.is_dir() && directory.starts_with(fs::canonicalize(source)?) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "Cannot copy or move a folder inside itself",
                        ));
                    }
                    if matches!(request.kind, Kind::Move)
                        && source
                            .parent()
                            .and_then(|p| fs::canonicalize(p).ok())
                            .as_ref()
                            == Some(&directory)
                    {
                        return Ok(None);
                    }
                    let Some(destination) = destination_for(source, &directory, request.conflict)?
                    else {
                        return Ok(None);
                    };
                    let history_before = if matches!(request.kind, Kind::Move) {
                        match crate::history::Snapshot::capture(source, cancel) {
                            Ok(before) => Some(Ok(before)),
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                                return Err(error);
                            }
                            Err(error) => Some(Err(error)),
                        }
                    } else {
                        None
                    };
                    if matches!(request.kind, Kind::Move) {
                        match no_replace(source, &destination) {
                            Ok(()) => {
                                completed += 1;
                                crate::history::append_action(
                                    &mut report,
                                    history_before.unwrap().and_then(|before| {
                                        crate::history::moved_action(source, &destination, before)
                                    }),
                                );
                                return Ok(Some(destination));
                            }
                            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {}
                            Err(e) => return Err(e),
                        }
                    }
                    let mut before = Vec::new();
                    if matches!(request.kind, Kind::Move) {
                        fingerprint(source, cancel, 0, &mut before)?;
                    }
                    let temporary = StagingDirectory::new(".rix-copy-", &directory)?;
                    let staging = temporary.0.path().join("payload");
                    let mut state = CopyState {
                        preserve_owner: matches!(request.kind, Kind::Move),
                        ..Default::default()
                    };
                    let mut context = CopyContext {
                        cancel,
                        completed: &mut completed,
                        total,
                        progress: &mut progress,
                        state: &mut state,
                    };
                    copy_tree(source, &staging, 0, &mut context)?;
                    interrupted(cancel)?;
                    if matches!(request.kind, Kind::Move) {
                        let mut after = Vec::new();
                        fingerprint(source, cancel, 0, &mut after)?;
                        if before != after {
                            return Err(io::Error::other(
                                "Source changed during the move; original preserved",
                            ));
                        }
                    }
                    no_replace(&staging, &destination)?;
                    if matches!(request.kind, Kind::Move) {
                        if let Err(error) = remove(source) {

                            report.completed.push(destination.clone());
                            crate::history::append_action(
                                &mut report,
                                crate::history::created_action(&destination),
                            );
                            return Err(io::Error::other(format!(
                                "Copy completed at {}, but the original could not be removed: {error}",
                                destination.display()
                            )));
                        }
                        crate::history::append_action(
                            &mut report,
                            history_before.unwrap().and_then(|before| {
                                crate::history::moved_action(source, &destination, before)
                            }),
                        );
                    } else {
                        crate::history::append_action(
                            &mut report,
                            crate::history::created_action(&destination),
                        );
                    }
                    Ok(Some(destination))
                }
            }
        })();
        match result {
            Ok(Some(path)) => report.completed.push(path),
            Ok(None) => report.skipped.push(source.clone()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                report.cancelled = true;
                break;
            }
            Err(e) => report.errors.push(format!("{}: {e}", source.display())),
        }
        progress(Progress {
            completed,
            total,
            message: source
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
    fn set_attribute(path: &Path, name: &str, value: &[u8]) {
        let file = File::open(path).unwrap();
        Attributes::File(&file)
            .set(&CString::new(name).unwrap(), value)
            .unwrap();
    }
    fn attribute(path: &Path, name: &str) -> Vec<u8> {
        Attributes::File(&File::open(path).unwrap())
            .get(&CString::new(name).unwrap())
            .unwrap()
    }
    #[test]
    fn cross_filesystem_move_preserves_acl_xattr_owner_and_nanosecond_times() {
        let root = tempfile::tempdir().unwrap();
        let Ok(destination) = tempfile::tempdir_in("/dev/shm") else {
            return;
        };
        if fs::metadata(root.path()).unwrap().dev()
            == fs::metadata(destination.path()).unwrap().dev()
        {
            return;
        }
        let source = root.path().join("private.txt");
        fs::write(&source, "only the named ACL user can read").unwrap();
        set_attribute(&source, "user.rix-test", b"retained metadata");

        let mut acl = 2u32.to_le_bytes().to_vec();
        for (tag, permissions, uid) in [
            (1u16, 6u16, u32::MAX),
            (2, 4, 65534),
            (4, 0, u32::MAX),
            (16, 4, u32::MAX),
            (32, 0, u32::MAX),
        ] {
            acl.extend(tag.to_le_bytes());
            acl.extend(permissions.to_le_bytes());
            acl.extend(uid.to_le_bytes());
        }
        set_attribute(&source, "system.posix_acl_access", &acl);
        let time = std::time::UNIX_EPOCH + std::time::Duration::new(1_600_000_000, 123_456_789);
        File::open(&source)
            .unwrap()
            .set_times(fs::FileTimes::new().set_accessed(time).set_modified(time))
            .unwrap();
        let before = fs::metadata(&source).unwrap();
        let report = execute(
            request(&source, destination.path(), Kind::Move),
            &AtomicBool::new(false),
            |_| {},
        );
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!source.exists());
        let target = destination.path().join("private.txt");
        assert_eq!(attribute(&target, "system.posix_acl_access"), acl);
        assert_eq!(attribute(&target, "user.rix-test"), b"retained metadata");
        let after = fs::metadata(&target).unwrap();
        assert_eq!(
            (after.uid(), after.gid(), after.mode() & 0o7777),
            (before.uid(), before.gid(), before.mode() & 0o7777)
        );
        assert_eq!(
            (
                after.mtime(),
                after.mtime_nsec(),
                after.atime(),
                after.atime_nsec()
            ),
            (
                before.mtime(),
                before.mtime_nsec(),
                before.atime(),
                before.atime_nsec()
            )
        );
    }
    #[test]
    fn failed_cross_filesystem_metadata_restoration_keeps_source_and_cleans_stage() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let Ok(destination) = tempfile::tempdir_in("/dev/shm") else {
            return;
        };
        if fs::metadata(root.path()).unwrap().dev()
            == fs::metadata(destination.path()).unwrap().dev()
        {
            return;
        }
        let source = root.path().join("keep.txt");
        fs::write(&source, "source remains").unwrap();
        set_attribute(&source, "user.rix-test", b"must survive");
        let mut interrupted_metadata = false;
        let report = execute(
            request(&source, destination.path(), Kind::Move),
            &AtomicBool::new(false),
            |_| {
                if !interrupted_metadata {

                    let staged = fs::read_dir(destination.path())
                        .unwrap()
                        .next()
                        .unwrap()
                        .unwrap()
                        .path()
                        .join("payload");
                    fs::set_permissions(staged, fs::Permissions::from_mode(0o000)).unwrap();
                    interrupted_metadata = true;
                }
            },
        );
        assert!(!report.errors.is_empty());
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.contains("Cannot preserve attribute")),
            "{:?}",
            report.errors
        );
        assert!(report.completed.is_empty());
        assert_eq!(fs::read_to_string(&source).unwrap(), "source remains");
        assert_eq!(attribute(&source, "user.rix-test"), b"must survive");
        assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 0);
    }
    #[test]
    fn copy_preserves_read_only_xattrs_hardlinks_and_sparse_extents() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        let path = source.join("sparse");
        let mut file = File::create(&path).unwrap();
        file.write_all(b"begin").unwrap();
        file.seek(SeekFrom::Start(16 * 1024 * 1024)).unwrap();
        file.write_all(b"end").unwrap();
        set_attribute(&path, "user.rix-test", b"readonly");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        fs::hard_link(&path, source.join("linked")).unwrap();
        let report = execute(
            request(&source, &destination, Kind::Copy),
            &AtomicBool::new(false),
            |_| {},
        );
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let copied = destination.join("source/sparse");
        assert_eq!(attribute(&copied, "user.rix-test"), b"readonly");
        let copied_meta = fs::metadata(&copied).unwrap();
        assert_eq!(
            copied_meta.ino(),
            fs::metadata(destination.join("source/linked"))
                .unwrap()
                .ino()
        );
        assert_eq!(copied_meta.mode() & 0o777, 0o400);
        assert_eq!(copied_meta.len(), 16 * 1024 * 1024 + 3);
        let mut copied_file = File::open(&copied).unwrap();
        let mut beginning = [0u8; 5];
        copied_file.read_exact(&mut beginning).unwrap();
        assert_eq!(&beginning, b"begin");
        copied_file.seek(SeekFrom::Start(8 * 1024 * 1024)).unwrap();
        let mut hole = [1u8; 4096];
        copied_file.read_exact(&mut hole).unwrap();
        assert_eq!(hole, [0u8; 4096]);
        copied_file.seek(SeekFrom::Start(16 * 1024 * 1024)).unwrap();
        let mut end = [0u8; 3];
        copied_file.read_exact(&mut end).unwrap();
        assert_eq!(&end, b"end");
        if fs::metadata(&path).unwrap().blocks() < fs::metadata(&path).unwrap().len() / 512 {
            assert!(copied_meta.blocks() * 512 < copied_meta.len() / 2);
        }
    }
    #[test]
    fn copied_tree_does_not_keep_access_granted_by_destination_default_acl() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        let path = source.join("private.txt");
        fs::write(&path, "private").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let mut acl = 2u32.to_le_bytes().to_vec();
        for (tag, permissions, uid) in [
            (1u16, 7u16, u32::MAX),
            (2, 4, 65534),
            (4, 0, u32::MAX),
            (16, 4, u32::MAX),
            (32, 0, u32::MAX),
        ] {
            acl.extend(tag.to_le_bytes());
            acl.extend(permissions.to_le_bytes());
            acl.extend(uid.to_le_bytes());
        }
        set_attribute(&destination, "system.posix_acl_default", &acl);
        let report = execute(
            request(&source, &destination, Kind::Copy),
            &AtomicBool::new(false),
            |_| {},
        );
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let copied = destination.join("source/private.txt");
        assert_eq!(fs::metadata(&copied).unwrap().mode() & 0o777, 0o600);
        let file = File::open(&copied).unwrap();
        assert_eq!(
            Attributes::File(&file)
                .get(&CString::new("system.posix_acl_access").unwrap())
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENODATA)
        );
        let directory = File::open(destination.join("source")).unwrap();
        assert_eq!(
            Attributes::File(&directory)
                .get(&CString::new("system.posix_acl_default").unwrap())
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENODATA)
        );
    }
    #[test]
    fn create_file_is_exclusive_and_rejects_invalid_names() {
        let root = tempfile::tempdir().unwrap();
        let file = create_file(root.path(), "created.txt").unwrap();
        assert!(fs::metadata(&file).unwrap().is_file());
        assert_eq!(fs::metadata(&file).unwrap().len(), 0);
        fs::write(&file, "preserve existing contents").unwrap();
        assert_eq!(
            create_file(root.path(), "created.txt").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            fs::read_to_string(file).unwrap(),
            "preserve existing contents"
        );
        symlink(root.path().join("missing"), root.path().join("link")).unwrap();
        assert_eq!(
            create_file(root.path(), "link").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(!root.path().join("missing").exists());
        for name in [
            "",
            ".",
            "..",
            "../escape",
            "nested/file",
            "/absolute",
            "nul\0byte",
        ] {
            assert_eq!(
                create_file(root.path(), name).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert_eq!(
                create_directory(root.path(), name).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn moving_a_selected_parent_skips_descendants_before_paths_disappear() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("parent");
        let child = parent.join("child");
        let dest = root.path().join("destination");
        fs::create_dir(&parent).unwrap();
        fs::create_dir(&dest).unwrap();
        fs::write(&child, "data").unwrap();
        let mut input = request(&parent, &dest, Kind::Move);
        input.sources.push(child.clone());
        let report = execute(input, &AtomicBool::new(false), |_| {});
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.skipped, vec![child]);
        assert_eq!(
            fs::read_to_string(dest.join("parent/child")).unwrap(),
            "data"
        );
    }
    #[test]
    fn unpublished_read_only_tree_is_removed_without_changing_source_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let dest = root.path().join("dest");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&dest).unwrap();
        fs::write(source.join("data"), "original").unwrap();
        symlink(&source, source.join("link")).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o500)).unwrap();
        let cancel = AtomicBool::new(false);
        let report = execute(request(&source, &dest, Kind::Copy), &cancel, |p| {
            if p.completed == p.total {
                cancel.store(true, Ordering::Relaxed);
            }
        });
        assert!(report.cancelled);
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
        assert_eq!(fs::read_to_string(source.join("data")).unwrap(), "original");
        assert_eq!(fs::metadata(&source).unwrap().mode() & 0o777, 0o500);
        fs::set_permissions(&source, fs::Permissions::from_mode(0o700)).unwrap();
    }
    #[test]
    fn trailing_separators_never_turn_source_links_into_recursive_targets() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let dest = root.path().join("dest");
        fs::create_dir(&target).unwrap();
        fs::create_dir(&dest).unwrap();
        fs::write(target.join("keep"), "untouched").unwrap();
        for (i, suffix) in ["/", "/."].into_iter().enumerate() {
            let link = root.path().join(format!("link-{i}"));
            symlink(&target, &link).unwrap();
            let path = PathBuf::from(format!("{}{suffix}", link.display()));
            let copied = execute(
                request(&path, &dest, Kind::Copy),
                &AtomicBool::new(false),
                |_| {},
            );
            assert!(copied.errors.is_empty(), "{:?}", copied.errors);
            assert!(fs::symlink_metadata(dest.join(format!("link-{i}")))
                .unwrap()
                .is_symlink());
            let deleted = execute(
                request(&path, &dest, Kind::Delete),
                &AtomicBool::new(false),
                |_| {},
            );
            assert!(deleted.errors.is_empty(), "{:?}", deleted.errors);
            assert!(fs::symlink_metadata(&link).is_err());
            assert_eq!(
                fs::read_to_string(target.join("keep")).unwrap(),
                "untouched"
            );
        }
    }
    fn request(source: &Path, dest: &Path, kind: Kind) -> Request {
        Request {
            kind,
            sources: vec![source.into()],
            destination: Some(dest.into()),
            conflict: ConflictPolicy::KeepBoth,
        }
    }
    #[test]
    fn copy_preserves_links_permissions_and_never_overwrites() {
        use std::os::unix::fs::PermissionsExt;
        let r = tempfile::tempdir().unwrap();
        let source = r.path().join("source");
        let dest = r.path().join("dest");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&dest).unwrap();
        fs::write(source.join("data"), "original").unwrap();
        fs::set_permissions(source.join("data"), fs::Permissions::from_mode(0o640)).unwrap();
        symlink("data", source.join("link")).unwrap();
        let report = execute(
            request(&source, &dest, Kind::Copy),
            &AtomicBool::new(false),
            |_| {},
        );
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(
            fs::read_link(dest.join("source/link")).unwrap(),
            Path::new("data")
        );
        assert_eq!(
            fs::metadata(dest.join("source/data"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        fs::write(source.join("data"), "changed").unwrap();
        execute(
            request(&source, &dest, Kind::Copy),
            &AtomicBool::new(false),
            |_| {},
        );
        assert_eq!(
            fs::read_to_string(dest.join("source/data")).unwrap(),
            "original"
        );
        assert_eq!(
            fs::read_to_string(dest.join("source (copy 1)/data")).unwrap(),
            "changed"
        );
    }
    #[test]
    fn cancellation_during_copy_keeps_source_and_cleans_staging() {
        let r = tempfile::tempdir().unwrap();
        let source = r.path().join("large");
        let dest = r.path().join("dest");
        fs::create_dir(&dest).unwrap();
        fs::write(&source, vec![1; 3 * 1024 * 1024]).unwrap();
        let cancel = AtomicBool::new(false);
        let report = execute(request(&source, &dest, Kind::Move), &cancel, |_| {});
        assert!(report.errors.is_empty());
        assert!(!source.exists());
        assert!(dest.join("large").exists());
        let source = dest.join("large");
        let target = r.path().join("target");
        fs::create_dir(&target).unwrap();
        let report = execute(request(&source, &target, Kind::Copy), &cancel, |_| {
            cancel.store(true, Ordering::Relaxed)
        });
        assert!(report.cancelled);
        assert!(source.exists());
        assert_eq!(fs::read_dir(target).unwrap().count(), 0);
    }
    #[test]
    fn rejects_descendants_invalid_names_and_rename_conflicts() {
        let r = tempfile::tempdir().unwrap();
        let source = r.path().join("source");
        fs::create_dir(&source).unwrap();
        let report = execute(
            request(&source, &source, Kind::Copy),
            &AtomicBool::new(false),
            |_| {},
        );
        assert!(!report.errors.is_empty());
        assert!(create_directory(r.path(), "../bad").is_err());
        assert!(create_directory(r.path(), ".").is_err());
        let a = r.path().join("a");
        let b = r.path().join("b");
        fs::write(&a, "a").unwrap();
        fs::write(&b, "b").unwrap();
        assert!(rename(&a, "b").is_err());
        assert_eq!(fs::read_to_string(b).unwrap(), "b");
    }
    #[test]
    fn delete_link_does_not_delete_target() {
        let r = tempfile::tempdir().unwrap();
        let target = r.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("data"), "safe").unwrap();
        let link = r.path().join("link");
        symlink(&target, &link).unwrap();
        let report = execute(
            Request {
                kind: Kind::Delete,
                sources: vec![link],
                destination: None,
                conflict: ConflictPolicy::Skip,
            },
            &AtomicBool::new(false),
            |_| {},
        );
        assert!(report.errors.is_empty());
        assert!(target.join("data").exists());
    }
    #[test]
    fn destination_created_during_copy_is_never_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("data");
        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();
        fs::write(&source, "source").unwrap();
        let mut created = false;
        let report = execute(
            request(&source, &dest, Kind::Copy),
            &AtomicBool::new(false),
            |_| {
                if !created {
                    fs::write(dest.join("data"), "concurrent").unwrap();
                    created = true;
                }
            },
        );
        assert!(!report.errors.is_empty());
        assert_eq!(fs::read_to_string(dest.join("data")).unwrap(), "concurrent");
        assert_eq!(fs::read_to_string(source).unwrap(), "source");
        assert_eq!(fs::read_dir(dest).unwrap().count(), 1);
    }
    #[test]
    fn cross_filesystem_move_preserves_original_on_cancellation_and_changes() {
        let root = tempfile::tempdir().unwrap();
        let Ok(dest) = tempfile::tempdir_in("/dev/shm") else {
            return;
        };
        if fs::metadata(root.path()).unwrap().dev() == fs::metadata(dest.path()).unwrap().dev() {
            return;
        }
        let source = root.path().join("data");
        fs::write(&source, vec![1; 3 * 1024 * 1024]).unwrap();
        let cancel = AtomicBool::new(false);
        let report = execute(request(&source, dest.path(), Kind::Move), &cancel, |_| {
            cancel.store(true, Ordering::Relaxed)
        });
        assert!(report.cancelled);
        assert!(source.exists());
        assert_eq!(fs::read_dir(dest.path()).unwrap().count(), 0);
        let mut changed = false;
        let report = execute(
            request(&source, dest.path(), Kind::Move),
            &AtomicBool::new(false),
            |_| {
                if !changed {
                    fs::write(&source, "new content").unwrap();
                    changed = true;
                }
            },
        );
        assert!(!report.errors.is_empty());
        assert_eq!(fs::read_to_string(&source).unwrap(), "new content");
        assert!(!dest.path().join("data").exists());
        let report = execute(
            request(&source, dest.path(), Kind::Move),
            &AtomicBool::new(false),
            |_| {},
        );
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!source.exists());
        assert_eq!(
            fs::read_to_string(dest.path().join("data")).unwrap(),
            "new content"
        );
    }
    #[test]
    fn trash_restore_uses_an_isolated_process() {
        if let Some(root) = std::env::var_os("RIX_FILES_TRASH_TEST") {
            let path = PathBuf::from(root).join("sample.txt");
            fs::write(&path, "restore me").unwrap();
            let report = execute(
                Request {
                    kind: Kind::Trash,
                    sources: vec![path.clone()],
                    destination: None,
                    conflict: ConflictPolicy::Skip,
                },
                &AtomicBool::new(false),
                |_| {},
            );
            assert!(report.errors.is_empty(), "{:?}", report.errors);
            assert!(!path.exists());
            let items = trash::os_limited::list().unwrap();
            let item = items
                .into_iter()
                .find(|i| i.original_path() == path)
                .unwrap();
            trash::os_limited::restore_all([item]).unwrap();
            assert_eq!(fs::read_to_string(path).unwrap(), "restore me");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "operations::tests::trash_restore_uses_an_isolated_process",
                "--nocapture",
            ])
            .env("RIX_FILES_TRASH_TEST", root.path())
            .env("XDG_DATA_HOME", root.path().join("data"))
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    fn reaped_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }
    fn journal(cache: &Path) -> StagingJournal {
        serde_json::from_slice(&fs::read(journal_path(cache)).unwrap()).unwrap()
    }
    #[test]
    fn staging_journal_tracks_registration_and_removal() {
        let cache = tempfile::tempdir().unwrap();
        let path = cache.path().join(".rix-copy-live");
        fs::create_dir(&path).unwrap();
        register_staging_at(cache.path(), &path);
        let entries = journal(cache.path()).entries;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pid, std::process::id());
        unregister_staging_at(cache.path(), &path);
        assert!(journal(cache.path()).entries.is_empty());
    }
    #[test]
    fn staging_journal_sweeps_only_old_dead_process_directories() {
        let cache = tempfile::tempdir().unwrap();
        let now = now_seconds();
        let dead_pid = reaped_pid();
        let directory = |name: &str| {
            let path = cache.path().join(name);
            fs::create_dir(&path).unwrap();
            path
        };
        let dead = directory(".rix-copy-dead");
        let live = directory(".rix-move-live");
        let young = directory(".rix-extract-young");
        let old = now - ORPHAN_MIN_AGE.as_secs() - 1;
        update_journal(cache.path(), |state| {
            state.entries = vec![
                StagingRecord {
                    pid: dead_pid,
                    path: dead.to_string_lossy().into_owned(),
                    created: old,
                },
                StagingRecord {
                    pid: std::process::id(),
                    path: live.to_string_lossy().into_owned(),
                    created: old,
                },
                StagingRecord {
                    pid: dead_pid,
                    path: young.to_string_lossy().into_owned(),
                    created: now,
                },
            ];
        })
        .unwrap();
        let removed = sweep_staging_at(cache.path(), now).unwrap();
        assert_eq!(removed, vec![dead.clone()]);
        assert!(!dead.exists());
        assert!(live.is_dir());
        assert!(young.is_dir());
        assert_eq!(journal(cache.path()).entries.len(), 2);
    }
}
