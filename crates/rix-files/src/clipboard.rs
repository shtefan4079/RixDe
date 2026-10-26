use std::{
    collections::HashSet,
    io::{self, Read},
    os::fd::AsRawFd,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_FILES: usize = 10_000;

fn valid_path(path: &Path) -> bool {
    path.is_absolute() && path != Path::new("/") && !path.as_os_str().as_bytes().contains(&0)
}
pub fn encode(paths: &[PathBuf], cut: bool) -> Result<(Vec<u8>, Vec<u8>), String> {
    if paths.is_empty() || paths.len() > MAX_FILES || paths.iter().any(|p| !valid_path(p)) {
        return Err("Invalid file clipboard paths".into());
    }
    let uris = paths
        .iter()
        .map(|p| {
            url::Url::from_file_path(p)
                .map(|u| u.to_string())
                .map_err(|_| "Invalid local path".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let gnome = format!("{}\n{}", if cut { "cut" } else { "copy" }, uris.join("\n")).into_bytes();
    let uri_list = format!("{}\r\n", uris.join("\r\n")).into_bytes();
    if gnome.len() > MAX_BYTES || uri_list.len() > MAX_BYTES {
        return Err("Clipboard file list is too large".into());
    }
    Ok((gnome, uri_list))
}
pub fn decode(data: &[u8], gnome: bool) -> Result<(Vec<PathBuf>, bool), String> {
    if data.len() > MAX_BYTES {
        return Err("Clipboard file list is too large".into());
    }
    let text = std::str::from_utf8(data).map_err(|e| e.to_string())?;
    let mut lines = text.lines();
    let cut = if gnome {
        match lines.next() {
            Some("cut") => true,
            Some("copy") => false,
            _ => return Err("Invalid file clipboard".into()),
        }
    } else {
        false
    };
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for line in lines.filter(|s| !s.is_empty() && !s.starts_with('#')) {
        let uri = url::Url::parse(line).map_err(|e| e.to_string())?;

        if uri.query().is_some() || uri.fragment().is_some() {
            return Err("File clipboard URI contains a query or fragment".into());
        }
        let path = uri
            .to_file_path()
            .map_err(|_| "Only local file URIs can be pasted")?;
        if !valid_path(&path) {
            return Err("Invalid file clipboard path".into());
        }
        if seen.insert(path.clone()) {
            paths.push(path);
        }
        if paths.len() > MAX_FILES {
            return Err("Clipboard contains too many files".into());
        }
    }
    if paths.is_empty() {
        return Err("The clipboard contains no files".into());
    }
    Ok((paths, cut))
}
pub fn publish(paths: &[PathBuf], cut: bool) -> Result<(), String> {
    use wl_clipboard_rs::copy::{MimeSource, MimeType, Options, Source};
    let (gnome, uris) = encode(paths, cut)?;

    let mut options = Options::new();
    options.foreground(true);
    let prepared = options
        .prepare_copy_multi(vec![
            MimeSource {
                source: Source::Bytes(gnome.into_boxed_slice()),
                mime_type: MimeType::Specific("x-special/gnome-copied-files".into()),
            },
            MimeSource {
                source: Source::Bytes(uris.into_boxed_slice()),
                mime_type: MimeType::Specific("text/uri-list".into()),
            },
        ])
        .map_err(|e| e.to_string())?;
    std::thread::Builder::new()
        .name("rix-file-clipboard".into())
        .spawn(move || {
            let _ = prepared.serve();
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn path_text(paths: &[PathBuf]) -> Result<String, String> {
    if paths.is_empty()
        || paths.len() > MAX_FILES
        || paths
            .iter()
            .any(|path| !path.is_absolute() || path.as_os_str().as_bytes().contains(&0))
    {
        return Err("Invalid paths".into());
    }
    let text = paths
        .iter()
        .map(|path| {
            path.to_str()
                .filter(|text| !text.contains(['\n', '\r']))
                .map(str::to_owned)
                .or_else(|| {
                    url::Url::from_file_path(path)
                        .ok()
                        .map(|url| url.to_string())
                })
                .ok_or_else(|| "Invalid path".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?
        .join("\n");
    if text.len() > MAX_BYTES {
        return Err("Paths are too large".into());
    }
    Ok(text)
}

pub fn publish_text(text: String) -> Result<(), String> {
    use wl_clipboard_rs::copy::{MimeType, Options, Source};
    if text.len() > MAX_BYTES {
        return Err("Clipboard text is too large".into());
    }
    let mut options = Options::new();
    options.foreground(true);
    let prepared = options
        .prepare_copy(
            Source::Bytes(text.into_bytes().into_boxed_slice()),
            MimeType::Text,
        )
        .map_err(|error| error.to_string())?;
    std::thread::Builder::new()
        .name("rix-path-clipboard".into())
        .spawn(move || {
            let _ = prepared.serve();
        })
        .map_err(|error| error.to_string())?;
    Ok(())
}
pub fn read() -> Result<(Vec<PathBuf>, bool), String> {
    use wl_clipboard_rs::paste::{self, ClipboardType, MimeType, Seat};
    let types = paste::get_mime_types(ClipboardType::Regular, Seat::Unspecified)
        .map_err(|e| e.to_string())?;
    let gnome = types.contains("x-special/gnome-copied-files");
    let mime = if gnome {
        "x-special/gnome-copied-files"
    } else if types.contains("text/uri-list") {
        "text/uri-list"
    } else {
        return Err("The clipboard contains no files".into());
    };
    let (pipe, _) = paste::get_contents(
        ClipboardType::Regular,
        Seat::Unspecified,
        MimeType::Specific(mime),
    )
    .map_err(|e| e.to_string())?;
    let bytes = read_bounded(pipe, Duration::from_secs(3))?;
    decode(&bytes, gnome)
}

pub fn read_bounded(mut pipe: impl Read + AsRawFd, timeout: Duration) -> Result<Vec<u8>, String> {
    let fd = pipe.as_raw_fd();

    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut buffer = [0; 16 * 1024];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("Clipboard owner did not finish the file transfer in time".into());
        }
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let wait_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;

        let ready = unsafe { libc::poll(&mut poll_fd, 1, wait_ms) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.to_string());
        }
        if ready == 0 {
            continue;
        }
        if poll_fd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err("Clipboard pipe became unavailable".into());
        }
        match pipe.read(&mut buffer) {
            Ok(0) => return Ok(bytes),
            Ok(size) => {
                if bytes.len() + size > MAX_BYTES {
                    return Err("Clipboard file list is too large".into());
                }
                bytes.extend_from_slice(&buffer[..size]);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copied_path_text_is_readable_and_unambiguous_for_raw_unix_names() {
        use std::os::unix::ffi::OsStringExt;
        assert_eq!(
            path_text(&[PathBuf::from("/tmp/hello world.txt")]).unwrap(),
            "/tmp/hello world.txt"
        );
        let raw = PathBuf::from(std::ffi::OsString::from_vec(
            b"/tmp/raw-\xff\n.txt".to_vec(),
        ));
        let text = path_text(std::slice::from_ref(&raw)).unwrap();
        assert_eq!(url::Url::parse(&text).unwrap().to_file_path().unwrap(), raw);
        assert!(!text.contains('\n'));
        assert!(path_text(&["relative".into()]).is_err());
    }
    #[test]
    fn file_clipboard_roundtrip() {
        let paths = vec![PathBuf::from("/tmp/space # %\nfile")];
        let (g, u) = encode(&paths, true).unwrap();
        assert_eq!(decode(&g, true).unwrap(), (paths.clone(), true));
        assert_eq!(decode(&u, false).unwrap(), (paths, false));
        assert!(decode(b"https://example.com/file", false).is_err());
    }
    #[test]
    fn clipboard_preserves_raw_paths_and_rejects_ambiguous_or_invalid_sources() {
        use std::os::unix::ffi::OsStringExt;
        let paths = vec![PathBuf::from(std::ffi::OsString::from_vec(
            b"/tmp/\xff %?#\n".to_vec(),
        ))];
        let (gnome, _) = encode(&paths, false).unwrap();
        assert_eq!(decode(&gnome, true).unwrap(), (paths, false));
        for data in [
            b"file:///tmp/file?query".as_slice(),
            b"file:///tmp/file#fragment",
            b"file:///tmp/%00",
            b"file:///",
            b"file://remote/tmp/file",
        ] {
            assert!(decode(data, false).is_err(), "accepted {data:?}");
        }
        assert!(encode(&[], false).is_err());
        assert!(encode(&["relative".into()], false).is_err());
        assert_eq!(
            decode(b"# comment\r\nfile:///tmp/a\r\nfile:///tmp/a\r\n", false).unwrap(),
            (vec!["/tmp/a".into()], false)
        );
    }
    #[test]
    fn stalled_clipboard_pipe_has_a_deadline() {
        let (reader, _owner) = std::os::unix::net::UnixStream::pair().unwrap();
        let start = Instant::now();
        assert!(read_bounded(reader, Duration::from_millis(20)).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn completed_clipboard_pipe_is_read_to_eof() {
        use std::io::Write;
        let (reader, mut owner) = std::os::unix::net::UnixStream::pair().unwrap();
        owner.write_all(b"file:///tmp/a\r\n").unwrap();
        drop(owner);
        assert_eq!(
            read_bounded(reader, Duration::from_secs(1)).unwrap(),
            b"file:///tmp/a\r\n"
        );
    }
}
