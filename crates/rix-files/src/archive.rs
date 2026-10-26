use anyhow::{bail, ensure, Context, Result};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
const MAX_ENTRIES: usize = 100_000;
const MAX_BYTES: u64 = 32 * 1024 * 1024 * 1024;
const MAX_XZ_MEMORY_KIB: u32 = 128 * 1024;
#[derive(Clone, Debug)]
pub struct Member {
    pub path: PathBuf,
    pub directory: bool,
    pub size: u64,
    pub link: bool,
    pub mode: Option<u32>,
    pub modified: Option<SystemTime>,
}
#[derive(Clone, Debug)]
pub struct Archive {
    pub path: PathBuf,
    pub format: String,
    pub members: Vec<Member>,
    pub total_size: u64,
}
#[derive(Clone, Debug)]
pub struct Child {
    pub path: PathBuf,
    pub name: String,
    pub directory: bool,
    pub size: u64,
}
#[derive(Clone, Copy, Debug)]
enum Format {
    Rar,
    Zip,
    Seven,
    Tar,
    TarGz,
    TarXz,
    TarBz,
    TarZst,
    Gz,
    Xz,
    Bz,
    Zst,
}
fn format(path: &Path) -> Result<Format> {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    for (suffix, format) in [
        (".tar.gz", Format::TarGz),
        (".tgz", Format::TarGz),
        (".tar.xz", Format::TarXz),
        (".txz", Format::TarXz),
        (".tar.bz2", Format::TarBz),
        (".tbz2", Format::TarBz),
        (".tar.zst", Format::TarZst),
        (".tzst", Format::TarZst),
        (".rar", Format::Rar),
        (".cbr", Format::Rar),
        (".zip", Format::Zip),
        (".jar", Format::Zip),
        (".cbz", Format::Zip),
        (".7z", Format::Seven),
        (".tar", Format::Tar),
        (".gz", Format::Gz),
        (".xz", Format::Xz),
        (".bz2", Format::Bz),
        (".zst", Format::Zst),
    ] {
        if name.ends_with(suffix) {
            return Ok(format);
        }
    }
    bail!("This archive format is not supported yet")
}
pub fn supported(path: &Path) -> bool {
    format(path).is_ok()
}
fn regular(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "An archive must be a regular file"
    );
    Ok(file)
}
fn validated_path(path: &Path) -> Result<PathBuf> {
    let bytes = path.as_os_str().as_bytes();
    ensure!(
        !bytes.is_empty()
            && bytes.len() <= 4096
            && !bytes.contains(&0)
            && !bytes.contains(&b'\\')
            && !bytes.contains(&b':'),
        "Invalid archive member path"
    );
    ensure!(
        !path.is_absolute()
            && !path.components().any(|part| matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )),
        "Archive contains a path outside its folder"
    );
    let path: PathBuf = path
        .components()
        .filter(|part| matches!(part, Component::Normal(_)))
        .collect();
    Ok(path)
}
fn safe_path(path: &Path) -> Result<PathBuf> {
    let path = validated_path(path)?;
    ensure!(!path.as_os_str().is_empty(), "Empty archive member path");
    Ok(path)
}
fn zip_modified(time: zip::DateTime) -> Option<SystemTime> {
    use chrono::TimeZone;
    let local = chrono::NaiveDate::from_ymd_opt(
        time.year().into(),
        time.month().into(),
        time.day().into(),
    )?
    .and_hms_opt(
        time.hour().into(),
        time.minute().into(),
        time.second().into(),
    )?;

    chrono::Local
        .from_local_datetime(&local)
        .earliest()
        .map(Into::into)
}
fn seven_member(entry: &sevenz_rust2::ArchiveEntry) -> Result<Member> {
    let unix_mode = (entry.has_windows_attributes && entry.windows_attributes & 0x8000 != 0)
        .then_some(entry.windows_attributes >> 16);
    Ok(Member {
        path: safe_path(Path::new(&entry.name))?,
        directory: entry.is_directory,
        size: entry.size,
        link: entry.has_windows_attributes && entry.windows_attributes & 0x400 != 0
            || unix_mode.is_some_and(|mode| !matches!(mode & 0o170000, 0 | 0o100000 | 0o040000)),
        mode: unix_mode.map(|mode| mode & 0o777),
        modified: entry
            .has_last_modified_date
            .then(|| entry.last_modified_date.into()),
    })
}
fn restore_metadata(file: &File, member: &Member) -> Result<()> {
    if let Some(mode) = member.mode {

        file.set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
    }
    if let Some(modified) = member.modified {
        file.set_times(fs::FileTimes::new().set_modified(modified))?;
    }
    Ok(())
}
fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Relaxed),
        "Archive operation cancelled"
    );
    Ok(())
}
fn decoder(file: File, kind: Format) -> Result<Box<dyn Read>> {
    Ok(match kind {
        Format::TarGz | Format::Gz => Box::new(flate2::read::MultiGzDecoder::new(file)),
        Format::TarXz | Format::Xz => Box::new(lzma_rust2::XzReader::new_mem_limit(
            file,
            true,
            MAX_XZ_MEMORY_KIB,
        )),
        Format::TarBz | Format::Bz => Box::new(bzip2::read::MultiBzDecoder::new(file)),
        Format::TarZst | Format::Zst => Box::new(zstd::stream::read::Decoder::new(file)?),
        _ => Box::new(file),
    })
}

