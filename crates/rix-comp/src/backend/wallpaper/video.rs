use super::{cancelled, publish, wait, Frame, Job, Shared};
use anyhow::{bail, Context};
use image::RgbaImage;
use rix_config::WallpaperFit;
use std::{
    io::{ErrorKind, Read},
    os::{fd::OwnedFd, unix::net::UnixStream},
    process::{Child, Command, Stdio},
    sync::mpsc::{SyncSender, TrySendError},
    time::{Duration, Instant},
};

struct Decoder {
    child: Child,
    pixels: UnixStream,
}

impl Drop for Decoder {
    fn drop(&mut self) {

        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn filter(job: &Job) -> String {
    let (width, height) = job.size;
    let [r, g, b] = job.settings.background;
    let color = format!("0x{r:02x}{g:02x}{b:02x}");
    let placement = match job.settings.fit {
        WallpaperFit::Fill => format!(
            "scale={width}:{height}:force_original_aspect_ratio=increase,crop={width}:{height}"
        ),
        WallpaperFit::Fit => format!(
            "scale={width}:{height}:force_original_aspect_ratio=decrease,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:color={color}"
        ),
        WallpaperFit::Stretch => format!("scale={width}:{height}"),
        WallpaperFit::Center => format!(
            "crop='min(iw,{width})':'min(ih,{height})',pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:color={color}"
        ),
    };
    let mut result = format!(
        "fps={},format=rgba,{placement},setsar=1",
        job.settings.max_fps.clamp(1, 60)
    );
    if job.settings.blur > 0.0 {
        result.push_str(&format!(
            ",gblur=sigma={:.3}:steps=2",
            job.settings.blur.clamp(0.0, 80.0)
        ));
    }
    if job.settings.dim > 0.0 {
        let factor = 1.0 - job.settings.dim.clamp(0.0, 1.0);
        result.push_str(&format!(
            ",lutrgb=r='val*{factor:.6}':g='val*{factor:.6}':b='val*{factor:.6}'"
        ));
    }
    result
}

fn start(job: &Job) -> anyhow::Result<Decoder> {
    let path = job
        .settings
        .path
        .as_ref()
        .context("missing video path")?
        .canonicalize()?;
    if !path.is_file() {
        bail!("video wallpaper must be a regular local file");
    }
    let (pixels, output) = UnixStream::pair()?;
    pixels.set_read_timeout(Some(Duration::from_millis(50)))?;
    let mut command = Command::new("ffmpeg");
    rix_services::resources::ordinary_command(&mut command);
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-threads",
        "2",
        "-filter_threads",
        "1",
        "-protocol_whitelist",
        "file",
        "-format_whitelist",
        "mov,matroska,webm",
        "-stream_loop",
        "-1",
        "-i",
    ]);
    command
        .arg(path)
        .args(["-map", "0:v:0", "-an", "-sn", "-dn", "-vf"]);
    command.arg(filter(job));
    if !job.settings.animate {
        command.args(["-frames:v", "1"]);
    }
    let child = command
        .args([
            "-threads", "1", "-pix_fmt", "rgba", "-f", "rawvideo", "pipe:1",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(output)))
        .spawn()
        .context("could not start FFmpeg for video wallpaper")?;
    Ok(Decoder { child, pixels })
}

