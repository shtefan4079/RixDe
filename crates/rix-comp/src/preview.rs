use crate::state::RixState;
use smithay::backend::{
    allocator::Fourcc,
    renderer::{
        element::{surface::WaylandSurfaceRenderElement, AsRenderElements},
        gles::GlesRenderer,
    },
};
use smithay::utils::{Physical, Size, Transform};
type Reply = tokio::sync::oneshot::Sender<Result<rix_ipc::IpcResponse, String>>;
struct Job {
    id: u64,
    size: Size<i32, Physical>,
    download: crate::readback::Download,
    reply: Reply,
}
#[derive(Default)]
pub(crate) struct Queue {
    targets: std::collections::HashMap<u64, crate::readback::Target>,
    jobs: Vec<Job>,
}
impl Queue {
    pub(crate) fn cancel(&mut self, reason: &str) {
        for job in self.jobs.drain(..) {
            let _ = job.reply.send(Err(reason.into()));
        }
    }
    pub(crate) fn submit(
        &mut self,
        state: &RixState,
        renderer: &mut GlesRenderer,
        id: u64,
        width: u32,
        height: u32,
        reply: Reply,
    ) {
        let result = (|| -> anyhow::Result<_> {
            anyhow::ensure!(self.jobs.len() < 4, "Preview queue is busy");
            let window = state
                .windows
                .entries
                .iter()
                .find(|(known, _)| *known == id)
                .map(|(_, window)| window)
                .ok_or_else(|| anyhow::anyhow!("Window no longer exists"))?;
            anyhow::ensure!(
                !state.lock.active && !crate::capture_policy::window_excluded(window),
                "Window is excluded from capture"
            );
            let geometry = window.geometry();
            anyhow::ensure!(
                geometry.size.w > 0 && geometry.size.h > 0,
                "Window has no buffer"
            );
            let scale = (width.clamp(64, 512) as f64 / geometry.size.w as f64)
                .min(height.clamp(48, 384) as f64 / geometry.size.h as f64)
                .min(1.);
            let size = geometry.size.to_physical_precise_round(scale);
            let filter = crate::capture_policy::CaptureFilter::for_window(window);
            let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = window
                .render_elements(
                    renderer,
                    (smithay::utils::Point::from((0, 0)) - geometry.loc)
                        .to_physical_precise_round(scale),
                    scale.into(),
                    1.,
                )
                .into_iter()
                .filter(|e| filter.allows(e))
                .collect();
            anyhow::ensure!(!elements.is_empty(), "Window has no renderable buffer");
            let download = self.targets.entry(id).or_default().submit(
                renderer,
                size,
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                &elements,
                [0., 0., 0., 0.],
            )?;
            Ok((size, download))
        })();
        match result {
            Ok((size, download)) => self.jobs.push(Job {
                id,
                size,
                download,
                reply,
            }),
            Err(error) => {
                let _ = reply.send(Err(error.to_string()));
            }
        }
    }
    pub(crate) fn poll(&mut self, state: &RixState, renderer: &mut GlesRenderer) {
        self.targets
            .retain(|id, _| state.windows.entries.iter().any(|(known, _)| known == id));
        let jobs = std::mem::take(&mut self.jobs);
        for job in jobs {
            if job.reply.is_closed() {
                continue;
            }
            if state.lock.active
                || !state
                    .windows
                    .entries
                    .iter()
                    .any(|(id, w)| *id == job.id && !crate::capture_policy::window_excluded(w))
            {
                let _ = job.reply.send(Err("Window is no longer capturable".into()));
                continue;
            }
            match job.download.with_pixels(renderer, |pixels, _| {
                rix_ipc::IpcResponse::WindowPreview {
                    id: job.id,
                    width: job.size.w as u32,
                    height: job.size.h as u32,
                    rgba: pixels.to_vec(),
                }
            }) {
                Ok(None) => self.jobs.push(job),
                Ok(Some(response)) => {
                    let _ = job.reply.send(Ok(response));
                }
                Err(error) => {
                    let _ = job.reply.send(Err(error.to_string()));
                }
            }
        }
    }
}
impl RixState {
    pub(crate) fn poll_previews(&mut self, renderer: &mut GlesRenderer) {
        let mut queue = std::mem::take(&mut self.previews);
        queue.poll(self, renderer);
        self.previews = queue;
    }
}