struct Bounded<'a, R> {
    inner: R,
    cancel: &'a AtomicBool,
    read: u64,
}
impl<R: Read> Read for Bounded<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("Archive operation cancelled"));
        }
        if self.read >= MAX_BYTES {
            return Err(std::io::Error::other("Archive decoded size exceeds 32 GiB"));
        }
        let maximum = bytes.len().min((MAX_BYTES - self.read) as usize);
        let count = self.inner.read(&mut bytes[..maximum])?;
        self.read += count as u64;
        Ok(count)
    }
}
fn visit(
    path: &Path,
    password: &str,
    cancel: &AtomicBool,
    data: bool,
    mut each: impl FnMut(Member, &mut dyn Read) -> Result<()>,
) -> Result<()> {
    let kind = format(path)?;
    let file = regular(path)?;
    match kind {
        Format::Rar => bail!("RAR streams are handled by the isolated decoder"),
        Format::Zip => {
            let mut zip = zip::ZipArchive::new(file)?;
            ensure!(zip.len() <= MAX_ENTRIES, "Archive contains too many files");
            for index in 0..zip.len() {
                check_cancel(cancel)?;
                let mut entry = if data {
                    if password.is_empty() {
                        zip.by_index(index)?
                    } else {
                        zip.by_index_decrypt(index, password.as_bytes())?
                    }
                } else {
                    zip.by_index_raw(index)?
                };
                let member = Member {
                    path: safe_path(Path::new(entry.name()))?,
                    directory: entry.is_dir(),
                    size: entry.size(),
                    link: entry
                        .unix_mode()
                        .is_some_and(|mode| !matches!(mode & 0o170000, 0 | 0o100000 | 0o040000)),
                    mode: entry.unix_mode().map(|mode| mode & 0o777),
                    modified: entry.last_modified().and_then(zip_modified),
                };
                each(member, &mut entry)?;
            }
        }
        Format::Seven => {
            let mut seven = sevenz_rust2::ArchiveReader::new(file, password.into())?;
            seven.set_thread_count(1);
            ensure!(
                seven.archive().files.len() <= MAX_ENTRIES,
                "Archive contains too many files"
            );
            if data {
                seven.for_each_entries(|entry, reader| {
                    let result = (|| {
                        check_cancel(cancel)?;
                        each(seven_member(entry)?, reader)
                    })();
                    result.map(|()| true).map_err(|error: anyhow::Error| {
                        sevenz_rust2::Error::Other(error.to_string().into())
                    })
                })?;
            } else {
                for entry in &seven.archive().files {
                    check_cancel(cancel)?;
                    each(seven_member(entry)?, &mut std::io::empty())?;
                }
            }
        }
        Format::Tar | Format::TarGz | Format::TarXz | Format::TarBz | Format::TarZst => {
            let reader = Bounded {
                inner: decoder(file, kind)?,
                cancel,
                read: 0,
            };
            let mut tar = tar::Archive::new(reader);
            for (index, entry) in tar.entries()?.enumerate() {
                check_cancel(cancel)?;
                ensure!(index < MAX_ENTRIES, "Archive contains too many files");
                let mut entry = entry?;
                let typ = entry.header().entry_type();
                let name = validated_path(&entry.path()?)?;
                if name.as_os_str().is_empty() {
                    ensure!(typ.is_dir(), "Empty archive member path");

                    continue;
                }
                let member = Member {
                    path: name,
                    directory: typ.is_dir(),
                    size: entry.size(),
                    link: !(typ.is_file() || typ.is_dir()),
                    mode: Some(entry.header().mode()? & 0o777),
                    modified: UNIX_EPOCH.checked_add(Duration::from_secs(entry.header().mtime()?)),
                };
                each(member, &mut entry)?;
            }
        }
        _ => {
            let name = path.file_stem().context("Archive filename is missing")?;
            let mut reader = Bounded {
                inner: decoder(file, kind)?,
                cancel,
                read: 0,
            };
            each(
                Member {
                    path: safe_path(Path::new(name))?,
                    directory: false,
                    size: 0,
                    link: false,
                    mode: None,
                    modified: None,
                },
                &mut reader,
            )?;
        }
    }
    Ok(())
}
pub fn inspect(path: &Path, password: &str, cancel: &AtomicBool) -> Result<Archive> {
    inspect_with_warnings(path, password, cancel).map(|(archive, _)| archive)
}

