#[cfg(test)]
use smithay::{
    backend::renderer::{
        damage::OutputDamageTracker, gles::GlesTexture, Bind, ExportMem, Offscreen, TextureMapping,
    },
    utils::Rectangle,
};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{element::RenderElement, gles::GlesRenderer},
    },
    output::Output,
    utils::Transform,
};

type Job = (image::RgbaImage, bool, Vec<rix_ipc::ScreenshotCommand>);

#[cfg(test)]
#[path = "screenshot_quality_tests.rs"]
mod quality_tests;

impl crate::state::RixState {
    pub(crate) fn screenshots_for_output(
        &mut self,
        output: &Output,
    ) -> Vec<rix_ipc::ScreenshotCommand> {
        self.screenshot_pending
            .extend(self.screenshot_rx.try_iter());
        let names: Vec<_> = self.space.outputs().map(Output::name).collect();
        route_requests(&mut self.screenshot_pending, &output.name(), &names)
    }
}

fn route_requests(
    pending: &mut Vec<rix_ipc::ScreenshotCommand>,
    output: &str,
    names: &[String],
) -> Vec<rix_ipc::ScreenshotCommand> {
    let mut ready = Vec::new();
    for request in std::mem::take(pending) {
        if request.responder.is_closed() {
            continue;
        }
        if request
            .output
            .as_ref()
            .is_some_and(|name| !names.contains(name))
        {
            let _ = request
                .responder
                .send(Err("Requested monitor is unavailable".into()));
        } else if request.output.as_ref().is_none_or(|name| name == output) {
            ready.push(request);
        } else {
            pending.push(request);
        }
    }
    ready
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    #[test]
    fn selected_output_never_falls_back_to_another_monitor() {
        let (sender, mut reply) = tokio::sync::oneshot::channel();
        let (invalid, mut error) = tokio::sync::oneshot::channel();
        let mut pending = vec![
            rix_ipc::ScreenshotCommand {
                output: Some("second".into()),
                path: None,
                responder: sender,
            },
            rix_ipc::ScreenshotCommand {
                output: Some("gone".into()),
                path: None,
                responder: invalid,
            },
        ];
        let names = vec!["first".into(), "second".into()];
        assert!(route_requests(&mut pending, "first", &names).is_empty());
        assert!(error
            .try_recv()
            .unwrap()
            .unwrap_err()
            .contains("unavailable"));
        assert!(reply.try_recv().is_err());
        let mut selected = route_requests(&mut pending, "second", &names);
        assert_eq!(selected.len(), 1);
        assert!(pending.is_empty());
        selected
            .pop()
            .unwrap()
            .responder
            .send(Ok("second.png".into()))
            .unwrap();
        assert_eq!(reply.try_recv().unwrap().unwrap(), "second.png");
    }
}
#[derive(Default)]
pub(crate) struct Writer {
    sender: Option<std::sync::mpsc::SyncSender<Job>>,
    targets: std::collections::HashMap<String, crate::readback::Target>,
    downloads: Vec<Pending>,
}
impl Writer {
    fn save(
        &mut self,
        pixels: image::RgbaImage,
        flipped: bool,
        requests: Vec<rix_ipc::ScreenshotCommand>,
    ) {
        if self.sender.is_none() {
            let (sender, receiver) = std::sync::mpsc::sync_channel::<Job>(2);
            match std::thread::Builder::new()
                .name("rix-screenshot-writer".into())
                .spawn(move || {
                    for (mut pixels, flipped, requests) in receiver {
                        if flipped {
                            image::imageops::flip_vertical_in_place(&mut pixels);
                        }
                        for request in requests {
                            let path = request.path.unwrap_or_else(|| {
                                let directory = std::env::var_os("XDG_RUNTIME_DIR")
                                    .map(std::path::PathBuf::from)
                                    .unwrap_or_else(std::env::temp_dir);
                                directory
                                    .join(format!(
                                        "rix-screenshot-{}.png",
                                        chrono::Local::now().format("%Y%m%d_%H%M%S_%f")
                                    ))
                                    .to_string_lossy()
                                    .into_owned()
                            });
                            let result = pixels
                                .save(&path)
                                .map(|()| path)
                                .map_err(|error| error.to_string());
                            let _ = request.responder.send(result);
                        }
                    }
                }) {
                Ok(_) => self.sender = Some(sender),
                Err(error) => {
                    for request in requests {
                        let _ = request.responder.send(Err(error.to_string()));
                    }
                    return;
                }
            }
        }
        if let Err(error) = self
            .sender
            .as_ref()
            .unwrap()
            .try_send((pixels, flipped, requests))
        {
            let (std::sync::mpsc::TrySendError::Full((_, _, requests))
            | std::sync::mpsc::TrySendError::Disconnected((_, _, requests))) = error;
            for request in requests {
                let _ = request
                    .responder
                    .send(Err("Screenshot writer is busy".into()));
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn capture<E: RenderElement<GlesRenderer>>(
    renderer: &mut GlesRenderer,
    output: &Output,
    elements: &[E],
) -> anyhow::Result<image::RgbaImage> {
    let mode = output
        .current_mode()
        .ok_or_else(|| anyhow::anyhow!("Output has no mode"))?;
    let size = output.current_transform().transform_size(mode.size);
    anyhow::ensure!(
        size.w > 0 && size.h > 0 && size.w as i64 * size.h as i64 <= 33_177_600,
        "Screenshot dimensions exceed the capture limit"
    );
    let buffer_size = (size.w, size.h).into();
    let mut texture: GlesTexture = renderer.create_buffer(Fourcc::Abgr8888, buffer_size)?;
    let mut framebuffer = renderer.bind(&mut texture)?;
    OutputDamageTracker::new(
        size,
        output.current_scale().fractional_scale(),
        Transform::Flipped180,
    )
    .render_output(renderer, &mut framebuffer, 0, elements, [0., 0., 0., 1.])?;
    let mapping = renderer.copy_framebuffer(
        &framebuffer,
        Rectangle::from_size(buffer_size),
        Fourcc::Abgr8888,
    )?;
    let flipped = mapping.flipped();
    let mut image = image::RgbaImage::from_raw(
        size.w as u32,
        size.h as u32,
        renderer.map_texture(&mapping)?.to_vec(),
    )
    .ok_or_else(|| anyhow::anyhow!("Invalid GPU readback size"))?;
    if flipped {
        image::imageops::flip_vertical_in_place(&mut image);
    }
    Ok(image)
}

struct Pending {
    output: Output,
    size: smithay::utils::Size<i32, smithay::utils::Physical>,
    download: crate::readback::Download,
    requests: Vec<rix_ipc::ScreenshotCommand>,
}
impl Writer {
    pub(crate) fn submit<E: RenderElement<GlesRenderer>>(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        elements: &[E],
        requests: Vec<rix_ipc::ScreenshotCommand>,
    ) {
        let result = (|| -> anyhow::Result<_> {
            anyhow::ensure!(
                self.downloads.len() < 2 && requests.len() <= 16,
                "Screenshot queue is busy"
            );
            let size = output.current_transform().transform_size(
                output
                    .current_mode()
                    .ok_or_else(|| anyhow::anyhow!("Output has no mode"))?
                    .size,
            );
            let download = self.targets.entry(output.name()).or_default().submit(
                renderer,
                size,
                output.current_scale().fractional_scale(),
                Transform::Flipped180,
                Fourcc::Abgr8888,
                elements,
                [0., 0., 0., 1.],
            )?;
            Ok((size, download))
        })();
        match result {
            Ok((size, download)) => self.downloads.push(Pending {
                output: output.clone(),
                size,
                download,
                requests,
            }),
            Err(error) => {
                for request in requests {
                    let _ = request.responder.send(Err(error.to_string()));
                }
            }
        }
    }
    pub(crate) fn cancel(&mut self, reason: &str) {
        for pending in self.downloads.drain(..) {
            for request in pending.requests {
                let _ = request.responder.send(Err(reason.into()));
            }
        }
    }
    pub(crate) fn poll(&mut self, renderer: &mut GlesRenderer, outputs: &[Output]) {
        self.targets
            .retain(|name, _| outputs.iter().any(|o| o.name() == *name));
        for pending in std::mem::take(&mut self.downloads) {
            if !outputs.contains(&pending.output)
                || pending.output.current_mode().is_none_or(|m| {
                    pending.output.current_transform().transform_size(m.size) != pending.size
                })
            {
                for request in pending.requests {
                    let _ = request
                        .responder
                        .send(Err("Display changed during screenshot".into()));
                }
                continue;
            }
            match pending.download.with_pixels(renderer, |pixels, flipped| {
                image::RgbaImage::from_raw(
                    pending.size.w as u32,
                    pending.size.h as u32,
                    pixels.to_vec(),
                )
                .map(|pixels| (pixels, flipped))
            }) {
                Ok(None) => self.downloads.push(pending),
                Ok(Some(Some((pixels, flipped)))) => self.save(pixels, flipped, pending.requests),
                Ok(Some(None)) => {
                    for request in pending.requests {
                        let _ = request
                            .responder
                            .send(Err("Invalid GPU screenshot layout".into()));
                    }
                }
                Err(error) => {
                    for request in pending.requests {
                        let _ = request.responder.send(Err(error.to_string()));
                    }
                }
            }
        }
    }
}
