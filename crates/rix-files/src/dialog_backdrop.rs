use crate::{DialogBackdrop, FilesWindow, Theme};
use image::{imageops, RgbaImage};
use slint::{ComponentHandle, Image, SharedPixelBuffer};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc, Arc,
};

const MAX_SOURCE_PIXELS: u64 = 8_000_000;
const MAX_EDGE: u32 = 960;
struct Job {
    generation: u64,
    source: RgbaImage,
    background: [u8; 3],
    sigma: f32,
}

fn blur(mut source: RgbaImage, background: [u8; 3], sigma: f32) -> RgbaImage {

    for pixel in source.pixels_mut() {
        let alpha = u32::from(pixel[3]);
        for channel in 0..3 {
            pixel[channel] = ((u32::from(pixel[channel]) * alpha
                + u32::from(background[channel]) * (255 - alpha)
                + 127)
                / 255) as u8;
        }
        pixel[3] = 255;
    }
    let ratio = (MAX_EDGE as f32 / source.width().max(source.height()) as f32).min(1.);
    let scaled = if ratio < 1. {
        imageops::resize(
            &source,
            (source.width() as f32 * ratio).round().max(1.) as u32,
            (source.height() as f32 * ratio).round().max(1.) as u32,
            imageops::FilterType::Triangle,
        )
    } else {
        source
    };
    imageops::blur(&scaled, (sigma * ratio).clamp(2., 24.))
}

pub fn install(window: &FilesWindow) {
    let generation = Arc::new(AtomicU64::new(0));
    let (jobs, receiver) = mpsc::sync_channel::<Job>(1);
    let weak = window.as_weak();
    let current = generation.clone();
    std::thread::spawn(move || {
        while let Ok(mut job) = receiver.recv() {
            for newer in receiver.try_iter() {
                job = newer;
            }
            let image = blur(job.source, job.background, job.sigma);
            let current = current.clone();
            if weak
                .upgrade_in_event_loop(move |window| {
                    if job.generation != current.load(Ordering::Relaxed)
                        || window.get_dialog_kind() == 0
                    {
                        return;
                    }
                    window
                        .global::<DialogBackdrop>()
                        .set_image(Image::from_rgba8(SharedPixelBuffer::clone_from_slice(
                            image.as_raw(),
                            image.width(),
                            image.height(),
                        )));
                    window.global::<DialogBackdrop>().set_ready(true);
                    tracing::debug!(
                        width = image.width(),
                        height = image.height(),
                        "Local dialog backdrop prepared"
                    );
                })
                .is_err()
            {
                break;
            }
        }
    });
    let weak = window.as_weak();
    let next = generation.clone();
    window.global::<DialogBackdrop>().on_prepare(move || {
        next.fetch_add(1, Ordering::Relaxed);
        let Some(window) = weak.upgrade() else {
            return;
        };
        let backdrop = window.global::<DialogBackdrop>();
        backdrop.set_ready(false);
        if !window.get_using_desktop_material() || window.get_dialog_kind() == 10 {
            return;
        }
        let size = window.window().size();
        if size.width == 0
            || size.height == 0
            || u64::from(size.width) * u64::from(size.height) > MAX_SOURCE_PIXELS
        {
            return;
        }
        let Ok(pixels) = window.window().take_snapshot() else {
            return;
        };
        let Some(source) =
            RgbaImage::from_raw(pixels.width(), pixels.height(), pixels.as_bytes().to_vec())
        else {
            return;
        };
        let theme = window.global::<Theme>();
        let color = theme.get_window_background();
        let job = Job {
            generation: next.load(Ordering::Relaxed),
            source,
            background: [color.red(), color.green(), color.blue()],
            sigma: backdrop.get_radius().clamp(1., 128.) * window.window().scale_factor(),
        };

        let _ = jobs.try_send(job);
    });
    let weak = window.as_weak();
    let next = generation.clone();
    window.global::<DialogBackdrop>().on_clear(move || {
        next.fetch_add(1, Ordering::Relaxed);
        if let Some(window) = weak.upgrade() {
            let backdrop = window.global::<DialogBackdrop>();
            backdrop.set_ready(false);
            backdrop.set_image(Image::default());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_blur_removes_sharp_content_and_transparent_holes_with_bounded_size() {
        let source = RgbaImage::from_fn(2000, 1000, |x, y| {
            if y < 100 {
                image::Rgba([255, 255, 255, 0])
            } else {
                image::Rgba([if x % 2 == 0 { 0 } else { 255 }, 40, 80, 255])
            }
        });
        let blurred = blur(source, [20, 30, 40], 14.);
        assert_eq!(blurred.dimensions(), (960, 480));
        assert!(blurred.pixels().all(|p| p[3] == 255));
        assert_eq!(blurred.get_pixel(400, 2).0, [20, 30, 40, 255]);
        let a = blurred.get_pixel(400, 300)[0];
        let b = blurred.get_pixel(401, 300)[0];
        assert!((i16::from(a) - i16::from(b)).abs() <= 2);
        assert!((100..155).contains(&a));
    }
}