pub fn inspect_with_warnings(
    path: &Path,
    password: &str,
    cancel: &AtomicBool,
) -> Result<(Archive, Vec<String>)> {
    if matches!(format(path)?, Format::Rar) {
        let listing: RarListing =
            serde_json::from_slice(&rar_command(path, "list", None, None, password, cancel)?)?;
        let mut warnings = listing.warnings;
        let mut members = Vec::new();
        let mut seen = HashSet::new();
        for member in listing.members {
            let member = Member {
                path: safe_path(Path::new(std::ffi::OsStr::from_bytes(&member.path)))?,
                directory: member.directory,
                size: member.size,
                link: member.link,
                mode: None,
                modified: None,
            };
            if !seen.insert(member.path.clone()) {
                warnings.push(duplicate_warning(&member.path));
                continue;
            }
            members.push(member);
        }
        return Ok((
            Archive {
                path: path.to_owned(),
                format: "RAR".into(),
                total_size: members.iter().map(|member| member.size).sum(),
                members,
            },
            warnings,
        ));
    }
    let mut members = Vec::new();
    let mut warnings = Vec::new();
    let mut total = 0u64;
    let mut seen = HashSet::new();
    visit(path, password, cancel, false, |member, _| {
        if !seen.insert(member.path.clone()) {
            warnings.push(duplicate_warning(&member.path));
            return Ok(());
        }
        total = total
            .checked_add(member.size)
            .context("Archive size overflow")?;
        ensure!(total <= MAX_BYTES, "Archive unpacked size exceeds 32 GiB");
        members.push(member);
        Ok(())
    })?;
    Ok((
        Archive {
            path: path.to_owned(),
            format: match format(path)? {
                Format::Zip => "ZIP",
                Format::Seven => "7z",
                Format::Tar => "TAR",
                Format::TarGz => "TAR · gzip",
                Format::TarBz => "TAR · bzip2",
                Format::TarXz => "TAR · XZ",
                Format::TarZst => "TAR · Zstandard",
                Format::Gz => "gzip",
                Format::Xz => "XZ",
                Format::Bz => "bzip2",
                Format::Zst => "Zstandard",
                Format::Rar => "RAR",
            }
            .into(),
            members,
            total_size: total,
        },
        warnings,
    ))
}
fn duplicate_warning(path: &Path) -> String {
    format!("Skipped duplicate path: {}", path.display())
}
impl Archive {
    pub fn children(&self, prefix: &Path, query: &str) -> Vec<Child> {
        let mut items = BTreeMap::<PathBuf, Child>::new();
        let query = query.to_lowercase();
        for member in &self.members {
            let Ok(relative) = member.path.strip_prefix(prefix) else {
                continue;
            };
            let mut components = relative.components();
            let Some(Component::Normal(name)) = components.next() else {
                continue;
            };
            let directory = components.next().is_some() || member.directory;
            let path = prefix.join(name);
            let name = name.to_string_lossy().into_owned();
            if !name.to_lowercase().contains(&query) {
                continue;
            }
            items
                .entry(path.clone())
                .and_modify(|entry| entry.directory |= directory)
                .or_insert(Child {
                    path,
                    name,
                    directory,
                    size: if directory { 0 } else { member.size },
                });
        }
        let mut items: Vec<_> = items.into_values().collect();
        items.sort_by_key(|entry| (!entry.directory, entry.name.to_lowercase()));
        items
    }
}
fn extract_into(
    path: &Path,
    password: &str,
    destination: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, &Path),
) -> Result<(u64, Vec<String>)> {
    if matches!(format(path)?, Format::Rar) {
        let output: serde_json::Value = serde_json::from_slice(&rar_command(
            path,
            "extract",
            Some(destination),
            None,
            password,
            cancel,
        )?)?;
        let written = output["written"].as_u64().unwrap_or(0);
        let skipped = serde_json::from_value(output["skipped"].clone()).unwrap_or_default();
        return Ok((written, skipped));
    }
    let mut written = 0u64;
    let mut count = 0usize;
    let mut skipped = Vec::new();
    let mut extracted: HashMap<PathBuf, bool> = HashMap::new();
    let mut directories = Vec::new();
    visit(path, password, cancel, true, |member, reader| {
        check_cancel(cancel)?;
        count += 1;
        ensure!(count <= MAX_ENTRIES, "Archive contains too many files");

        if member.link {
            skipped.push(format!(
                "Skipped link or special file: {}",
                member.path.display()
            ));
            return Ok(());
        }
        if extracted.contains_key(&member.path) {
            skipped.push(duplicate_warning(&member.path));
            return Ok(());
        }

        if member.path.ancestors().skip(1).any(|ancestor| {
            !ancestor.as_os_str().is_empty() && extracted.get(ancestor) == Some(&false)
        }) {
            skipped.push(format!(
                "Skipped path inside an extracted file: {}",
                member.path.display()
            ));
            return Ok(());
        }
        extracted.insert(member.path.clone(), member.directory);
        let target = destination.join(&member.path);
        if member.directory {
            fs::create_dir_all(&target)?;
            directories.push((target, member));
        } else {
            fs::create_dir_all(target.parent().unwrap())?;
            let mut out = match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&target)
            {
                Ok(out) => out,

                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists
                        || error.kind() == std::io::ErrorKind::IsADirectory =>
                {
                    extracted.insert(member.path.clone(), true);
                    skipped.push(format!(
                        "Skipped path conflicting with an extracted directory: {}",
                        member.path.display()
                    ));
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            };
            let mut buffer = [0u8; 65536];
            loop {
                check_cancel(cancel)?;
                let count = reader.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                written = written
                    .checked_add(count as u64)
                    .context("Archive size overflow")?;
                ensure!(written <= MAX_BYTES, "Archive decoded size exceeds 32 GiB");
                out.write_all(&buffer[..count])?;
                progress(written, &member.path);
            }
            restore_metadata(&out, &member)?;
            out.sync_all()?;
        }
        Ok(())
    })?;

    directories.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
    for (path, member) in directories {
        check_cancel(cancel)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)?;
        restore_metadata(&file, &member)?;
        file.sync_all()?;
    }
    Ok((written, skipped))
}

