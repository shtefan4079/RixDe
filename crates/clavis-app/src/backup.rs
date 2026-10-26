use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{CString, OsStr},
    fs,
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const MAX_ARCHIVE: u64 = 64 * 1024 * 1024;
const MAX_FILE: u64 = 8 * 1024 * 1024;
const HEADER: &[u8] = b"GnomeKeyring\n\r\0\n\x01\x00";
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Archive {
    format: String,
    version: u32,
    files: Vec<StoredFile>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredFile {
    name: String,
    encrypted: Vec<u8>,
}

pub fn data_dir() -> Result<PathBuf> {
    let root = std::env::var_os("XDG_DATA_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
        .context("No data directory")?;
    Ok(root.join("clavis/keyrings/v1"))
}

struct Directory(fs::File);
impl Directory {
    fn path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd()))
    }
    fn child(&self, name: impl AsRef<OsStr>) -> PathBuf {
        self.path().join(name.as_ref())
    }
}
fn open_directory(path: &Path, create: bool) -> Result<Directory> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let options = || {
        let mut options = fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
        options
    };
    let mut directory = Directory(options().open("/")?);
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            _ => anyhow::bail!("Parent traversal is not allowed in protected data paths"),
        };
        let child = directory.child(name);
        match options().open(&child) {
            Ok(file) => directory = Directory(file),
            Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                match fs::DirBuilder::new().mode(0o700).create(&child) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
                directory = Directory(options().open(&child)?);
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(directory)
}

