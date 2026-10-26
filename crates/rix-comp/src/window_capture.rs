use crate::state::{ClientState, RixState};
use smithay::wayland::shm::with_buffer_contents_mut;
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{
                surface::WaylandSurfaceRenderElement, texture::TextureRenderElement,
                AsRenderElements,
            },
            gles::{GlesRenderer, GlesTexture},
        },
    },
    desktop::Window,
    utils::{Physical, Size, Transform},
    wayland::foreign_toplevel_list::{
        ForeignToplevelHandle, ForeignToplevelListHandler, ForeignToplevelListState,
    },
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use wayland_protocols::ext::{
    image_capture_source::v1::server::{
        ext_foreign_toplevel_image_capture_source_manager_v1::{
            self, ExtForeignToplevelImageCaptureSourceManagerV1,
        },
        ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    },
    image_copy_capture::v1::server::{
        ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
        ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
        ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
    },
};
use wayland_server::{
    protocol::{wl_buffer::WlBuffer, wl_output, wl_shm},
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

const MAX_CAPTURE_PIXELS: i64 = 16_777_216;
const MAX_PENDING_FRAMES: usize = 32;

#[derive(Default)]
pub struct WindowCaptureState {
    pub toplevels: Option<ForeignToplevelListState>,
    handles: HashMap<u64, ForeignToplevelHandle>,
    sessions: HashMap<u64, Vec<ExtImageCopyCaptureSessionV1>>,
    pending: Vec<PendingFrame>,
    flights: Vec<Flight>,
    targets: HashMap<(u64, bool), crate::readback::Target>,
    last_frame: Option<std::time::Instant>,
}

struct CaptureSourceData {
    window_id: u64,
}

struct CaptureSessionState {
    size: Mutex<(u32, u32)>,
    frame_created: AtomicBool,
    stopped: AtomicBool,
}

struct CaptureSessionData {
    window_id: u64,
    paint_cursor: bool,
    state: Arc<CaptureSessionState>,
}

struct FrameData {
    window_id: u64,
    paint_cursor: bool,
    session: ExtImageCopyCaptureSessionV1,
    session_state: Arc<CaptureSessionState>,
    buffer: Mutex<Option<WlBuffer>>,
    captured: AtomicBool,
}

impl Drop for FrameData {
    fn drop(&mut self) {
        self.session_state
            .frame_created
            .store(false, Ordering::Release);
    }
}

struct PendingFrame {
    frame: ExtImageCopyCaptureFrameV1,
    buffer: WlBuffer,
    window_id: u64,
    session: ExtImageCopyCaptureSessionV1,
    session_state: Arc<CaptureSessionState>,
    paint_cursor: bool,
}

struct Flight {
    pending: PendingFrame,
    download: crate::readback::Download,
    size: Size<i32, Physical>,
    time: std::time::Duration,
}

smithay::backend::renderer::element::render_elements! {
    WindowCaptureElement<=GlesRenderer>;
    Surface=WaylandSurfaceRenderElement<GlesRenderer>,
    Cursor=TextureRenderElement<GlesTexture>,
}

enum CaptureCursor {
    Surfaces(Vec<WaylandSurfaceRenderElement<GlesRenderer>>),
    Texture(TextureRenderElement<GlesTexture>),
}

impl WindowCaptureState {
    pub fn new<D: ForeignToplevelListHandler>(dh: &DisplayHandle) -> Self {
        Self {
            toplevels: Some(ForeignToplevelListState::new_with_filter::<D>(
                dh,
                capture_client,
            )),
            ..Default::default()
        }
    }

    pub fn announce(&mut self, id: u64, title: &str, app_id: &str) {
        if let Some(handle) = self.handles.get(&id) {
            handle.send_title(title);
            handle.send_app_id(app_id);
            handle.send_done();
            return;
        }
        let Some(toplevels) = self.toplevels.as_mut() else {
            return;
        };
        let handle = toplevels.new_toplevel::<RixState>(title, app_id);
        handle.user_data().insert_if_missing(|| id);
        self.handles.insert(id, handle);
    }

    pub fn close_window(&mut self, id: u64) {
        if let Some(handle) = self.handles.remove(&id) {
            if let Some(toplevels) = self.toplevels.as_mut() {
                toplevels.remove_toplevel(&handle);
            } else {
                handle.send_closed();
            }
        }
        self.stop_window(id);
    }

    pub(crate) fn stop_window(&mut self, id: u64) {
        if let Some(sessions) = self.sessions.remove(&id) {
            for session in sessions.into_iter().filter(Resource::is_alive) {
                if let Some(data) = session.data::<CaptureSessionData>() {
                    if data.state.stopped.swap(true, Ordering::AcqRel) {
                        continue;
                    }
                }
                session.stopped();
            }
        }
    }

    pub(crate) fn invalidate(&mut self) {
        self.pending
            .extend(self.flights.drain(..).map(|flight| flight.pending));
    }

    pub(crate) fn active(&self) -> bool {
        !self.flights.is_empty()
            || self.pending.iter().any(|p| p.frame.is_alive())
            || self
                .last_frame
                .is_some_and(|time| time.elapsed() < std::time::Duration::from_secs(2))
    }

    fn add_session(&mut self, id: u64, session: ExtImageCopyCaptureSessionV1) {
        self.sessions.entry(id).or_default().push(session);
    }

    fn prune_sessions(&mut self) {
        self.sessions.retain(|_, sessions| {
            sessions.retain(Resource::is_alive);
            !sessions.is_empty()
        });
        if let Some(toplevels) = self.toplevels.as_mut() {
            toplevels.cleanup_closed_handles();
        }
    }
}

pub fn install(display: &DisplayHandle) {
    display.create_global::<RixState, ExtImageCopyCaptureManagerV1, _>(1, ());
    display.create_global::<RixState, ExtForeignToplevelImageCaptureSourceManagerV1, _>(1, ());
}

pub fn render(
    state: &mut RixState,
    renderer: &mut GlesRenderer,
    cursor_renderer: &mut crate::backend::CursorRenderer,
) {
    state.window_capture.prune_sessions();
    poll(state, renderer);
    state
        .window_capture
        .targets
        .retain(|(id, _), _| state.windows.entries.iter().any(|(known, _)| known == id));
    let pending = std::mem::take(&mut state.window_capture.pending);
    let time: std::time::Duration = smithay::utils::Clock::<smithay::utils::Monotonic>::new()
        .now()
        .into();

    let mut submitted = 0;
    for pending in pending {
        if state.window_capture.flights.len() >= 4 || submitted >= 2 {
            state.window_capture.pending.push(pending);
            continue;
        }
        if !pending.frame.is_alive() || !pending.buffer.is_alive() {
            continue;
        }
        if pending.session_state.stopped.load(Ordering::Acquire) || !pending.session.is_alive() {
            pending
                .frame
                .failed(ext_image_copy_capture_frame_v1::FailureReason::Stopped);
            continue;
        }
        if state.lock.active {
            pending.session_state.stopped.store(true, Ordering::Release);
            pending.session.stopped();
            pending
                .frame
                .failed(ext_image_copy_capture_frame_v1::FailureReason::Stopped);
            continue;
        }
        let Some(window) = state
            .windows
            .entries
            .iter()
            .find(|(id, _)| *id == pending.window_id)
            .map(|(_, window)| window)
        else {
            pending.session_state.stopped.store(true, Ordering::Release);
            pending.session.stopped();
            pending
                .frame
                .failed(ext_image_copy_capture_frame_v1::FailureReason::Stopped);
            continue;
        };
        let Some((scale, size)) = capture_geometry(state, window) else {
            if crate::capture_policy::window_excluded(window) {
                if !pending.session_state.stopped.swap(true, Ordering::AcqRel) {
                    pending.session.stopped();
                }
                pending
                    .frame
                    .failed(ext_image_copy_capture_frame_v1::FailureReason::Stopped);
                continue;
            }
            pending
                .frame
                .failed(ext_image_copy_capture_frame_v1::FailureReason::Unknown);
            continue;
        };
        let dimensions = (size.w as u32, size.h as u32);
        let mut session_size = pending.session_state.size.lock().unwrap();
        if *session_size != dimensions {
            *session_size = dimensions;

            pending.session.shm_format(wl_shm::Format::Xrgb8888);
            pending.session.buffer_size(dimensions.0, dimensions.1);
            pending.session.done();
            pending
                .frame
                .failed(ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints);
            continue;
        }
        drop(session_size);

        let cursor = pending
            .paint_cursor
            .then(|| capture_cursor(state, renderer, cursor_renderer, window, scale))
            .flatten();
        let target = state
            .window_capture
            .targets
            .entry((pending.window_id, pending.paint_cursor))
            .or_default();
        match capture_window(renderer, target, window, scale, size, cursor) {
            Ok(download) => {
                state.window_capture.flights.push(Flight {
                    pending,
                    download,
                    size,
                    time,
                });
                submitted += 1;
            }
            Err(error) => {
                tracing::debug!(%error, window_id = pending.window_id, "Toplevel frame submission failed");
                pending
                    .frame
                    .failed(ext_image_copy_capture_frame_v1::FailureReason::Unknown);
            }
        }
    }
}

fn poll(state: &mut RixState, renderer: &mut GlesRenderer) {
    let flights = std::mem::take(&mut state.window_capture.flights);
    for flight in flights {
        let pending = &flight.pending;
        if !pending.frame.is_alive() || !pending.buffer.is_alive() {
            continue;
        }
        let window = state
            .windows
            .entries
            .iter()
            .find(|(id, _)| *id == pending.window_id)
            .map(|(_, window)| window);
        if state.lock.active
            || pending.session_state.stopped.load(Ordering::Acquire)
            || !pending.session.is_alive()
            || window.is_none_or(crate::capture_policy::window_excluded)
        {
            if !pending.session_state.stopped.swap(true, Ordering::AcqRel) {
                pending.session.stopped();
            }
            pending
                .frame
                .failed(ext_image_copy_capture_frame_v1::FailureReason::Stopped);
            continue;
        }
        if capture_geometry(state, window.unwrap()).is_none_or(|(_, size)| size != flight.size) {
            pending
                .frame
                .failed(ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints);
            continue;
        }
        let completed = flight.download.with_pixels(renderer, |pixels, _flipped| {
            with_buffer_contents_mut(&pending.buffer, |pointer, len, buffer| {
                copy_to_shm(pointer, len, buffer, flight.size, pixels)
            })
            .unwrap_or(false)
        });
        match completed {
            Ok(None) => state.window_capture.flights.push(flight),
            Ok(Some(true)) => {
                pending.frame.transform(wl_output::Transform::Normal);
                pending.frame.damage(0, 0, flight.size.w, flight.size.h);
                pending.frame.presentation_time(
                    (flight.time.as_secs() >> 32) as u32,
                    flight.time.as_secs() as u32,
                    flight.time.subsec_nanos(),
                );
                pending.frame.ready();
                pending.buffer.release();
                state.window_capture.last_frame = Some(std::time::Instant::now());
            }
            Ok(Some(false)) => pending
                .frame
                .failed(ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints),
            Err(error) => {
                tracing::warn!(%error, "Asynchronous window download failed");
                pending
                    .frame
                    .failed(ext_image_copy_capture_frame_v1::FailureReason::Unknown);
            }
        }
    }
}

fn capture_client(client: &Client) -> bool {
    client
        .get_data::<ClientState>()
        .is_some_and(|data| data.allow_capture)
}

fn capture_geometry(state: &RixState, window: &Window) -> Option<(f64, Size<i32, Physical>)> {
    if crate::capture_policy::window_excluded(window) {
        return None;
    }
    let geometry = window.geometry();
    if geometry.size.w <= 0 || geometry.size.h <= 0 {
        return None;
    }
    let outputs = state.space.outputs_for_element(window);
    let output = outputs.first().or_else(|| state.space.outputs().next())?;
    let scale = output.current_scale().fractional_scale();
    let size = geometry.size.to_physical_precise_round(scale);
    let pixels = i64::from(size.w) * i64::from(size.h);
    (size.w > 0 && size.h > 0 && pixels <= MAX_CAPTURE_PIXELS).then_some((scale, size))
}

fn capture_window(
    renderer: &mut GlesRenderer,
    target: &mut crate::readback::Target,
    window: &Window,
    scale: f64,
    size: Size<i32, Physical>,
    cursor: Option<CaptureCursor>,
) -> anyhow::Result<crate::readback::Download> {
    anyhow::ensure!(
        !crate::capture_policy::window_excluded(window),
        "Toplevel is excluded from capture"
    );
    let geometry = window.geometry();
    let mut elements = Vec::new();
    match cursor {
        Some(CaptureCursor::Surfaces(surfaces)) => {
            elements.extend(surfaces.into_iter().map(WindowCaptureElement::Surface));
        }
        Some(CaptureCursor::Texture(texture)) => {
            elements.push(WindowCaptureElement::Cursor(texture));
        }
        None => {}
    }
    let filter = crate::capture_policy::CaptureFilter::for_window(window);
    elements.extend(
        window
            .render_elements(
                renderer,
                (smithay::utils::Point::from((0, 0)) - geometry.loc)
                    .to_physical_precise_round(scale),
                scale.into(),
                1.,
            )
            .into_iter()
            .filter(|element| filter.allows(element))
            .map(WindowCaptureElement::Surface),
    );
    anyhow::ensure!(!elements.is_empty(), "Toplevel has no renderable surfaces");
    target.submit(
        renderer,
        size,
        scale,
        Transform::Normal,
        Fourcc::Argb8888,
        &elements,
        [0., 0., 0., 1.],
    )
}

fn capture_cursor(
    state: &RixState,
    renderer: &mut GlesRenderer,
    cursor_renderer: &mut crate::backend::CursorRenderer,
    window: &Window,
    scale: f64,
) -> Option<CaptureCursor> {
    let pointer = state.seat.get_pointer()?;
    let location = pointer.current_location();
    let geometry = state.space.element_geometry(window)?;
    if !geometry.to_f64().contains(location) {
        return None;
    }
    let local = location - geometry.loc.to_f64();
    let status = state.effective_cursor_status();
    if let Some(mut surfaces) =
        crate::backend::cursor_surface_elements(renderer, &status, local, scale)
    {
        let filter = crate::capture_policy::CaptureFilter::for_cursor(&status);
        surfaces.retain(|element| filter.allows(element));
        return Some(CaptureCursor::Surfaces(surfaces));
    }
    cursor_renderer
        .element(renderer, &status, local, scale, state.start_time.elapsed())
        .map(CaptureCursor::Texture)
}

fn copy_to_shm(
    pointer: *mut u8,
    len: usize,
    buffer: smithay::wayland::shm::BufferData,
    size: Size<i32, Physical>,
    pixels: &[u8],
) -> bool {
    if buffer.format != wl_shm::Format::Xrgb8888
        || buffer.width != size.w
        || buffer.height != size.h
        || buffer.offset < 0
        || buffer.stride < size.w.saturating_mul(4)
        || pixels.len() < size.w as usize * size.h as usize * 4
    {
        return false;
    }
    let Some(last_row) = (buffer.height as usize - 1).checked_mul(buffer.stride as usize) else {
        return false;
    };
    let Some(end) = (buffer.offset as usize)
        .checked_add(last_row)
        .and_then(|offset| offset.checked_add(size.w as usize * 4))
    else {
        return false;
    };
    if end > len {
        return false;
    }
    for row in 0..size.h as usize {
        let source = row * size.w as usize * 4;
        let destination = buffer.offset as usize + row * buffer.stride as usize;

        unsafe {
            std::ptr::copy_nonoverlapping(
                pixels.as_ptr().add(source),
                pointer.add(destination),
                size.w as usize * 4,
            );
        }
    }
    true
}

impl GlobalDispatch<ExtImageCopyCaptureManagerV1, ()> for RixState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ExtImageCopyCaptureManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }

    fn can_view(client: Client, _: &()) -> bool {
        capture_client(&client)
    }
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for RixState {
    fn request(
        state: &mut Self,
        client: &Client,
        resource: &ExtImageCopyCaptureManagerV1,
        request: ext_image_copy_capture_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        let ext_image_copy_capture_manager_v1::Request::CreateSession {
            session,
            source,
            options,
        } = request
        else {
            return;
        };
        let Ok(options) = options.into_result() else {
            resource.post_error(
                ext_image_copy_capture_manager_v1::Error::InvalidOption,
                "Unknown capture option",
            );
            return;
        };
        let paint_cursor =
            options.contains(ext_image_copy_capture_manager_v1::Options::PaintCursors);
        let window_id = source
            .data::<CaptureSourceData>()
            .map(|source| source.window_id)
            .unwrap_or_default();
        let size = state
            .windows
            .entries
            .iter()
            .find(|(id, _)| *id == window_id)
            .and_then(|(_, window)| capture_geometry(state, window).map(|(_, size)| size));
        let available = capture_client(client) && !state.lock.active && size.is_some();
        let (width, height) = size.map_or((0, 0), |size| (size.w as u32, size.h as u32));
        let state_data = Arc::new(CaptureSessionState {
            size: Mutex::new((width, height)),
            frame_created: AtomicBool::new(false),
            stopped: AtomicBool::new(!available),
        });
        let resource = init.init(
            session,
            CaptureSessionData {
                window_id,
                paint_cursor,
                state: state_data,
            },
        );
        if available {
            resource.shm_format(wl_shm::Format::Xrgb8888);
            resource.buffer_size(width, height);
            resource.done();
        } else {
            resource.stopped();
        }
        state.window_capture.add_session(window_id, resource);
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, CaptureSessionData> for RixState {
    fn request(
        _: &mut Self,
        _: &Client,
        resource: &ExtImageCopyCaptureSessionV1,
        request: ext_image_copy_capture_session_v1::Request,
        data: &CaptureSessionData,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_image_copy_capture_session_v1::Request::CreateFrame { frame } => {
                if data.state.frame_created.swap(true, Ordering::AcqRel) {
                    resource.post_error(
                        ext_image_copy_capture_session_v1::Error::DuplicateFrame,
                        "A capture frame is already pending for this session",
                    );
                    return;
                }
                init.init(
                    frame,
                    FrameData {
                        window_id: data.window_id,
                        paint_cursor: data.paint_cursor,
                        session: resource.clone(),
                        session_state: data.state.clone(),
                        buffer: Mutex::new(None),
                        captured: AtomicBool::new(false),
                    },
                );
            }
            ext_image_copy_capture_session_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, FrameData> for RixState {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &ExtImageCopyCaptureFrameV1,
        request: ext_image_copy_capture_frame_v1::Request,
        data: &FrameData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_image_copy_capture_frame_v1::Request::AttachBuffer { buffer } => {
                if data.captured.load(Ordering::Acquire) {
                    resource.post_error(
                        ext_image_copy_capture_frame_v1::Error::AlreadyCaptured,
                        "Cannot replace the buffer after requesting capture",
                    );
                    return;
                }
                *data.buffer.lock().unwrap() = Some(buffer);
            }
            ext_image_copy_capture_frame_v1::Request::DamageBuffer {
                x,
                y,
                width,
                height,
            } => {
                if data.captured.load(Ordering::Acquire) {
                    resource.post_error(
                        ext_image_copy_capture_frame_v1::Error::AlreadyCaptured,
                        "Cannot change damage after requesting capture",
                    );
                } else if x < 0 || y < 0 || width <= 0 || height <= 0 {
                    resource.post_error(
                        ext_image_copy_capture_frame_v1::Error::InvalidBufferDamage,
                        "Damage requires nonnegative coordinates and positive dimensions",
                    );
                }

            }
            ext_image_copy_capture_frame_v1::Request::Capture => {
                let Some(buffer) = data.buffer.lock().unwrap().clone() else {
                    resource.post_error(
                        ext_image_copy_capture_frame_v1::Error::NoBuffer,
                        "Attach a SHM buffer before requesting capture",
                    );
                    return;
                };
                if data.captured.swap(true, Ordering::AcqRel) {
                    resource.post_error(
                        ext_image_copy_capture_frame_v1::Error::AlreadyCaptured,
                        "A frame can only be captured once",
                    );
                    return;
                }
                if state.window_capture.pending.len() + state.window_capture.flights.len()
                    >= MAX_PENDING_FRAMES
                {
                    resource.failed(ext_image_copy_capture_frame_v1::FailureReason::Unknown);
                    return;
                }
                state.window_capture.pending.push(PendingFrame {
                    frame: resource.clone(),
                    buffer,
                    window_id: data.window_id,
                    session: data.session.clone(),
                    session_state: data.session_state.clone(),
                    paint_cursor: data.paint_cursor,
                });
            }
            ext_image_copy_capture_frame_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl GlobalDispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for RixState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ExtForeignToplevelImageCaptureSourceManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }

    fn can_view(client: Client, _: &()) -> bool {
        capture_client(&client)
    }
}

impl Dispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for RixState {
    fn request(
        _state: &mut Self,
        client: &Client,
        _resource: &ExtForeignToplevelImageCaptureSourceManagerV1,
        request: ext_foreign_toplevel_image_capture_source_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let ext_foreign_toplevel_image_capture_source_manager_v1::Request::CreateSource {
            source,
            toplevel_handle,
        } = request
        {
            let Some(handle) = ForeignToplevelHandle::from_resource(&toplevel_handle) else {
                return;
            };
            if toplevel_handle.client().as_ref() != Some(client) {
                return;
            }
            let Some(window_id) = handle.user_data().get::<u64>().copied() else {
                return;
            };
            init.init(source, CaptureSourceData { window_id });
        }
    }
}

impl Dispatch<ExtImageCaptureSourceV1, CaptureSourceData> for RixState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ExtImageCaptureSourceV1,
        _: <ExtImageCaptureSourceV1 as Resource>::Request,
        _: &CaptureSourceData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_window_frames_into_offset_padded_shm() {
        let size: Size<i32, Physical> = (2, 2).into();
        let pixels = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let mut memory = vec![0xaa; 4 + 12 * 2];
        let buffer = smithay::wayland::shm::BufferData {
            offset: 4,
            width: 2,
            height: 2,
            stride: 12,
            format: wl_shm::Format::Xrgb8888,
        };

        assert!(copy_to_shm(
            memory.as_mut_ptr(),
            memory.len(),
            buffer,
            size,
            &pixels,
        ));
        assert_eq!(&memory[4..12], &pixels[..8]);
        assert_eq!(&memory[16..24], &pixels[8..]);
        assert_eq!(&memory[12..16], &[0xaa; 4]);
    }

    #[test]
    fn rejects_window_buffers_that_do_not_match_the_session_constraints() {
        let size: Size<i32, Physical> = (2, 2).into();
        let buffer = smithay::wayland::shm::BufferData {
            offset: 0,
            width: 2,
            height: 2,
            stride: 8,
            format: wl_shm::Format::Xrgb8888,
        };
        let mut memory = vec![0; 16];

        assert!(!copy_to_shm(
            memory.as_mut_ptr(),
            memory.len(),
            smithay::wayland::shm::BufferData { width: 1, ..buffer },
            size,
            &[0; 16],
        ));
        assert!(!copy_to_shm(
            memory.as_mut_ptr(),
            memory.len(),
            smithay::wayland::shm::BufferData {
                stride: 7,
                ..buffer
            },
            size,
            &[0; 16],
        ));
        assert!(!copy_to_shm(
            memory.as_mut_ptr(),
            memory.len(),
            smithay::wayland::shm::BufferData {
                format: wl_shm::Format::Argb8888,
                ..buffer
            },
            size,
            &[0; 16],
        ));
        assert!(!copy_to_shm(
            memory.as_mut_ptr(),
            memory.len(),
            smithay::wayland::shm::BufferData {
                offset: 1,
                ..buffer
            },
            size,
            &[0; 16],
        ));
    }
}