pub fn extract(
    path: &Path,
    password: &str,
    parent: &Path,
    cancel: &AtomicBool,
    progress: impl FnMut(u64, &Path),
) -> Result<PathBuf> {
    extract_with_warnings(path, password, parent, cancel, progress).map(|(published, _)| published)
}

pub fn extract_with_warnings(
    path: &Path,
    password: &str,
    parent: &Path,
    cancel: &AtomicBool,
    progress: impl FnMut(u64, &Path),
) -> Result<(PathBuf, Vec<String>)> {
    ensure!(parent.is_dir(), "Extraction folder is unavailable");
    let staged = crate::operations::StagingDirectory::new(".rix-extract-", parent)?;
    fs::set_permissions(staged.path(), fs::Permissions::from_mode(0o700))?;
    let (_, skipped) = extract_into(path, password, staged.path(), cancel, progress)?;
    check_cancel(cancel)?;
    let mut name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    for suffix in [
        ".rar", ".cbr", ".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".zip", ".7z", ".tar",
        ".tgz", ".txz", ".tbz2", ".gz", ".xz", ".bz2", ".zst",
    ] {
        if name.to_lowercase().ends_with(suffix) {
            name.truncate(name.len() - suffix.len());
            break;
        }
    }
    if name.is_empty() {
        name = "Archive".into();
    }
    let source = std::ffi::CString::new(staged.path().as_os_str().as_bytes())?;
    for index in 0..10_000 {
        let target = parent.join(if index == 0 {
            name.clone()
        } else {
            format!("{name} ({index})")
        });
        let target_c = std::ffi::CString::new(target.as_os_str().as_bytes())?;

        if unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                target_c.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        } == 0
        {
            return Ok((target, skipped));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error.into());
        }
    }
    bail!("Too many folders with the same extraction name")
}
pub fn read_member(
    archive: &Archive,
    name: &Path,
    password: &str,
    cancel: &AtomicBool,
) -> Result<(tempfile::TempDir, PathBuf)> {
    let member = archive
        .members
        .iter()
        .find(|entry| entry.path == name && !entry.directory && !entry.link)
        .context("Archive member is unavailable")?;
    ensure!(
        member.size <= 128 * 1024 * 1024,
        "Archive member is too large to preview"
    );
    let temporary = tempfile::Builder::new()
        .prefix("rix-archive-preview-")
        .tempdir()?;
    let path = temporary
        .path()
        .join(name.file_name().context("Member has no name")?);
    if matches!(format(&archive.path)?, Format::Rar) {
        rar_command(
            &archive.path,
            "member",
            Some(temporary.path()),
            Some(name),
            password,
            cancel,
        )?;
        ensure!(path.is_file(), "RAR member is unavailable");
        return Ok((temporary, path));
    }
    let mut found = false;
    visit(&archive.path, password, cancel, true, |member, reader| {

        if member.path == name && !member.directory && !member.link && !found {
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            let mut bounded = reader.take(128 * 1024 * 1024 + 1);
            let size = std::io::copy(&mut bounded, &mut output)?;
            ensure!(
                size <= 128 * 1024 * 1024,
                "Archive member is too large to preview"
            );
            found = true;
        }
        Ok(())
    })?;
    ensure!(found, "Archive member disappeared");
    Ok((temporary, path))
}
#[cfg(test)]
mod tests {
    use super::*;
    use zip::write::SimpleFileOptions;
    fn tar_entry(
        builder: &mut tar::Builder<Vec<u8>>,
        name: &str,
        directory: bool,
        mode: u32,
        modified: u64,
        bytes: &[u8],
    ) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(if directory {
            tar::EntryType::Directory
        } else {
            tar::EntryType::Regular
        });
        header.set_mode(mode);
        header.set_mtime(modified);
        header.set_size(bytes.len() as u64);

