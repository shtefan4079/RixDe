use crate::{SettingsWindow, WallpaperPreferences};
use rix_config::{ConfigManager, WallpaperFit};
use slint::ComponentHandle;
use std::sync::Arc;
fn sync(window: &SettingsWindow, settings: &rix_config::WallpaperSettings) {
    let ui = window.global::<WallpaperPreferences>();
    ui.set_path(
        settings
            .path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into(),
    );
    ui.set_filename(
        settings
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| crate::i18n::tr("Solid color").to_string())
            .into(),
    );
    ui.set_fit(match settings.fit {
        WallpaperFit::Fill => 0,
        WallpaperFit::Fit => 1,
        WallpaperFit::Stretch => 2,
        WallpaperFit::Center => 3,
    });
    ui.set_blur(settings.blur);
    ui.set_dim(settings.dim);
    ui.set_animate(settings.animate);
    ui.set_max_fps(settings.max_fps as i32);
    let [r, g, b] = settings.background;
    ui.set_fill_color(slint::Color::from_rgb_u8(r, g, b));
    preview(window, settings.path.clone());
}
fn preview(window: &SettingsWindow, path: Option<std::path::PathBuf>) {
    let ui = window.global::<WallpaperPreferences>();
    ui.set_preview(Default::default());
    let Some(path) = path else {
        return;
    };
    let selected_path = path.to_string_lossy().into_owned();
    let weak = window.as_weak();
    tokio::task::spawn_blocking(move || {
        let decoded = (|| -> anyhow::Result<_> {
            let temporary;
            let path = if rix_config::desktop::is_video_wallpaper(&path) {
                let poster = path.with_extension("jpg");
                if poster.is_file() {
                    poster
                } else {
                    temporary = tempfile::tempdir()?;
                    let poster = temporary.path().join("preview.png");
                    extract_video_preview(&path, &poster)?;
                    poster
                }
            } else {
                path
            };
            let mut reader = image::ImageReader::open(path)?.with_guessed_format()?;
            let mut limits = image::Limits::default();
            limits.max_alloc = Some(128 * 1024 * 1024);
            limits.max_image_width = Some(16384);
            limits.max_image_height = Some(16384);
            reader.limits(limits);
            Ok(reader.decode()?.thumbnail(960, 540).to_rgba8())
        })();
        let _ = weak.upgrade_in_event_loop(move |window| {
            let ui = window.global::<WallpaperPreferences>();
            if ui.get_path().as_str() != selected_path {
                return;
            }
            match decoded {
                Ok(pixels) => {
                    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                        pixels.as_raw(),
                        pixels.width(),
                        pixels.height(),
                    );
                    ui.set_preview(slint::Image::from_rgba8(buffer));
                    ui.set_error("".into());
                }
                Err(error) => ui.set_error(
                    format!("{}: {error}", crate::i18n::tr("Could not read the image")).into(),
                ),
            }
        });
    });
}

pub fn refresh_localized_labels(window: &SettingsWindow) {
    let ui = window.global::<WallpaperPreferences>();
    if ui.get_path().is_empty() {
        ui.set_filename(crate::i18n::tr("Solid color"));
    }
}

pub fn install(window: &SettingsWindow, config: Arc<ConfigManager>) {
    sync(window, &config.get_desktop().wallpaper);
    let baseline = Arc::new(std::sync::Mutex::new(config.get_desktop()));
    let weak = window.as_weak();
    window.global::<WallpaperPreferences>().on_choose(move || {
        let weak = weak.clone();
        if let Some(window) = weak.upgrade() { window.global::<WallpaperPreferences>().set_choosing(true); }
        tokio::spawn(async move {
            let result = crate::dialogs::choose_file(crate::i18n::tr("Choose wallpaper").as_str(), "Images and videos | *.png *.jpg *.jpeg *.webp *.gif *.bmp *.tif *.tiff *.mp4 *.m4v *.webm *.mkv *.mov *.PNG *.JPG *.JPEG *.GIF *.WEBP *.MP4 *.M4V *.WEBM *.MKV *.MOV").await;
            let _ = weak.upgrade_in_event_loop(move |window| {
                let ui = window.global::<WallpaperPreferences>(); ui.set_choosing(false);
                match result {
                    Ok(Some(path)) => {
                        ui.set_path(path.to_string_lossy().as_ref().into());
                        ui.set_filename(path.file_name().unwrap_or_default().to_string_lossy().as_ref().into());
                        preview(&window, Some(path));
                    }
                    Ok(None) => {}, Err(e) => ui.set_error(e.to_string().into()),
                }
            });
        });
    });
    let weak = window.as_weak();
    window.global::<WallpaperPreferences>().on_clear(move || {
        if let Some(window) = weak.upgrade() {
            let ui = window.global::<WallpaperPreferences>();
            ui.set_path("".into());
            ui.set_filename(crate::i18n::tr("Solid color"));
            ui.set_preview(Default::default());
            ui.set_error("".into());
        }
    });
    let weak = window.as_weak();
    window.global::<WallpaperPreferences>().on_apply(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let ui = window.global::<WallpaperPreferences>();
        let base = baseline.lock().unwrap().clone();
        let mut desktop = base.clone();
        let settings = &mut desktop.wallpaper;
        settings.path =
            (!ui.get_path().is_empty()).then(|| std::path::PathBuf::from(ui.get_path().as_str()));
        settings.fit = match ui.get_fit() {
            1 => WallpaperFit::Fit,
            2 => WallpaperFit::Stretch,
            3 => WallpaperFit::Center,
            _ => WallpaperFit::Fill,
        };
        settings.blur = ui.get_blur();
        settings.dim = ui.get_dim();
        settings.animate = ui.get_animate();
        settings.max_fps = ui.get_max_fps() as u32;
        let color = ui.get_fill_color();
        settings.background = [color.red(), color.green(), color.blue()];
        let baseline = baseline.clone();
        crate::jobs::run(
            &window,
            move || crate::jobs::commit_desktop(base, desktop),
            move |window, desktop| {
                sync(window, &desktop.wallpaper);
                *baseline.lock().unwrap() = desktop;
                window.global::<WallpaperPreferences>().set_error("".into());
                window.set_status_message(crate::i18n::tr("Wallpaper saved"));
            },
        );
    });
}

fn extract_video_preview(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> anyhow::Result<()> {
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    let source = source.canonicalize()?;
    anyhow::ensure!(
        source.is_file(),
        "video wallpaper must be a regular local file"
    );
    let mut child = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-threads",
            "2",
            "-filter_threads",
            "1",
            "-protocol_whitelist",
            "file",
            "-format_whitelist",
            "mov,matroska,webm",
            "-i",
        ])
        .arg(source)
        .args([
            "-map",
            "0:v:0",
            "-an",
            "-sn",
            "-dn",
            "-frames:v",
            "1",
            "-vf",
            "scale=960:540:force_original_aspect_ratio=decrease",
            "-threads",
            "1",
            "-y",
        ])
        .arg(destination)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                anyhow::ensure!(status.success(), "could not decode video preview");
                return Ok(());
            }
            Ok(None) if start.elapsed() < Duration::from_secs(5) => {
                std::thread::sleep(Duration::from_millis(25));
            }
            result => {
                let _ = child.kill();
                let _ = child.wait();
                result?;
                anyhow::bail!("video preview timed out");
            }
        }
    }
}
