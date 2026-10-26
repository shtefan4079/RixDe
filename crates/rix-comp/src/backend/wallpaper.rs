mod video;
use image::{
    imageops::{self, FilterType},
    AnimationDecoder, DynamicImage, ImageDecoder, Rgba, RgbaImage,
};
use rix_config::{WallpaperFit, WallpaperSettings};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::texture::TextureRenderBuffer,
            gles::{GlesRenderer, GlesTexture},
        },
    },
    output::Output,
    utils::{Rectangle, Size, Transform},
};
use std::{
    collections::HashMap,
    fs::File,
    io::BufReader,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant, SystemTime},
};

#[derive(Clone, PartialEq)]
struct Job {
    settings: WallpaperSettings,
    size: (u32, u32),
    modified: Option<SystemTime>,
}
struct Frame {
    generation: u64,
    pixels: RgbaImage,
}
struct Shared {
    job: Mutex<Option<(u64, Job)>>,
    wake: Condvar,
    generation: AtomicU64,
    stopped: AtomicBool,
}
struct Player {
    shared: Arc<Shared>,
    frames: Receiver<Frame>,
    requested: Option<Job>,
    buffer: Option<TextureRenderBuffer<GlesTexture>>,
    buffer_size: Option<(i32, i32)>,
    last_stat: Instant,
    metadata: MetadataProbe,
    pending_stat: Option<u64>,
}

struct MetadataShared {
    request: Mutex<Option<(u64, Option<PathBuf>)>>,
    result: Mutex<Option<(u64, Option<SystemTime>)>>,
    wake: Condvar,
    stopped: AtomicBool,
}

struct MetadataProbe {
    shared: Arc<MetadataShared>,
    generation: u64,
}
impl MetadataProbe {
    fn new() -> Self {
        Self::with_reader(|path| {
            path.as_ref()
                .and_then(|path| path.metadata().ok())
                .and_then(|metadata| metadata.modified().ok())
        })
    }
    fn with_reader(
        reader: impl Fn(Option<PathBuf>) -> Option<SystemTime> + Send + 'static,
    ) -> Self {
        let shared = Arc::new(MetadataShared {
            request: Mutex::new(None),
            result: Mutex::new(None),
            wake: Condvar::new(),
            stopped: AtomicBool::new(false),
        });
        let worker = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("rix-wallpaper-stat".into())
            .spawn(move || loop {
                let mut request = worker.request.lock().unwrap();
                while request.is_none() && !worker.stopped.load(Ordering::Relaxed) {
                    request = worker.wake.wait(request).unwrap();
                }
                if worker.stopped.load(Ordering::Relaxed) {
                    break;
                }
                let (generation, path) = request.take().unwrap();
                drop(request);
                let modified = reader(path);
                if worker.stopped.load(Ordering::Relaxed) {
                    break;
                }
                *worker.result.lock().unwrap() = Some((generation, modified));
            });
        if let Err(error) = spawned {

            tracing::warn!(%error, "Wallpaper metadata worker could not start");
        }
        Self {
            shared,
            generation: 0,
        }
    }
    fn request(&mut self, path: Option<PathBuf>) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        *self.shared.request.lock().unwrap() = Some((self.generation, path));
        self.shared.wake.notify_one();
        self.generation
    }
    fn poll(&self) -> Option<(u64, Option<SystemTime>)> {
        self.shared.result.try_lock().ok()?.take()
    }
}
impl Drop for MetadataProbe {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
    }
}
impl Drop for Player {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
    }
}
impl Player {
    fn new() -> Self {
        let shared = Arc::new(Shared {
            job: Mutex::new(None),
            wake: Condvar::new(),
            generation: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
        });
        let (tx, frames) = mpsc::sync_channel(1);
        let worker = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("rix-wallpaper".into())
            .spawn(move || loop {
                let mut pending = worker.job.lock().unwrap();
                while pending.is_none() && !worker.stopped.load(Ordering::Relaxed) {
                    pending = worker.wake.wait(pending).unwrap();
                }
                if worker.stopped.load(Ordering::Relaxed) {
                    break;
                }
                let (generation, job) = pending.take().unwrap();
                drop(pending);
                if let Err(error) = produce(&worker, &tx, generation, &job) {
                    tracing::warn!(%error, "Wallpaper could not be decoded");
                    let pixels = video_poster(&job).unwrap_or_else(|| solid(&job));
                    publish(&worker, &tx, generation, pixels);
                }
            });
        if let Err(error) = spawned {

            tracing::warn!(%error, "Wallpaper worker could not start");
        }
        Self {
            shared,
            frames,
            requested: None,
            buffer: None,
            buffer_size: None,
            last_stat: Instant::now(),
            metadata: MetadataProbe::new(),
            pending_stat: None,
        }
    }
    fn update(
        &mut self,
        renderer: &mut GlesRenderer,
        settings: &WallpaperSettings,
        size: (u32, u32),
    ) {
        let path_changed = self
            .requested
            .as_ref()
            .is_none_or(|job| job.settings.path != settings.path);
        let mut modified = (!path_changed)
            .then(|| self.requested.as_ref().and_then(|j| j.modified))
            .flatten();
        if let Some((generation, result)) = self.metadata.poll() {
            if !path_changed && self.pending_stat == Some(generation) {
                modified = result;
                self.pending_stat = None;
            }
        }
        if path_changed
            || (self.pending_stat.is_none() && self.last_stat.elapsed() >= Duration::from_secs(1))
        {
            self.pending_stat = Some(self.metadata.request(settings.path.clone()));
            self.last_stat = Instant::now();
        }
        let job = Job {
            settings: settings.clone(),
            size,
            modified,
        };
        if self.requested.as_ref() != Some(&job) {
            let generation = self.shared.generation.fetch_add(1, Ordering::Relaxed) + 1;
            *self.shared.job.lock().unwrap() = Some((generation, job.clone()));
            self.requested = Some(job);
            self.shared.wake.notify_one();
        }

        if let Ok(frame) = self.frames.try_recv() {
            if frame.generation != self.shared.generation.load(Ordering::Relaxed) {
                return;
            }
            if let Err(error) = self.upload(renderer, &frame.pixels) {
                tracing::warn!(%error, "Wallpaper texture upload failed");
            }
        }
    }