        header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
        header.set_cksum();
        builder.append(&header, bytes).unwrap();
    }
    #[test]
    fn tar_root_markers_do_not_hide_files_or_change_private_staging_permissions() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("root.tar");
        let mut tar = tar::Builder::new(Vec::new());
        tar_entry(&mut tar, "./", true, 0o000, 1, b"");
        tar_entry(&mut tar, "./nested", true, 0o550, 1_600_000_000, b"");
        tar_entry(
            &mut tar,
            "./nested/script.sh",
            false,
            0o4755,
            1_500_000_000,
            b"#!/bin/sh\n",
        );
        fs::write(&path, tar.into_inner().unwrap()).unwrap();
        let cancel = AtomicBool::new(false);
        let archive = inspect(&path, "", &cancel).unwrap();
        assert_eq!(archive.members.len(), 2);
        let output = extract(&path, "", root.path(), &cancel, |_, _| {}).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(fs::metadata(&output).unwrap().mode() & 0o7777, 0o700);
        let directory = fs::metadata(output.join("nested")).unwrap();
        let script = fs::metadata(output.join("nested/script.sh")).unwrap();
        assert_eq!(
            (directory.mode() & 0o7777, directory.mtime()),
            (0o550, 1_600_000_000)
        );
        assert_eq!(
            (script.mode() & 0o7777, script.mtime()),
            (0o755, 1_500_000_000)
        );
        assert_eq!(
            fs::read(output.join("nested/script.sh")).unwrap(),
            b"#!/bin/sh\n"
        );
        let (_temporary, preview) =
            read_member(&archive, Path::new("nested/script.sh"), "", &cancel).unwrap();
        assert_eq!(fs::metadata(preview).unwrap().mode() & 0o7777, 0o600);
        fs::set_permissions(output.join("nested"), fs::Permissions::from_mode(0o700)).unwrap();
    }
    #[test]
    fn tar_empty_regular_paths_and_root_traversal_are_rejected_before_publication() {
        let root = tempfile::tempdir().unwrap();
        for (index, name, directory) in [
            (0, "./", false),
            (1, "./../outside", true),
            (2, "/outside", true),
        ] {
            let path = root.path().join(format!("invalid-{index}.tar"));
            let mut tar = tar::Builder::new(Vec::new());
            tar_entry(&mut tar, name, directory, 0o755, 1, b"");
            fs::write(&path, tar.into_inner().unwrap()).unwrap();
            assert!(inspect(&path, "", &AtomicBool::new(false)).is_err());
            assert!(extract(&path, "", root.path(), &AtomicBool::new(false), |_, _| {}).is_err());
            assert!(!root.path().join(format!("invalid-{index}")).exists());
        }
        assert!(!root.path().join("outside").exists());
    }
    #[test]
    fn zip_extraction_restores_executable_mode_and_local_dos_timestamp() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("executable.zip");
        let modified = zip::DateTime::from_date_and_time(2020, 5, 12, 13, 14, 16).unwrap();
        let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
        zip.start_file(
            "script.sh",
            SimpleFileOptions::default()
                .unix_permissions(0o755)
                .last_modified_time(modified),
        )
        .unwrap();
        zip.write_all(b"#!/bin/sh\n").unwrap();
        zip.finish().unwrap();
        let output = extract(&path, "", root.path(), &AtomicBool::new(false), |_, _| {}).unwrap();
        let metadata = fs::metadata(output.join("script.sh")).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o755);
        assert_eq!(
            metadata.modified().unwrap(),
            zip_modified(modified).unwrap()
        );
    }
    #[test]
    fn seven_zip_unix_attributes_and_mtime_are_translated_without_privileged_bits() {
        let time = UNIX_EPOCH + Duration::new(1_600_000_000, 123_456_700);
        let mut entry = sevenz_rust2::ArchiveEntry::new_file("script.sh");
        entry.has_windows_attributes = true;
        entry.windows_attributes = (0o104755 << 16) | 0x8000;
        entry.has_last_modified_date = true;
        entry.last_modified_date = time.try_into().unwrap();
        let member = seven_member(&entry).unwrap();
        assert_eq!(member.mode, Some(0o755));
        assert_eq!(member.modified, Some(time));
        assert!(!member.link);
        entry.windows_attributes = (0o120777 << 16) | 0x8000;
        assert!(seven_member(&entry).unwrap().link);
        entry.windows_attributes = (0o010600 << 16) | 0x8000;
        assert!(seven_member(&entry).unwrap().link);
    }
    #[test]
    fn xz_dictionary_limit_rejects_tiny_archive_before_large_allocation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("dictionary-limit.tar.xz");
        fs::write(
            &path,
            include_bytes!("../tests/fixtures/dictionary-limit.tar.xz"),
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let error = inspect(&path, "", &cancel).unwrap_err().to_string();
        assert!(error.to_lowercase().contains("memory"), "{error}");
        assert!(extract(&path, "", root.path(), &cancel, |_, _| {}).is_err());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }
    #[test]
    fn seven_zip_data_and_encoded_header_memory_are_bounded_before_allocation() {
        let root = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        for (name, bytes, encoded_header) in [
            (
                "dictionary-limit.7z",
                include_bytes!("../tests/fixtures/dictionary-limit.7z").as_slice(),
                false,
            ),
            (
                "header-dictionary-limit.7z",
                include_bytes!("../tests/fixtures/header-dictionary-limit.7z").as_slice(),
                true,
            ),
            (
                "decoded-header-limit.7z",
                include_bytes!("../tests/fixtures/decoded-header-limit.7z").as_slice(),
                true,
            ),
        ] {
            let path = root.path().join(name);
            fs::write(&path, bytes).unwrap();
            if encoded_header {
                let error = inspect(&path, "", &cancel).unwrap_err().to_string();
                assert!(error.to_lowercase().contains("memory"), "{name}: {error}");
            } else {
                assert_eq!(inspect(&path, "", &cancel).unwrap().members.len(), 1);
            }
            let error = extract(&path, "", root.path(), &cancel, |_, _| {})
                .unwrap_err()
                .to_string();
            assert!(error.to_lowercase().contains("memory"), "{name}: {error}");
            assert!(!root.path().join(path.file_stem().unwrap()).exists());
        }
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 3);
    }
    #[test]
    fn zip_lzma_and_xz_cannot_bypass_the_dictionary_budget() {
        let root = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        for (name, bytes) in [
            (
                "xz-dictionary-limit.zip",
                include_bytes!("../tests/fixtures/xz-dictionary-limit.zip").as_slice(),
            ),
            (
                "lzma-dictionary-limit.zip",
                include_bytes!("../tests/fixtures/lzma-dictionary-limit.zip").as_slice(),
            ),
        ] {
            let path = root.path().join(name);
            fs::write(&path, bytes).unwrap();
            assert_eq!(inspect(&path, "", &cancel).unwrap().members.len(), 1);
            let error = extract(&path, "", root.path(), &cancel, |_, _| {})
                .unwrap_err()
                .to_string();
            assert!(
                error.to_lowercase().contains("memory limit"),
                "{name}: {error}"
            );
            assert!(!root.path().join(path.file_stem().unwrap()).exists());
        }
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }
    fn fixture(path: &Path, names: &[(&str, &str)]) {
        let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
        for (name, text) in names {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(text.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }
    #[test]
    fn browse_and_extract_keep_both_and_preserve_contents() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("fixture.zip");
        fixture(
            &path,
            &[("nested/hello.txt", "hello"), ("readme.txt", "readme")],
        );
        let cancel = AtomicBool::new(false);
        let archive = inspect(&path, "", &cancel).unwrap();
        assert!(archive.children(Path::new(""), "")[0].directory);
        fs::create_dir(root.path().join("fixture")).unwrap();
        let extracted = extract(&path, "", root.path(), &cancel, |_, _| {}).unwrap();
        assert_eq!(extracted.file_name().unwrap(), "fixture (1)");
        assert_eq!(
            fs::read_to_string(extracted.join("nested/hello.txt")).unwrap(),
            "hello"
        );
    }
    #[test]
    fn traversal_and_symlinks_never_publish() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("bad.zip");
        fixture(&path, &[("../outside", "bad")]);
        assert!(inspect(&path, "", &AtomicBool::new(false)).is_err());
        assert!(extract(&path, "", root.path(), &AtomicBool::new(false), |_, _| {}).is_err());
        assert!(!root.path().join("outside").exists());
        assert!(!root.path().join("bad").exists());
    }
    #[test]
    fn cancellation_preserves_existing_destination_and_cleans_stage() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("fixture.zip");
        fixture(&path, &[("hello.txt", "hello")]);
        assert!(extract(&path, "", root.path(), &AtomicBool::new(true), |_, _| {}).is_err());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn independent_seven_zip_and_xz_fixtures_browse_preview_and_extract() {
        let root = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        for (name, bytes, member) in [
            (
                "stored.7z",
                &include_bytes!("../tests/fixtures/stored.7z")[..],
                "hello.txt",
            ),
            (
                "nested.tar.xz",
                &include_bytes!("../tests/fixtures/nested.tar.xz")[..],
                "nested/hello.txt",
            ),
        ] {
            let path = root.path().join(name);
            fs::write(&path, bytes).unwrap();
            let archive = inspect(&path, "", &cancel).unwrap();
            assert_eq!(archive.members.len(), 1);
            assert_eq!(archive.members[0].path, Path::new(member));
            let (_temporary, preview) =
                read_member(&archive, Path::new(member), "", &cancel).unwrap();
            assert_eq!(fs::read(preview).unwrap(), b"seven fixture\n");
            let output = extract(&path, "", root.path(), &cancel, |_, _| {}).unwrap();
            assert_eq!(fs::read(output.join(member)).unwrap(), b"seven fixture\n");
        }
    }

    #[test]
    fn compressed_tar_browses_and_extracts_without_losing_nested_bytes() {
        let root = tempfile::tempdir().unwrap();
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(5);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "nested/hello.txt", &b"hello"[..])
            .unwrap();
        let bytes = tar.into_inner().unwrap();
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(&bytes).unwrap();
        let mut bzip = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
        bzip.write_all(&bytes).unwrap();
        let zstd = zstd::encode_all(&bytes[..], 1).unwrap();
        for (name, encoded) in [
            ("gzip.tar.gz", gzip.finish().unwrap()),
            ("bzip.tar.bz2", bzip.finish().unwrap()),
            ("zstd.tar.zst", zstd),
        ] {
            let path = root.path().join(name);
            fs::write(&path, encoded).unwrap();
            let archive = inspect(&path, "", &AtomicBool::new(false)).unwrap();
            assert_eq!(archive.members.len(), 1);
            let output =
                extract(&path, "", root.path(), &AtomicBool::new(false), |_, _| {}).unwrap();
            assert_eq!(fs::read(output.join("nested/hello.txt")).unwrap(), b"hello");
        }
    }

    #[test]
    fn tar_links_are_skipped_with_warnings_and_regular_members_extract() {
        fn special(builder: &mut tar::Builder<Vec<u8>>, name: &str, entry_type: tar::EntryType) {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(entry_type);
            header.set_mode(0o777);
            header.set_mtime(1);
            header.set_size(0);

            header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            header.set_cksum();
            builder.append(&header, &b""[..]).unwrap();
        }
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("linked.tar");
        let mut tar = tar::Builder::new(Vec::new());
        tar_entry(
            &mut tar,
            "readme.txt",
            false,
            0o644,
            1_500_000_000,
            b"hello\n",
        );
        special(&mut tar, "link", tar::EntryType::Symlink);
        special(&mut tar, "hard", tar::EntryType::Link);
        special(&mut tar, "pipe", tar::EntryType::Fifo);
        fs::write(&path, tar.into_inner().unwrap()).unwrap();
        let (archive, warnings) =
            inspect_with_warnings(&path, "", &AtomicBool::new(false)).unwrap();
        assert_eq!(archive.members.len(), 4);
        assert!(archive.members[1].link);
        assert!(warnings.is_empty());
        let (output, warnings) =
            extract_with_warnings(&path, "", root.path(), &AtomicBool::new(false), |_, _| {})
                .unwrap();
        assert_eq!(fs::read(output.join("readme.txt")).unwrap(), b"hello\n");
        assert!(!output.join("link").exists());
        assert!(!output.join("hard").exists());
        assert!(!output.join("pipe").exists());
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        for (name, warning) in [
            ("link", &warnings[0]),
            ("hard", &warnings[1]),
            ("pipe", &warnings[2]),
        ] {
            assert!(warning.contains(name), "{name}: {warning}");
        }
        assert!(
            warnings[0].contains("link or special file"),
            "{:?}",
            warnings
        );
    }
    #[test]
    fn duplicate_members_are_listed_once_and_extraction_keeps_the_first_record() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("duplicates.zip");

        fixture(&path, &[("./same.txt", "first"), ("same.txt", "second")]);
        let (archive, warnings) =
            inspect_with_warnings(&path, "", &AtomicBool::new(false)).unwrap();
        assert_eq!(archive.members.len(), 1);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("same.txt"), "{warnings:?}");
        let (output, warnings) =
            extract_with_warnings(&path, "", root.path(), &AtomicBool::new(false), |_, _| {})
                .unwrap();
        assert_eq!(
            fs::read_to_string(output.join("same.txt")).unwrap(),
            "first"
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let (_temporary, preview) =
            read_member(&archive, Path::new("same.txt"), "", &AtomicBool::new(false)).unwrap();
        assert_eq!(fs::read_to_string(preview).unwrap(), "first");
    }
    #[test]
    fn file_directory_conflicts_are_skipped_without_losing_other_members() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("conflict.tar");
        let mut tar = tar::Builder::new(Vec::new());
        tar_entry(&mut tar, "tree", false, 0o644, 1, b"file");
        tar_entry(&mut tar, "tree/child.txt", false, 0o644, 1, b"child");
        tar_entry(&mut tar, "keep.txt", false, 0o644, 1, b"keep");

        tar_entry(&mut tar, "folder/early.txt", false, 0o644, 1, b"early");
        tar_entry(&mut tar, "folder", false, 0o644, 1, b"file");
        tar_entry(&mut tar, "tail.txt", false, 0o644, 1, b"tail");
        fs::write(&path, tar.into_inner().unwrap()).unwrap();
        let (output, warnings) =
            extract_with_warnings(&path, "", root.path(), &AtomicBool::new(false), |_, _| {})
                .unwrap();
        assert_eq!(fs::read_to_string(output.join("keep.txt")).unwrap(), "keep");
        assert!(fs::read_to_string(output.join("tree")).unwrap() == "file");
        assert!(!output.join("tree/child.txt").exists());
        assert_eq!(
            fs::read_to_string(output.join("folder/early.txt")).unwrap(),
            "early"
        );
        assert!(output.join("folder").is_dir());
        assert_eq!(fs::read_to_string(output.join("tail.txt")).unwrap(), "tail");
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings.iter().any(|w| w.contains("tree/child.txt")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("conflicting with an extracted directory: folder")),
            "{warnings:?}"
        );
    }
    #[test]
    fn rar_helper_resolution_prefers_sibling_then_path_then_override() {
        let root = tempfile::tempdir().unwrap();
        let helper = root.path().join("rix-rar");
        fs::write(&helper, b"#!/bin/sh\n").unwrap();
        let sibling = root.path().join("sibling-rix-rar");
        assert!(find_rar_helper(&sibling, None, None).is_err());
        let search_path = root.path().as_os_str();
        assert_eq!(
            find_rar_helper(&sibling, Some(search_path), None).unwrap(),
            helper
        );
        let configured = root.path().join("configured-rix-rar");
        fs::write(&configured, b"#!/bin/sh\n").unwrap();
        assert_eq!(
            find_rar_helper(
                &sibling,
                Some(root.path().as_os_str()),
                Some(configured.as_os_str())
            )
            .unwrap(),
            helper,
            "the sibling directory takes precedence over PATH"
        );
        assert_eq!(
            find_rar_helper(&sibling, None, Some(configured.as_os_str())).unwrap(),
            configured
        );
        let error = find_rar_helper(&sibling, None, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("rix-rar"), "{error}");
    }
    #[test]
    fn rar_progress_lines_are_recognized_without_matching_diagnostics() {
        assert_eq!(rar_progress(b"progress 1048576\n"), Some(1_048_576));
        assert_eq!(rar_progress(b"progress 12\r\n"), Some(12));
        assert_eq!(rar_progress(b"progress nope\n"), None);
        assert_eq!(rar_progress(b"some decoder error\n"), None);
    }
}

