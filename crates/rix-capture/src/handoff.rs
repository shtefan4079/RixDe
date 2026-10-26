use anyhow::{Context, Result};
use image::RgbaImage;
use std::{
    fs::File,
    io::{BufReader, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::process::CommandExt,
    },
    process::Command,
};

pub fn create() -> Result<File> {

    let fd = unsafe { libc::memfd_create(c"rix-capture-pixels".as_ptr(), libc::MFD_CLOEXEC) };
    anyhow::ensure!(
        fd >= 0,
        "Could not create private capture storage: {}",
        std::io::Error::last_os_error()
    );

    Ok(unsafe { File::from_raw_fd(fd) })
}

pub fn write(file: &mut File, png: &[u8]) -> Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(png)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(())
}

pub fn read(file: &mut File) -> Result<RgbaImage> {
    anyhow::ensure!(
        file.metadata()?.len() <= super::editor::MAX_PIXELS * 4 + 1024 * 1024,
        "Capture transfer is too large"
    );
    file.seek(SeekFrom::Start(0))?;
    decode(BufReader::new(file))
}

pub fn open_image(path: &std::path::Path) -> Result<RgbaImage> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .context("Could not open image")?;
    let metadata = file.metadata()?;
    anyhow::ensure!(metadata.is_file(), "Image source is not a regular file");
    anyhow::ensure!(
        metadata.len() <= super::editor::MAX_PIXELS * 4 + 1024 * 1024,
        "Image file is too large"
    );

    decode(BufReader::new(ImageSource {
        file,
        length: metadata.len(),
        position: 0,
    }))
}

struct ImageSource {
    file: File,
    length: u64,
    position: u64,
}
impl std::io::Read for ImageSource {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = buffer
            .len()
            .min(self.length.saturating_sub(self.position) as usize);
        let read = self.file.read(&mut buffer[..count])?;
        self.position += read as u64;
        Ok(read)
    }
}
impl Seek for ImageSource {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let target = match from {
            SeekFrom::Start(position) => Some(position),
            SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
            SeekFrom::End(offset) => self.length.checked_add_signed(offset),
        }
        .filter(|position| *position <= self.length)
        .ok_or(std::io::ErrorKind::InvalidInput)?;
        self.position = self.file.seek(SeekFrom::Start(target))?;
        Ok(self.position)
    }
}

pub fn decode(reader: impl std::io::BufRead + Seek) -> Result<RgbaImage> {
    let mut reader = image::ImageReader::new(reader).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(super::editor::MAX_PIXELS * 4 + 1024 * 1024);
    reader.limits(limits);
    let pixels = reader.decode()?.into_rgba8();
    anyhow::ensure!(
        pixels.width() > 0
            && pixels.height() > 0
            && pixels.width() as u64 * pixels.height() as u64 <= super::editor::MAX_PIXELS,
        "Image exceeds the capture limit"
    );
    Ok(pixels)
}

pub fn inherited(fd: RawFd) -> Result<File> {
    anyhow::ensure!(fd >= 3, "Invalid capture descriptor");

    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    anyhow::ensure!(flags >= 0, "Capture descriptor is unavailable");

    anyhow::ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } >= 0,
        "Could not protect capture descriptor"
    );

    let file = unsafe { File::from_raw_fd(fd) };
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "Capture descriptor is not a file"
    );
    Ok(file)
}

pub fn pass(command: &mut Command, file: &File, argument: &str) {
    let fd = file.as_raw_fd();
    command.arg(argument).arg(fd.to_string());

    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

pub fn editor_command(file: &File, preview: bool, monitor: &str, status: &str) -> Result<Command> {
    let mut command =
        Command::new(std::env::current_exe().context("Capture executable is unavailable")?);
    pass(&mut command, file, "--input-fd");
    command
        .arg("--monitor-label")
        .arg(monitor)
        .arg("--initial-status")
        .arg(status);
    if preview {
        command.arg("--preview");
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            use std::os::unix::ffi::{OsStrExt, OsStringExt};
            let template = std::env::temp_dir().join("rix-capture-test-XXXXXX");
            let mut bytes = std::ffi::CString::new(template.as_os_str().as_bytes())
                .unwrap()
                .into_bytes_with_nul();
            assert!(!unsafe { libc::mkdtemp(bytes.as_mut_ptr().cast()) }.is_null());
            bytes.pop();
            Self(std::ffi::OsString::from_vec(bytes).into())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn named_pipes_and_other_special_files_are_rejected_before_reading() {
        use std::os::unix::ffi::OsStrExt;
        let fixture = Fixture::new();
        let path = fixture.0.join("image.png");
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        let error = open_image(&path).unwrap_err();
        std::fs::remove_file(path).unwrap();
        assert!(error.to_string().contains("regular file"));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(open_image(std::path::Path::new("/dev/zero")).is_err());
    }

    #[test]
    fn file_loader_preserves_pixels_and_enforces_encoded_limit() {
        let fixture = Fixture::new();
        let path = fixture.0.join("image.png");
        let pixels = RgbaImage::from_pixel(3, 2, image::Rgba([12, 23, 34, 255]));
        std::fs::write(&path, super::super::encoding::encode_png(&pixels).unwrap()).unwrap();
        assert_eq!(open_image(&path).unwrap(), pixels);
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(super::super::editor::MAX_PIXELS * 4 + 1024 * 1024 + 1)
            .unwrap();
        assert!(open_image(&path)
            .unwrap_err()
            .to_string()
            .contains("too large"));
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn image_reader_cannot_seek_or_read_past_its_opened_snapshot() {
        let mut file = create().unwrap();
        file.write_all(b"before").unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut source = ImageSource {
            file,
            length: 3,
            position: 0,
        };
        let mut bytes = Vec::new();
        source.read_to_end(&mut bytes).unwrap();
        assert_eq!(&bytes, b"bef");
        assert_eq!(source.seek(SeekFrom::End(0)).unwrap(), 3);
        assert!(source.seek(SeekFrom::Start(4)).is_err());
        assert!(source.seek(SeekFrom::Current(-4)).is_err());
    }
    #[test]
    fn private_transfer_preserves_pixels_and_does_not_create_a_named_file() {
        let pixels = RgbaImage::from_fn(91, 53, |x, y| {
            image::Rgba([x as u8, y as u8, (x ^ y) as u8, 255])
        });
        let mut file = create().unwrap();
        assert!(
            std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
                .unwrap()
                .to_string_lossy()
                .starts_with("/memfd:rix-capture-pixels")
        );
        write(
            &mut file,
            &super::super::encoding::encode_png(&pixels).unwrap(),
        )
        .unwrap();
        assert_eq!(read(&mut file).unwrap(), pixels);
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }

    #[test]
    fn one_descriptor_is_inherited_by_a_child_without_unprotecting_the_parent() {
        let file = create().unwrap();
        let fd = file.as_raw_fd();
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(format!("test -f /proc/self/fd/{fd}"));
        pass(&mut command, &file, "--unused");
        assert!(command.status().unwrap().success());
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
    }
}
