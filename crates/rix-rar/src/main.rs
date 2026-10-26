use anyhow::{ensure, Context, Result};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Component, PathBuf},
    time::SystemTime,
};
#[derive(serde::Deserialize)]
struct Request {
    action: String,
    path: Vec<u8>,
    destination: Option<Vec<u8>>,
    member: Option<Vec<u8>>,
    password: String,
}
#[derive(serde::Serialize)]
struct Member {
    path: Vec<u8>,
    directory: bool,
    size: u64,
    link: bool,
    #[serde(skip)]
    mode: Option<u32>,
    #[serde(skip)]
    modified: Option<SystemTime>,
}
fn restore_metadata(file: &File, mode: Option<u32>, modified: Option<SystemTime>) -> Result<()> {
    if let Some(mode) = mode {
        file.set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
    }
    if let Some(modified) = modified {
        file.set_times(fs::FileTimes::new().set_modified(modified))?;
    }
    Ok(())
}
fn safe(bytes: &[u8]) -> Result<PathBuf> {
    ensure!(
        !bytes.is_empty()
            && bytes.len() <= 4096
            && !bytes.contains(&0)
            && !bytes.contains(&b'\\')
            && !bytes.contains(&b':'),
        "Invalid archive path"
    );
    let path = PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()));
    ensure!(
        !path.is_absolute()
            && !path.components().any(|part| matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )),
        "Archive path escapes its folder"
    );
    let path: PathBuf = path
        .components()
        .filter(|part| matches!(part, Component::Normal(_)))
        .collect();
    ensure!(!path.as_os_str().is_empty(), "Empty archive member path");
    Ok(path)
}
struct Bounded<'a> {
    file: File,
    total: &'a mut u64,
    maximum: u64,
    reported: &'a mut u64,
}