pub(super) fn produce(
    shared: &Shared,
    tx: &SyncSender<Frame>,
    generation: u64,
    job: &Job,
) -> anyhow::Result<()> {
    let bytes = (job.size.0 as usize)
        .checked_mul(job.size.1 as usize)
        .and_then(|size| size.checked_mul(4))
        .context("video dimensions overflow")?;
    if bytes == 0 || bytes > 64 * 1024 * 1024 {
        bail!("video wallpaper exceeds frame memory limit");
    }
    let mut decoder = start(job)?;
    let interval = Duration::from_secs_f64(1.0 / job.settings.max_fps.clamp(1, 60) as f64);
    let mut deadline = Instant::now();
    let mut first = true;
    while !cancelled(shared, generation) {
        let mut pixels = vec![0; bytes];
        let mut offset = 0;
        let read_start = Instant::now();
        while offset < bytes {
            if cancelled(shared, generation) {
                return Ok(());
            }
            match decoder.pixels.read(&mut pixels[offset..]) {
                Ok(0) => bail!("video decoder closed before a complete frame"),
                Ok(count) => offset += count,
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error.into()),
            }
            if read_start.elapsed() > Duration::from_secs(10) {
                bail!("video decoder stalled");
            }
        }
        if first {
            deadline = Instant::now();
        }
        if wait(
            shared,
            generation,
            deadline.saturating_duration_since(Instant::now()),
        ) {
            return Ok(());
        }
        let pixels =
            RgbaImage::from_raw(job.size.0, job.size.1, pixels).context("invalid video frame")?;
        if first {
            publish(shared, tx, generation, pixels);
            first = false;
        } else {

            match tx.try_send(Frame { generation, pixels }) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => return Ok(()),
            }
        }
        if !job.settings.animate {
            return Ok(());
        }
        deadline += interval;
        if deadline + interval < Instant::now() {
            deadline = Instant::now();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rix_config::WallpaperSettings;
    use std::sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Condvar, Mutex,
    };

    fn fixture() -> (tempfile::TempDir, Job) {
        let temporary = tempfile::tempdir().unwrap();

        let path = temporary.path().join("local video $(not-a-command).mp4");
        let mut encoder = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "rawvideo",
                "-pixel_format",
                "rgb24",
                "-video_size",
                "32x24",
                "-framerate",
                "60",
                "-i",
                "pipe:0",
                "-an",
                "-c:v",
                "libx264",
                "-threads",
                "1",
                "-pix_fmt",
                "yuv420p",
                "-y",
            ])
            .arg(&path)
            .stdin(Stdio::piped())
            .spawn()
            .expect("wallpaper integration tests require FFmpeg");
        {
            use std::io::Write;
            let mut input = encoder.stdin.take().unwrap();
            for index in 0..6 {
                input
                    .write_all(&[index * 40, 220 - index * 30, 30 + index * 20].repeat(32 * 24))
                    .unwrap();
            }
        }
        assert!(encoder.wait().unwrap().success());
        let settings = WallpaperSettings {
            path: Some(path),
            max_fps: 60,
            ..Default::default()
        };
        (
            temporary,
            Job {
                settings,
                size: (32, 24),
                modified: None,
            },
        )
    }

    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            job: Mutex::new(None),
            wake: Condvar::new(),
            generation: AtomicU64::new(1),
            stopped: AtomicBool::new(false),
        })
    }

    #[test]
    fn real_video_loops_and_cancels_with_bounded_delivery() {
        let (_temporary, job) = fixture();
        let state = shared();
        let worker_state = state.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            done_tx.send(produce(&worker_state, &tx, 1, &job)).unwrap();
        });
        let frames: Vec<_> = (0..8)
            .map(|_| rx.recv_timeout(Duration::from_secs(3)).unwrap().pixels)
            .collect();
        assert!(
            frames[0] == frames[6],
            "decoder must loop at the original frame boundary"
        );
        assert!(
            frames[0] != frames[3],
            "video must advance instead of displaying a poster"
        );

        std::thread::sleep(Duration::from_millis(80));
        state.generation.store(2, Ordering::Relaxed);
        state.wake.notify_all();
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn decoder_drop_reaps_child_and_disabled_animation_yields_one_frame() {
        let (_temporary, mut job) = fixture();
        let decoder = start(&job).unwrap();
        let pid = decoder.child.id();
        drop(decoder);
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        job.settings.animate = false;
        let (tx, rx) = mpsc::sync_channel(1);
        produce(&shared(), &tx, 1, &job).unwrap();
        assert_eq!(rx.try_recv().unwrap().pixels.dimensions(), (32, 24));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn real_decoder_letterboxes_dims_and_centers_to_requested_dimensions() {
        let (_temporary, mut job) = fixture();
        job.settings.animate = false;
        job.settings.fit = WallpaperFit::Fit;
        job.settings.background = [80, 120, 160];
        job.settings.dim = 0.5;
        job.size = (48, 48);
        let (tx, rx) = mpsc::sync_channel(1);
        produce(&shared(), &tx, 1, &job).unwrap();
        let pixels = rx.try_recv().unwrap().pixels;
        assert_eq!(pixels.dimensions(), (48, 48));
        assert_eq!(pixels.get_pixel(0, 0).0, [40, 60, 80, 255]);
        job.settings.fit = WallpaperFit::Center;
        job.size = (24, 36);
        produce(&shared(), &tx, 1, &job).unwrap();
        assert_eq!(rx.try_recv().unwrap().pixels.dimensions(), (24, 36));
    }
}