    fn upload(&mut self, renderer: &mut GlesRenderer, pixels: &RgbaImage) -> anyhow::Result<()> {
        let size = Size::from((pixels.width() as i32, pixels.height() as i32));
        if reuse_texture(self.buffer_size, (size.w, size.h)) {
            let buffer = self.buffer.as_mut().unwrap();
            buffer.update_from_memory(
                renderer,
                pixels.as_raw(),
                Rectangle::from_size(size),
                None,
            )?;
            return Ok(());
        }
        self.buffer = Some(TextureRenderBuffer::from_memory(
            renderer,
            pixels.as_raw(),
            Fourcc::Abgr8888,
            size,
            false,
            1,
            Transform::Normal,
            None,
        )?);
        self.buffer_size = Some((size.w, size.h));
        Ok(())
    }
}

fn reuse_texture(current: Option<(i32, i32)>, next: (i32, i32)) -> bool {
    current == Some(next)
}
#[derive(Default)]
pub(super) struct Wallpapers {
    players: HashMap<String, Player>,
}
impl Wallpapers {
    pub fn retain_outputs(&mut self, names: &[String]) {
        self.players.retain(|name, _| names.contains(name));
    }
    pub fn for_output(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        settings: &WallpaperSettings,
    ) -> Option<&TextureRenderBuffer<GlesTexture>> {
        let mode = output.current_mode()?;
        let size = output.current_transform().transform_size(mode.size);

        let scale = (16_777_216.0 / (size.w as f64 * size.h as f64))
            .sqrt()
            .min(1.);
        let size = (
            (size.w as f64 * scale).max(1.) as u32,
            (size.h as f64 * scale).max(1.) as u32,
        );
        let player = self
            .players
            .entry(output.name())
            .or_insert_with(Player::new);
        player.update(renderer, settings, size);
        player.buffer.as_ref()
    }
}
fn cancelled(shared: &Shared, generation: u64) -> bool {
    shared.stopped.load(Ordering::Relaxed)
        || shared.generation.load(Ordering::Relaxed) != generation
}
fn wait(shared: &Shared, generation: u64, duration: Duration) -> bool {
    let lock = shared.job.lock().unwrap();
    if cancelled(shared, generation) {
        return true;
    }
    let _ = shared.wake.wait_timeout(lock, duration).unwrap();
    cancelled(shared, generation)
}
fn limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(128 * 1024 * 1024);
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    limits
}
fn produce(
    shared: &Shared,
    tx: &SyncSender<Frame>,
    generation: u64,
    job: &Job,
) -> anyhow::Result<()> {
    let Some(path) = &job.settings.path else {
        publish(shared, tx, generation, solid(job));
        return Ok(());
    };
    if job.settings.is_video() {
        if let Some(poster) = video_poster(job) {
            publish(shared, tx, generation, poster);
            if !job.settings.animate {
                return Ok(());
            }
        }
        return video::produce(shared, tx, generation, job);
    }
    let mut reader = image::ImageReader::open(path)?.with_guessed_format()?;
    reader.limits(limits());
    let format = reader.format();
    let animate = job.settings.animate
        && matches!(
            format,
            Some(image::ImageFormat::Gif | image::ImageFormat::WebP)
        );
    if !animate {
        let image = reader.decode()?;
        if !cancelled(shared, generation) {
            publish(shared, tx, generation, prepare(image, job));
        }
        return Ok(());
    }
    drop(reader);
    while !cancelled(shared, generation) {
        let input = BufReader::new(File::open(path)?);
        let frames = match format {
            Some(image::ImageFormat::Gif) => {
                let mut decoder = image::codecs::gif::GifDecoder::new(input)?;
                decoder.set_limits(limits())?;
                decoder.into_frames()
            }
            Some(image::ImageFormat::WebP) => {
                let mut decoder = image::codecs::webp::WebPDecoder::new(input)?;
                decoder.set_limits(limits())?;
                if !decoder.has_animation() {
                    publish(
                        shared,
                        tx,
                        generation,
                        prepare(DynamicImage::from_decoder(decoder)?, job),
                    );
                    return Ok(());
                }
                decoder.into_frames()
            }
            _ => unreachable!(),
        };
        let mut count = 0;
        let mut last_emit = None::<Instant>;
        for frame in frames {
            if cancelled(shared, generation) {
                return Ok(());
            }
            let start = Instant::now();
            let frame = frame?;
            count += 1;
            let (numerator, denominator) = frame.delay().numer_denom_ms();
            let delay = Duration::from_secs_f64(
                (numerator as f64 / denominator.max(1) as f64 / 1000.).max(0.01),
            );
            if last_emit.is_none_or(|time| {
                time.elapsed()
                    >= Duration::from_secs_f64(1. / job.settings.max_fps.clamp(1, 60) as f64)
            }) {
                let pixels = prepare(DynamicImage::ImageRgba8(frame.into_buffer()), job);
                publish(shared, tx, generation, pixels);
                last_emit = Some(start);
            }
            if wait(shared, generation, delay.saturating_sub(start.elapsed())) {
                return Ok(());
            }
        }
        if count <= 1 {
            break;
        }
    }
    Ok(())
}
fn video_poster(job: &Job) -> Option<RgbaImage> {
    if !job.settings.is_video() {
        return None;
    }
    let poster = job.settings.path.as_ref()?.with_extension("jpg");
    let mut reader = image::ImageReader::open(poster)
        .ok()?
        .with_guessed_format()
        .ok()?;
    reader.limits(limits());
    Some(prepare(reader.decode().ok()?, job))
}
fn publish(shared: &Shared, tx: &SyncSender<Frame>, generation: u64, pixels: RgbaImage) {
    if !cancelled(shared, generation) {

        let mut frame = Frame { generation, pixels };
        loop {
            match tx.try_send(frame) {
                Ok(()) | Err(mpsc::TrySendError::Disconnected(_)) => break,
                Err(mpsc::TrySendError::Full(pending)) => frame = pending,
            }
            if wait(shared, generation, Duration::from_millis(4)) {
                break;
            }
        }
    }
}
fn solid(job: &Job) -> RgbaImage {
    let [r, g, b] = job.settings.background;
    RgbaImage::from_pixel(job.size.0, job.size.1, Rgba([r, g, b, 255]))
}
fn prepare(image: DynamicImage, job: &Job) -> RgbaImage {
    let (w, h) = job.size;
    let content = match job.settings.fit {
        WallpaperFit::Fill => image.resize_to_fill(w, h, FilterType::Triangle),
        WallpaperFit::Fit => image.resize(w, h, FilterType::Triangle),
        WallpaperFit::Stretch => image.resize_exact(w, h, FilterType::Triangle),
        WallpaperFit::Center => image,
    }
    .to_rgba8();
    let mut pixels = solid(job);
    imageops::overlay(
        &mut pixels,
        &content,
        (w as i64 - content.width() as i64) / 2,
        (h as i64 - content.height() as i64) / 2,
    );
    if job.settings.blur > 0. {

        let scale = (job.settings.blur / 4.).clamp(1., 8.);
        let small = imageops::resize(
            &pixels,
            (w as f32 / scale).max(1.) as u32,
            (h as f32 / scale).max(1.) as u32,
            FilterType::Triangle,
        );
        pixels = imageops::resize(
            &imageops::blur(&small, job.settings.blur / scale),
            w,
            h,
            FilterType::Triangle,
        );
    }
    if job.settings.dim > 0. {
        for pixel in pixels.pixels_mut() {
            for channel in &mut pixel.0[..3] {
                *channel = (*channel as f32 * (1. - job.settings.dim)) as u8;
            }
        }
    }
    pixels
}
#[cfg(test)]
mod tests {
    use super::*;

