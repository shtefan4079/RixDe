use smithay::{
    backend::{
        allocator::Fourcc,
        egl::fence::EGLFence,
        renderer::{
            damage::OutputDamageTracker,
            element::RenderElement,
            gles::{Capability, GlesMapping, GlesRenderer, GlesTexture},
            Bind, ExportMem, Offscreen, TextureMapping,
        },
    },
    utils::{Physical, Rectangle, Size, Transform},
};
use std::{
    sync::{Mutex, Once, OnceLock},
    time::{Duration, Instant},
};

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(2);

const SYNCHRONOUS_MAP_INTERVAL: Duration = Duration::from_millis(4);
static SYNCHRONOUS_MAP_CLOCK: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
static SYNCHRONOUS_WARNING: Once = Once::new();

fn uses_synchronous_readback(capabilities: &[Capability]) -> bool {
    !capabilities.contains(&Capability::ExportFence)
}

fn synchronous_map_ready(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|last| now.saturating_duration_since(last) >= SYNCHRONOUS_MAP_INTERVAL)
}

fn synchronous_map_expired(submitted: Instant, now: Instant) -> bool {
    now.saturating_duration_since(submitted) >= DOWNLOAD_TIMEOUT
}

#[derive(Default)]
pub(crate) struct Target {
    texture: Option<(Size<i32, Physical>, Fourcc, GlesTexture)>,
}

pub(crate) struct Download {
    mapping: GlesMapping,

    fence: Option<EGLFence>,
    submitted: Instant,
}

impl Target {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn submit<E: RenderElement<GlesRenderer>>(
        &mut self,
        renderer: &mut GlesRenderer,
        size: Size<i32, Physical>,
        scale: f64,
        transform: Transform,
        format: Fourcc,
        elements: &[E],
        clear: [f32; 4],
    ) -> anyhow::Result<Download> {
        let synchronous = uses_synchronous_readback(renderer.capabilities());
        if synchronous {
            SYNCHRONOUS_WARNING.call_once(|| {
                tracing::warn!(
                    "GPU capture fences are unavailable; using throttled synchronous readback"
                );
            });
        }
        anyhow::ensure!(
            size.w > 0 && size.h > 0 && i64::from(size.w) * i64::from(size.h) <= 33_177_600,
            "Capture dimensions exceed the download limit"
        );
        if self
            .texture
            .as_ref()
            .is_none_or(|(old_size, old_format, _)| *old_size != size || *old_format != format)
        {
            self.texture = Some((
                size,
                format,
                renderer.create_buffer(format, (size.w, size.h).into())?,
            ));
        }
        let mut framebuffer = renderer.bind(&mut self.texture.as_mut().unwrap().2)?;
        OutputDamageTracker::new(size, scale, transform).render_output(
            renderer,
            &mut framebuffer,
            0,
            elements,
            clear,
        )?;
        let mapping = renderer.copy_framebuffer(
            &framebuffer,
            Rectangle::from_size((size.w, size.h).into()),
            format,
        )?;

        let fence = if synchronous {
            None
        } else {
            match EGLFence::create(renderer.egl_context().display()) {
                Ok(fence) => Some(fence),
                Err(error) => {
                    SYNCHRONOUS_WARNING.call_once(|| {
                        tracing::warn!(
                            %error,
                            "GPU capture fence creation failed; using throttled synchronous readback"
                        );
                    });
                    None
                }
            }
        };
        renderer.with_context(|gl| {

            unsafe { gl.Flush() };
        })?;
        Ok(Download {
            mapping,
            fence,
            submitted: Instant::now(),
        })
    }
}

impl Download {
    pub(crate) fn with_pixels<T>(
        &self,
        renderer: &mut GlesRenderer,
        consume: impl FnOnce(&[u8], bool) -> T,
    ) -> anyhow::Result<Option<T>> {
        if let Some(fence) = &self.fence {

            if !fence.client_wait(Some(Duration::ZERO), false)? {
                anyhow::ensure!(
                    self.submitted.elapsed() < DOWNLOAD_TIMEOUT,
                    "GPU capture download timed out"
                );
                return Ok(None);
            }
            let pixels = renderer.map_texture(&self.mapping)?;
            return Ok(Some(consume(pixels, self.mapping.flipped())));
        }

        let clock = SYNCHRONOUS_MAP_CLOCK.get_or_init(|| Mutex::new(None));
        let mut last = clock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        if !synchronous_map_ready(*last, now) {
            return Ok(None);
        }
        anyhow::ensure!(
            !synchronous_map_expired(self.submitted, now),
            "GPU capture download timed out"
        );

        let pixels = renderer.map_texture(&self.mapping)?;
        *last = Some(Instant::now());
        Ok(Some(consume(pixels, self.mapping.flipped())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_fence_capability_selects_the_readback_path() {
        assert!(uses_synchronous_readback(&[
            Capability::Fencing,
            Capability::Renderbuffer
        ]));
        assert!(!uses_synchronous_readback(&[
            Capability::Fencing,
            Capability::ExportFence
        ]));
        assert!(uses_synchronous_readback(&[]));
    }

    #[test]
    fn synchronous_fallback_maps_at_most_one_download_per_interval() {
        let now = Instant::now();
        assert!(synchronous_map_ready(None, now));
        assert!(!synchronous_map_ready(Some(now), now));
        assert!(!synchronous_map_ready(
            Some(now),
            now + SYNCHRONOUS_MAP_INTERVAL - Duration::from_millis(1)
        ));
        assert!(synchronous_map_ready(
            Some(now),
            now + SYNCHRONOUS_MAP_INTERVAL
        ));
    }

    #[test]
    fn a_blocking_map_publishes_its_stamp_only_after_completion() {

        let started = Instant::now();
        let finished = started + Duration::from_millis(12);
        assert!(!synchronous_map_ready(
            Some(finished),
            finished + SYNCHRONOUS_MAP_INTERVAL - Duration::from_millis(1)
        ));
        assert!(synchronous_map_ready(
            Some(finished),
            finished + SYNCHRONOUS_MAP_INTERVAL
        ));
    }

    #[test]
    fn a_stale_synchronous_download_degrades_to_a_timeout() {
        let submitted = Instant::now();
        assert!(!synchronous_map_expired(
            submitted,
            submitted + DOWNLOAD_TIMEOUT - Duration::from_millis(1)
        ));
        assert!(synchronous_map_expired(
            submitted,
            submitted + DOWNLOAD_TIMEOUT
        ));
    }
}