const PROGRESS_STEP: u64 = 1024 * 1024;
impl Write for Bounded<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.total.saturating_add(bytes.len() as u64) > self.maximum {
            return Err(std::io::Error::other("RAR unpacked size limit exceeded"));
        }
        let count = self.file.write(bytes)?;
        *self.total += count as u64;
        if *self.total - *self.reported >= PROGRESS_STEP {
            *self.reported = *self.total;
            eprintln!("progress {}", self.total);
        }
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
fn run() -> Result<()> {

    unsafe {
        let memory = libc::rlimit {
            rlim_cur: 2 * 1024 * 1024 * 1024,
            rlim_max: 2 * 1024 * 1024 * 1024,
        };
        libc::setrlimit(libc::RLIMIT_AS, &memory);
        let cpu = libc::rlimit {

            rlim_cur: 3600,
            rlim_max: 3600,
        };
        libc::setrlimit(libc::RLIMIT_CPU, &cpu);
    }
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(256 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 256 * 1024, "Request too large");
    let request: Request = serde_json::from_slice(&bytes)?;
    ensure!(request.password.len() <= 4096, "Password too long");
    let path = PathBuf::from(std::ffi::OsString::from_vec(request.path));
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "RAR source must be a regular file"
    );
    let mut archive = unrar_rs::RarArchive::open_with_password(file, &request.password)?;
    archive.set_password(request.password);
    archive.set_decode_mode(unrar_rs::DecodeMode::Serial);
    let entries: Vec<_> = archive.entries().collect();
    ensure!(
        entries.len() <= 100_000,
        "Archive contains too many entries"
    );
    let mut members = Vec::new();
    let mut warnings = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut declared = 0u64;
    for (index, entry) in entries.into_iter().enumerate() {
        let mode = matches!(
            entry.host_os,
            unrar_rs::HostOs::Unix | unrar_rs::HostOs::Darwin
        )
        .then_some(entry.attributes.unix_mode());
        let raw = entry
            .raw_name_bytes
            .unwrap_or_else(|| entry.raw_name.into_bytes());
        let name = safe(&raw)?;

        let duplicate = !seen.insert(name.clone());
        if duplicate {
            warnings.push(format!("Skipped duplicate path: {}", name.display()));
        } else {
            declared = declared
                .checked_add(entry.unpacked_size.unwrap_or(0))
                .context("RAR size overflow")?;
            ensure!(
                declared <= 32 * 1024 * 1024 * 1024,
                "RAR size exceeds 32 GiB"
            );
        }
        members.push((
            index,
            Member {
                path: name.as_os_str().as_bytes().to_vec(),
                directory: entry.is_directory,
                size: entry.unpacked_size.unwrap_or(0),
                link: entry.is_symlink
                    || entry.is_hardlink
                    || entry.is_file_copy
                    || mode.is_some_and(|mode| !matches!(mode & 0o170000, 0 | 0o100000 | 0o040000)),
                mode: mode.map(|mode| mode & 0o777),
                modified: entry.mtime,
            },
            duplicate,
        ));
    }
    if request.action == "list" {
        let listed: Vec<&Member> = members
            .iter()
            .filter(|(_, _, duplicate)| !duplicate)
            .map(|(_, member, _)| member)
            .collect();
        serde_json::to_writer(
            std::io::stdout(),
            &serde_json::json!({"members": listed, "warnings": warnings}),
        )?;
        return Ok(());
    }
    let destination = PathBuf::from(std::ffi::OsString::from_vec(
        request.destination.context("Missing destination")?,
    ));
    ensure!(
        destination.is_absolute() && destination.is_dir() && !destination.is_symlink(),
        "Invalid extraction staging directory"
    );
    let selected = request
        .member
        .as_ref()
        .map(|bytes| safe(bytes))
        .transpose()?;
    let mut written = 0;
    let mut reported = 0;
    let mut directories = Vec::new();
    for (index, member, duplicate) in &members {
        let name = safe(&member.path)?;
        if selected.as_ref().is_some_and(|selected| selected != &name) {
            continue;
        }

        if *duplicate {
            continue;
        }
        if member.link {
            ensure!(selected.is_none(), "RAR links cannot be extracted");
            warnings.push(format!("Skipped link or special file: {}", name.display()));
            continue;
        }
        let target = if selected.is_some() {
            destination.join(name.file_name().context("Missing filename")?)
        } else {
            destination.join(&name)
        };
        if member.directory {
            fs::create_dir_all(&target)?;
            if selected.is_none() {
                directories.push((target, member));
            }
            continue;
        }
        fs::create_dir_all(target.parent().unwrap())?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(target)?;
        let maximum = if selected.is_some() {
            128 * 1024 * 1024
        } else {
            32 * 1024 * 1024 * 1024
        };
        let mut writer = Bounded {
            file,
            total: &mut written,
            maximum,
            reported: &mut reported,
        };
        archive.by_index(*index)?.copy_to(&mut writer)?;

        if selected.is_none() {
            restore_metadata(&writer.file, member.mode, member.modified)?;
        }
        writer.file.sync_all()?;
    }
    directories.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
    for (path, member) in directories {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)?;
        restore_metadata(&file, member.mode, member.modified)?;
        file.sync_all()?;
    }
    serde_json::to_writer(
        std::io::stdout(),
        &serde_json::json!({"written":written, "skipped":warnings}),
    )?;
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalized_paths_reject_empty_roots_and_traversal() {
        assert_eq!(
            safe(b"./nested/file").unwrap(),
            PathBuf::from("nested/file")
        );
        for path in [
            b".".as_slice(),
            b"./",
            b"../outside",
            b"./../outside",
            b"/absolute",
            b"C:drive",
            b"\\backslash",
        ] {
            assert!(safe(path).is_err(), "accepted {path:?}");
        }
    }
    #[test]
    fn extraction_metadata_keeps_executable_and_mtime_without_special_bits() {
        let root = std::env::temp_dir().join(format!(
            "rix-rar-metadata-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("script.sh");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        let modified = std::time::UNIX_EPOCH + std::time::Duration::new(1_600_000_000, 123_456_789);
        restore_metadata(&file, Some(0o104755), Some(modified)).unwrap();
        let metadata = file.metadata().unwrap();
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o755);
        assert_eq!(metadata.modified().unwrap(), modified);
        fs::remove_dir_all(root).unwrap();
    }
}