    const EVENT_TIMEOUT: Duration = Duration::from_secs(30);
    #[test]
    fn metadata_stall_keeps_requests_nonblocking_and_discards_obsolete_paths() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let paths = Arc::new(Mutex::new(Vec::new()));
        let observed = paths.clone();
        let mut probe = MetadataProbe::with_reader(move |path| {
            observed.lock().unwrap().push(path.clone());
            if path.as_deref() == Some(std::path::Path::new("slow")) {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(EVENT_TIMEOUT).unwrap();
            }
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(42))
        });
        probe.request(Some("slow".into()));
        started_rx.recv_timeout(EVENT_TIMEOUT).unwrap();
        probe.request(Some("obsolete".into()));
        let latest = probe.request(Some("current".into()));
        assert!(
            probe.poll().is_none(),
            "No synchronous stat is performed by the caller"
        );
        assert_eq!(
            probe.shared.request.lock().unwrap().as_ref().unwrap().0,
            latest
        );
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if let Some((generation, modified)) = probe.poll() {
                if generation == latest {
                    assert_eq!(
                        modified,
                        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(42))
                    );
                    break;
                }
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            *paths.lock().unwrap(),
            vec![Some("slow".into()), Some("current".into())]
        );
        let shared = probe.shared.clone();
        drop(probe);
        assert!(shared.stopped.load(Ordering::Relaxed));
    }

    #[test]
    fn placement_alpha_and_dimming_keep_opaque_letterbox() {
        let mut job = Job {
            settings: WallpaperSettings::default(),
            size: (8, 8),
            modified: None,
        };
        job.settings.fit = WallpaperFit::Fit;
        job.settings.background = [20, 40, 60];
        job.settings.dim = 0.5;
        let input =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(8, 4, Rgba([200, 100, 50, 255])));
        let result = prepare(input, &job);
        assert_eq!(result.get_pixel(0, 0).0, [10, 20, 30, 255]);
        assert_eq!(result.get_pixel(4, 4).0, [100, 50, 25, 255]);
    }
    #[test]
    fn gif_streams_frames_and_cancels_when_source_changes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("animated.gif");
        let mut encoder = image::codecs::gif::GifEncoder::new(File::create(&path).unwrap());
        for color in [[255, 0, 0, 255], [0, 255, 0, 255]] {
            encoder
                .encode_frame(image::Frame::from_parts(
                    RgbaImage::from_pixel(4, 4, Rgba(color)),
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(60, 1),
                ))
                .unwrap();
        }
        drop(encoder);
        let player = Player::new();
        let mut job = Job {
            settings: WallpaperSettings::default(),
            size: (8, 6),
            modified: None,
        };
        job.settings.path = Some(path);
        player.shared.generation.store(1, Ordering::Relaxed);
        *player.shared.job.lock().unwrap() = Some((1, job));
        player.shared.wake.notify_one();
        let first = player.frames.recv_timeout(EVENT_TIMEOUT).unwrap();
        let second = player.frames.recv_timeout(EVENT_TIMEOUT).unwrap();
        assert_eq!(first.pixels.get_pixel(2, 2).0, [255, 0, 0, 255]);
        assert_eq!(second.pixels.get_pixel(2, 2).0, [0, 255, 0, 255]);
        player.shared.generation.store(2, Ordering::Relaxed);
        player.shared.wake.notify_one();
    }

    #[test]
    fn decoded_frames_of_equal_size_reuse_both_cpu_and_gpu_buffers() {
        assert!(reuse_texture(Some((3840, 2160)), (3840, 2160)));
        assert!(!reuse_texture(Some((3840, 2160)), (3840, 2161)));
        assert!(!reuse_texture(Some((3840, 2160)), (1920, 1080)));
        assert!(
            !reuse_texture(None, (3840, 2160)),
            "The first frame always allocates"
        );
    }

    #[test]
    fn invalid_video_uses_its_static_poster_and_output_removal_stops_worker() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("broken.mp4");
        std::fs::write(&path, b"invalid video").unwrap();
        DynamicImage::ImageRgba8(RgbaImage::from_pixel(4, 4, Rgba([60, 100, 140, 255])))
            .to_rgb8()
            .save(path.with_extension("jpg"))
            .unwrap();
        let player = Player::new();
        let job = Job {
            settings: WallpaperSettings {
                path: Some(path),
                ..Default::default()
            },
            size: (4, 4),
            modified: None,
        };
        player.shared.generation.store(1, Ordering::Relaxed);
        *player.shared.job.lock().unwrap() = Some((1, job));
        player.shared.wake.notify_one();

        let poster = player.frames.recv_timeout(EVENT_TIMEOUT).unwrap().pixels;
        let fallback = player.frames.recv_timeout(EVENT_TIMEOUT).unwrap().pixels;
        assert!(poster == fallback);
        assert_ne!(fallback.get_pixel(0, 0).0, [22, 25, 34, 255]);
        let state = player.shared.clone();
        let mut wallpapers = Wallpapers::default();
        wallpapers.players.insert("disconnected".into(), player);
        wallpapers.retain_outputs(&[]);
        assert!(state.stopped.load(Ordering::Relaxed));
        assert!(wallpapers.players.is_empty());
    }
}