pub fn prepare_directory(path: &Path) -> Result<()> {
    let directory = open_directory(path, true)?;
    ensure!(
        directory.0.metadata()?.uid() == unsafe { libc::getuid() },
        "Backup directory belongs to another user"
    );
    directory
        .0
        .set_permissions(fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn checked_read_in(directory: &Directory, name: &OsStr, max: u64) -> Result<Vec<u8>> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory.child(name))?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.len() <= max,
        "Invalid or excessive backup file"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(max + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= max, "Backup exceeds size limit");
    Ok(bytes)
}
fn checked_read(path: &Path, max: u64) -> Result<Vec<u8>> {
    let directory = open_directory(path.parent().context("No backup parent")?, false)?;
    checked_read_in(
        &directory,
        path.file_name().context("No backup file name")?,
        max,
    )
}
fn state_lock(destination: &Path) -> Result<fs::File> {
    let root = destination
        .ancestors()
        .nth(2)
        .context("Invalid Clavis data directory")?;
    let directory = open_directory(root, true)?;
    ensure!(
        directory.0.metadata()?.uid() == unsafe { libc::getuid() },
        "Clavis data directory belongs to another user"
    );
    directory
        .0
        .set_permissions(fs::Permissions::from_mode(0o700))?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory.child(".state.lock"))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.uid() == unsafe { libc::getuid() },
        "Unsafe Clavis state lock"
    );
    file.set_permissions(fs::Permissions::from_mode(0o600))?;

    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        anyhow::bail!("Stop every Clavis daemon before restoring a backup");
    }
    Ok(file)
}
fn rename_new(directory: &Directory, source: &OsStr, target: &OsStr) -> Result<()> {
    let source = CString::new(source.as_bytes())?;
    let target = CString::new(target.as_bytes())?;

    let result = unsafe {
        libc::renameat2(
            directory.0.as_raw_fd(),
            source.as_ptr(),
            directory.0.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
fn valid_file(file: &StoredFile) -> bool {
    let mut parts = Path::new(&file.name).components();
    matches!(parts.next(), Some(Component::Normal(_)))
        && parts.next().is_none()
        && file.name.len() <= 240
        && !file.name.contains('\0')
        && file.encrypted.len() as u64 <= MAX_FILE
        && if file.name.ends_with(".keyring") {
            file.encrypted.starts_with(HEADER)
        } else if file.name.ends_with(".metadata.json") {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Metadata {
                label: String,
                alias: String,
            }
            file.encrypted.len() <= 65536
                && serde_json::from_slice::<Metadata>(&file.encrypted).is_ok_and(|m| {
                    !m.label.is_empty()
                        && m.label.len() <= 256
                        && m.alias.len() <= 256
                        && !m.label.contains('\0')
                        && !m.alias.contains('\0')
                })
        } else {
            false
        }
}
pub fn save(destination: &Path) -> Result<()> {
    save_from(&data_dir()?, destination)
}
fn save_from(source: &Path, destination: &Path) -> Result<()> {
    let source = open_directory(source, false).context("Create a vault before backing it up")?;
    let mut files = Vec::new();
    let mut total = 0usize;
    for entry in fs::read_dir(source.path())? {
        let entry = entry?;
        let name = entry.file_name();
        if Path::new(&name).extension().is_some_and(|e| e == "keyring")
            || name.to_string_lossy().ends_with(".metadata.json")
        {
            let file = StoredFile {
                name: name
                    .clone()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("Invalid vault file name"))?,
                encrypted: checked_read_in(&source, &name, MAX_FILE)?,
            };
            ensure!(
                valid_file(&file),
                "Only encrypted Clavis v1 keyrings can be backed up"
            );
            total += file.encrypted.len();
            ensure!(
                total <= MAX_ARCHIVE as usize / 5,
                "Backup exceeds the encrypted inventory limit"
            );
            files.push(file);
        }
        ensure!(files.len() <= 256, "Too many vaults");
    }
    ensure!(
        files.iter().any(|f| f.name.ends_with(".keyring")),
        "There are no persistent vaults to back up"
    );
    files.sort_by(|a, b| a.name.cmp(&b.name));
    let archive = Archive {
        format: "clavis-encrypted-backup".into(),
        version: 1,
        files,
    };
    let bytes = serde_json::to_vec(&archive)?;
    ensure!(
        bytes.len() as u64 <= MAX_ARCHIVE,
        "Backup exceeds size limit"
    );
    let directory = open_directory(destination.parent().context("No backup parent")?, false)?;
    let name = destination.file_name().context("No backup file name")?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory.child(name))?;
    if let Err(err) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(directory.child(name));
        return Err(err.into());
    }
    directory.0.sync_all()?;
    Ok(())
}
pub fn restore(source: &Path) -> Result<()> {
    restore_into(source, &data_dir()?)
}
fn restore_into(source: &Path, destination: &Path) -> Result<()> {
    let archive: Archive = serde_json::from_slice(&checked_read(source, MAX_ARCHIVE)?)?;
    ensure!(
        archive.format == "clavis-encrypted-backup" && archive.version == 1,
        "Unsupported backup format"
    );
    ensure!(
        !archive.files.is_empty() && archive.files.len() <= 256,
        "Invalid backup inventory"
    );
    let mut seen = std::collections::BTreeSet::new();
    for file in &archive.files {
        ensure!(
            valid_file(file) && seen.insert(&file.name),
            "Unsafe or duplicate backup entry"
        );
    }
    for file in &archive.files {
        if let Some(stem) = file.name.strip_suffix(".metadata.json") {
            ensure!(
                archive
                    .files
                    .iter()
                    .any(|f| f.name == format!("{stem}.keyring")),
                "Orphan vault metadata"
            );
        }
    }

    let _state_lock = state_lock(destination)?;
    let parent = open_directory(destination.parent().context("No restore parent")?, true)?;
    let name = destination
        .file_name()
        .context("No restore directory name")?;
    let target = parent.child(name);
    match fs::symlink_metadata(&target) {
        Ok(metadata) => ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && fs::read_dir(&target)?.next().is_none(),
            "Restore requires a real empty data directory"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let temporary_name = format!(
        ".clavis-restore-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let temporary = parent.child(&temporary_name);
    fs::DirBuilder::new().mode(0o700).create(&temporary)?;
    let result = (|| -> Result<()> {
        let temporary_directory = Directory(
            fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temporary)?,
        );
        for entry in &archive.files {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(temporary_directory.child(&entry.name))?;
            file.write_all(&entry.encrypted)?;
            file.sync_all()?;
        }
        temporary_directory.0.sync_all()?;
        if fs::symlink_metadata(&target).is_ok() {
            fs::remove_dir(&target)?;
        }
        rename_new(&parent, OsStr::new(&temporary_name), name)?;
        parent.0.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encrypted_roundtrip_and_private_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input");
        fs::create_dir(&input).unwrap();
        fs::write(
            input.join("login.keyring"),
            [HEADER, b"encrypted-fixture"].concat(),
        )
        .unwrap();
        fs::write(
            input.join("login.metadata.json"),
            br#"{"label":"Personal","alias":"default"}"#,
        )
        .unwrap();
        let archive = temp.path().join("backup.clavis");
        save_from(&input, &archive).unwrap();
        assert_eq!(
            fs::metadata(&archive).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let output = temp.path().join("restored/v1");
        restore_into(&archive, &output).unwrap();
        assert_eq!(
            fs::read(output.join("login.keyring")).unwrap(),
            fs::read(input.join("login.keyring")).unwrap()
        );
        assert_eq!(
            fs::read(output.join("login.metadata.json")).unwrap(),
            fs::read(input.join("login.metadata.json")).unwrap()
        );
        assert_eq!(
            fs::metadata(output.join("login.keyring"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(restore_into(&archive, &output).is_err());
    }
    #[test]
    fn traversal_plaintext_duplicate_and_symlink_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("bad");
        let entry = |name: &str| StoredFile {
            name: name.into(),
            encrypted: HEADER.to_vec(),
        };
        for files in [
            vec![entry("../escape.keyring")],
            vec![entry("same.keyring"), entry("same.keyring")],
            vec![StoredFile {
                name: "a.keyring".into(),
                encrypted: b"plaintext".to_vec(),
            }],
        ] {
            fs::write(
                &archive,
                serde_json::to_vec(&Archive {
                    format: "clavis-encrypted-backup".into(),
                    version: 1,
                    files,
                })
                .unwrap(),
            )
            .unwrap();
            assert!(restore_into(&archive, &temp.path().join("output")).is_err());
            assert!(!temp.path().join("output").exists());
        }
        let symlink = temp.path().join("link");
        std::os::unix::fs::symlink(&archive, &symlink).unwrap();
        assert!(checked_read(&symlink, MAX_ARCHIVE).is_err());
    }
    #[test]
    fn restore_rejects_symlink_ancestors_targets_and_active_daemon_lock() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input");
        fs::create_dir(&input).unwrap();
        fs::write(
            input.join("login.keyring"),
            [HEADER, b"encrypted-fixture"].concat(),
        )
        .unwrap();
        let archive = temp.path().join("backup.clavis");
        save_from(&input, &archive).unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let linked = temp.path().join("linked");
        std::os::unix::fs::symlink(&outside, &linked).unwrap();
        assert!(restore_into(&archive, &linked.join("keyrings/v1")).is_err());
        assert!(save_from(&input, &linked.join("backup.clavis")).is_err());
        assert!(prepare_directory(&linked.join("backups")).is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        let output = temp.path().join("clavis/keyrings/v1");
        let held = state_lock(&output).unwrap();
        assert!(restore_into(&archive, &output).is_err());
        assert!(!output.exists());
        drop(held);
        let parent = output.parent().unwrap();
        fs::create_dir(parent).unwrap();
        std::os::unix::fs::symlink(temp.path().join("missing"), &output).unwrap();
        assert!(restore_into(&archive, &output).is_err());
        fs::remove_file(&output).unwrap();
        restore_into(&archive, &output).unwrap();
        assert_eq!(fs::read_dir(&output).unwrap().count(), 1);
    }
}