#[derive(serde::Deserialize)]
struct RarMember {
    path: Vec<u8>,
    directory: bool,
    size: u64,
    link: bool,
}
#[derive(Default, serde::Deserialize)]
struct RarListing {
    #[serde(default)]
    members: Vec<RarMember>,
    #[serde(default)]
    warnings: Vec<String>,
}

const RAR_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const RAR_ERROR_BYTES: usize = 64 * 1024;
fn rar_idle_timeout() -> Duration {
    std::env::var("RIX_RAR_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(|seconds| Duration::from_secs(seconds.min(24 * 60 * 60)))
        .unwrap_or(RAR_IDLE_TIMEOUT)
}

fn find_rar_helper(
    sibling: &Path,
    search_path: Option<&std::ffi::OsStr>,
    configured: Option<&std::ffi::OsStr>,
) -> Result<PathBuf> {
    if sibling.is_file() {
        return Ok(sibling.to_owned());
    }
    if let Some(search_path) = search_path {
        for directory in std::env::split_paths(search_path) {
            let candidate = directory.join("rix-rar");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    if let Some(configured) = configured.filter(|value| !value.is_empty()) {
        let candidate = PathBuf::from(configured);
        ensure!(
            candidate.is_file(),
            "RIX_RAR_PATH does not point to the rix-rar helper"
        );
        return Ok(candidate);
    }
    bail!("The rix-rar helper is not installed; install rix-rar or set RIX_RAR_PATH")
}
fn rar_helper() -> Result<PathBuf> {
    let sibling = std::env::current_exe()?
        .parent()
        .map(|parent| parent.join("rix-rar"))
        .unwrap_or_else(|| PathBuf::from("rix-rar"));
    find_rar_helper(
        &sibling,
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("RIX_RAR_PATH").as_deref(),
    )
}

fn rar_progress(line: &[u8]) -> Option<u64> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    std::str::from_utf8(line.strip_prefix(b"progress ")?)
        .ok()?
        .parse()
        .ok()
}
fn rar_command(
    path: &Path,
    action: &str,
    destination: Option<&Path>,
    member: Option<&Path>,
    password: &str,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    use std::process::{Command, Stdio};
    let helper = rar_helper()?;
    let request = serde_json::json!({"action":action,"path":path.as_os_str().as_bytes(),"destination":destination.map(|path|path.as_os_str().as_bytes()),"member":member.map(|path|path.as_os_str().as_bytes()),"password":password});
    let mut child = Command::new(helper)
        .env_clear()
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&request)?)?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let progressed = Arc::new(Mutex::new(Instant::now()));
    let observed = progressed.clone();
    let output = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes);
        (result, bytes)
    });
    let errors = std::thread::spawn(move || {

        let mut reader = std::io::BufReader::new(stderr);
        let mut line = Vec::new();
        let mut kept = Vec::new();
        while matches!(reader.read_until(b'\n', &mut line), Ok(count) if count > 0) {
            if rar_progress(&line).is_some() {
                *observed.lock().unwrap_or_else(|error| error.into_inner()) = Instant::now();
            } else if kept.len() < RAR_ERROR_BYTES {
                kept.extend_from_slice(&line);
            }
            line.clear();
        }
        kept
    });
    let idle = rar_idle_timeout();
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("RAR operation cancelled");
        }
        let quiet = progressed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .elapsed();
        if quiet >= idle {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "RAR operation timed out after {} s without progress; set RIX_RAR_IDLE_TIMEOUT_SECS to allow slower archives",
                idle.as_secs()
            );
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let (result, bytes) = output
        .join()
        .map_err(|_| anyhow::anyhow!("RAR decoder output failed"))?;
    result?;
    let error = errors.join().unwrap_or_default();
    ensure!(status.success(), "{}", String::from_utf8_lossy(&error));
    ensure!(
        bytes.len() <= 16 * 1024 * 1024,
        "RAR archive index is too large"
    );
    Ok(bytes)
}
