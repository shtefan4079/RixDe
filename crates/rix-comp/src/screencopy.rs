use crate::state::{ClientState, RixState};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{damage::OutputDamageTracker, element::RenderElement, gles::GlesRenderer},
    },
    output::Output,
    utils::{Physical, Rectangle, Transform},
    wayland::shm::with_buffer_contents_mut,
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};
use wayland_server::{
    protocol::{wl_buffer::WlBuffer, wl_shm},
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
#[derive(Default)]
pub struct ManagerData {
    last: Mutex<HashMap<(String, bool), u64>>,
    frames: AtomicUsize,
}
pub struct FrameData {
    output: Option<Output>,
    region: Rectangle<i32, Physical>,
    cursor: bool,
    used: Mutex<bool>,
    manager: Arc<ManagerData>,
}
impl Drop for FrameData {
    fn drop(&mut self) {
        self.manager.frames.fetch_sub(1, Ordering::Relaxed);
    }
}
struct Pending {
    frame: ZwlrScreencopyFrameV1,
    buffer: WlBuffer,
    damage: bool,
}
struct Tracking {
    size: smithay::utils::Size<i32, Physical>,
    scale: f64,
    tracker: OutputDamageTracker,
    generation: u64,
    target: crate::readback::Target,
}
struct Flight {
    selected: Vec<Pending>,
    download: crate::readback::Download,
    size: smithay::utils::Size<i32, Physical>,
    generation: u64,
    time: Duration,
}
#[derive(Default)]
pub struct Screencopy {
    last_frame: Option<std::time::Instant>,
    published_active: bool,
    pending: Vec<Pending>,
    tracking: HashMap<(String, bool), Tracking>,
    flights: HashMap<(String, bool), Flight>,
}
pub fn install(display: &DisplayHandle) {
    display.create_global::<RixState, ZwlrScreencopyManagerV1, _>(3, ());
}
impl GlobalDispatch<ZwlrScreencopyManagerV1, ()> for RixState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, Arc::new(ManagerData::default()));
    }
    fn can_view(client: Client, _: &()) -> bool {
        client
            .get_data::<ClientState>()
            .is_some_and(|data| data.allow_capture)
    }
}
impl Dispatch<ZwlrScreencopyManagerV1, Arc<ManagerData>> for RixState {
    fn request(
        state: &mut Self,
        client: &Client,
        resource: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        data: &Arc<ManagerData>,
        display: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        let (frame, overlay, output, region) = match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => (frame, overlay_cursor, output, None),
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => (frame, overlay_cursor, output, Some((x, y, width, height))),
            _ => return,
        };
        let output = Output::from_resource(&output);
        let rect = output
            .as_ref()
            .and_then(|o| {
                let mode = o.current_mode()?;
                let size = o.current_transform().transform_size(mode.size);
                let bounds = Rectangle::from_size(size);
                if let Some((x, y, w, h)) = region {
                    if w <= 0 || h <= 0 {
                        return None;
                    }
                    let scale = o.current_scale().fractional_scale();
                    let left = (x as f64 * scale).clamp(0., size.w as f64).floor() as i32;
                    let top = (y as f64 * scale).clamp(0., size.h as f64).floor() as i32;
                    let right = ((x as f64 + w as f64) * scale)
                        .clamp(0., size.w as f64)
                        .ceil() as i32;
                    let bottom = ((y as f64 + h as f64) * scale)
                        .clamp(0., size.h as f64)
                        .ceil() as i32;
                    Some(Rectangle::new(
                        (left, top).into(),
                        ((right - left).max(0), (bottom - top).max(0)).into(),
                    ))
                } else {
                    Some(bounds)
                }
            })
            .filter(|r| {
                r.size.w > 0 && r.size.h > 0 && r.size.w as i64 * r.size.h as i64 <= 16_777_216
            });
        let count = data.frames.fetch_add(1, Ordering::Relaxed);
        let data = FrameData {
            output,
            region: rect.unwrap_or_default(),
            cursor: overlay != 0,
            used: Mutex::new(false),
            manager: data.clone(),
        };
        let frame = init.init(frame, data);
        if count >= 32 {
            resource.post_error(0u32, "Too many capture frames");
            return;
        }
        if !client
            .get_data::<ClientState>()
            .is_some_and(|d| d.allow_capture)
            || rect.is_none()
            || state.lock.active
        {
            frame.failed();
            return;
        }
        let rect = rect.unwrap();
        frame.buffer(
            wl_shm::Format::Xrgb8888,
            rect.size.w as u32,
            rect.size.h as u32,
            rect.size.w as u32 * 4,
        );
        if frame.version() >= 3 {
            frame.buffer_done();
        }
        let _ = (state, display);
    }
}
impl Dispatch<ZwlrScreencopyFrameV1, FrameData> for RixState {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &FrameData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let (buffer, damage) = match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => (buffer, false),
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => (buffer, true),
            zwlr_screencopy_frame_v1::Request::Destroy => {
                state.screencopy.pending.retain(|p| &p.frame != resource);
                return;
            }
            _ => return,
        };
        let mut used = data.used.lock().unwrap();
        if *used {
            resource.post_error(
                zwlr_screencopy_frame_v1::Error::AlreadyUsed,
                "Frame already copied",
            );
            return;
        }
        *used = true;
        if state.lock.active {
            resource.failed();
            return;
        }
        let valid =
            with_buffer_contents_mut(&buffer, |_, len, b| valid_buffer(len, b, data.region))
                .unwrap_or(false);
        if !valid {
            resource.post_error(
                zwlr_screencopy_frame_v1::Error::InvalidBuffer,
                "Buffer dimensions, stride or format do not match",
            );
            return;
        }
        if state.screencopy.pending.len()
            + state
                .screencopy
                .flights
                .values()
                .map(|f| f.selected.len())
                .sum::<usize>()
            >= 32
        {
            resource.failed();
            return;
        }
        state.screencopy.pending.push(Pending {
            frame: resource.clone(),
            buffer,
            damage,
        });
    }
}
fn valid_buffer(
    len: usize,
    b: smithay::wayland::shm::BufferData,
    region: Rectangle<i32, Physical>,
) -> bool {
    let bytes = region.size.w as i64 * 4;
    region.size.w > 0
        && region.size.h > 0
        && b.offset >= 0
        && b.width == region.size.w
        && b.height == region.size.h
        && b.stride as i64 == bytes
        && b.format == wl_shm::Format::Xrgb8888
        && b.offset as i64 + bytes * region.size.h as i64 <= len as i64
}
impl Screencopy {
    pub fn needs_frame(&self, output: &Output) -> bool {
        self.pending.iter().any(|pending| {
            pending
                .frame
                .data::<FrameData>()
                .is_some_and(|data| data.output.as_ref() == Some(output))
        })
    }

    pub fn fail_output(&mut self, output: &Output) {
        for cursor in [false, true] {
            if let Some(flight) = self.flights.remove(&(output.name(), cursor)) {
                for pending in flight.selected {
                    pending.frame.failed();
                }
            }
        }
        self.pending.retain(|pending| {
            if pending
                .frame
                .data::<FrameData>()
                .is_some_and(|data| data.output.as_ref() == Some(output))
            {
                pending.frame.failed();
                false
            } else {
                true
            }
        });
    }

    pub(crate) fn cancel_pending(&mut self) {
        for pending in self.pending.drain(..) {
            pending.frame.failed();
        }
        for (_, flight) in self.flights.drain() {
            for pending in flight.selected {
                pending.frame.failed();
            }
        }
        self.last_frame = None;
    }
    pub fn active(&self) -> bool {
        !self.flights.is_empty()
            || self.pending.iter().any(|p| p.frame.is_alive())
            || self
                .last_frame
                .is_some_and(|t| t.elapsed() < Duration::from_secs(2))
    }
    pub fn publish_status(
        &mut self,
        sender: &Option<tokio::sync::broadcast::Sender<rix_ipc::IpcEvent>>,
        window_capture_active: bool,
    ) {
        let active = self.active() || window_capture_active;
        if self.published_active != active {
            self.published_active = active;
            if let Some(sender) = sender {
                let _ = sender.send(rix_ipc::IpcEvent::CaptureChanged(active));
            }
        }
    }
    pub fn prune(&mut self, outputs: &[Output]) {
        self.flights.retain(|(name, _), flight| {
            let valid = outputs.iter().any(|o| {
                o.name() == *name
                    && o.current_mode().is_some_and(|m| {
                        o.current_transform().transform_size(m.size) == flight.size
                    })
            });
            if !valid {
                for pending in &flight.selected {
                    pending.frame.failed();
                }
            }
            valid
        });
        self.pending.retain(|p| {
            let data = p.frame.data::<FrameData>().unwrap();
            if !p.frame.is_alive() || !p.buffer.is_alive() {
                return false;
            }
            if data.output.as_ref().is_none_or(|o| !outputs.contains(o)) {
                p.frame.failed();
                return false;
            }
            true
        });
        self.tracking
            .retain(|(name, _), _| outputs.iter().any(|o| o.name() == *name));
    }

    pub(crate) fn invalidate(&mut self) {
        for (_, flight) in self.flights.drain() {
            self.pending.extend(flight.selected);
        }
        for track in self.tracking.values_mut() {
            track.generation = track.generation.wrapping_add(1);
        }
    }

    pub(crate) fn poll(&mut self, renderer: &mut GlesRenderer) {
        self.flights.retain(|key, flight| {
            flight
                .selected
                .retain(|p| p.frame.is_alive() && p.buffer.is_alive());
            if flight.selected.is_empty() {
                return false;
            }
            let completed = flight.download.with_pixels(renderer, |pixels, flipped| {
                for pending in &flight.selected {
                    let data = pending.frame.data::<FrameData>().unwrap();
                    let copied =
                        with_buffer_contents_mut(&pending.buffer, |pointer, len, buffer| {
                            copy_pixels(
                                pointer,
                                len,
                                buffer,
                                data.region,
                                flight.size,
                                pixels,
                                flipped,
                            )
                        })
                        .unwrap_or(false);
                    if !copied {
                        pending.frame.failed();
                        continue;
                    }
                    data.manager
                        .last
                        .lock()
                        .unwrap()
                        .insert(key.clone(), flight.generation);
                    pending
                        .frame
                        .flags(zwlr_screencopy_frame_v1::Flags::empty());
                    if pending.damage {
                        pending.frame.damage(
                            0,
                            0,
                            data.region.size.w as u32,
                            data.region.size.h as u32,
                        );
                    }
                    pending.frame.ready(
                        (flight.time.as_secs() >> 32) as u32,
                        flight.time.as_secs() as u32,
                        flight.time.subsec_nanos(),
                    );
                    pending.buffer.release();
                    self.last_frame = Some(std::time::Instant::now());
                }
            });
            match completed {
                Ok(None) => true,
                Ok(Some(())) => false,
                Err(error) => {
                    tracing::warn!(%error, "Asynchronous screen download failed");
                    for pending in &flight.selected {
                        pending.frame.failed();
                    }
                    false
                }
            }
        });
    }

    pub fn render<E: RenderElement<GlesRenderer>>(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        all: &[E],
        without_cursor: &[&E],
        _time: Duration,
    ) {
        self.poll(renderer);
        let time: Duration = smithay::utils::Clock::<smithay::utils::Monotonic>::new()
            .now()
            .into();
        for cursor in [false, true] {
            let key = (output.name(), cursor);
            if self.flights.contains_key(&key) {
                continue;
            }
            let relevant = |p: &Pending| {
                p.frame
                    .data::<FrameData>()
                    .is_some_and(|d| d.output.as_ref() == Some(output) && d.cursor == cursor)
            };
            if !self.pending.iter().any(relevant) {
                continue;
            }
            let Some(mode) = output.current_mode() else {
                continue;
            };
            let size = output.current_transform().transform_size(mode.size);
            let elements: Vec<&E> = if cursor {
                all.iter().collect()
            } else {
                without_cursor.to_vec()
            };
            let track = self
                .tracking
                .entry(key.clone())
                .or_insert_with(|| Tracking {
                    size,
                    scale: output.current_scale().fractional_scale(),
                    tracker: OutputDamageTracker::new(
                        size,
                        output.current_scale().fractional_scale(),
                        Transform::Normal,
                    ),
                    generation: 0,
                    target: Default::default(),
                });
            if track.size != size || track.scale != output.current_scale().fractional_scale() {
                track.size = size;
                track.scale = output.current_scale().fractional_scale();
                track.tracker = OutputDamageTracker::new(size, track.scale, Transform::Normal);
            }
            if track
                .tracker
                .damage_output(1, &elements)
                .map_or(true, |(damage, _)| damage.is_some())
            {
                track.generation = track.generation.wrapping_add(1);
            }
            let generation = track.generation;
            let mut selected = Vec::new();
            let mut rest = Vec::new();
            for pending in self.pending.drain(..) {
                let ready = relevant(&pending)
                    && (!pending.damage
                        || pending
                            .frame
                            .data::<FrameData>()
                            .unwrap()
                            .manager
                            .last
                            .lock()
                            .unwrap()
                            .get(&key)
                            != Some(&generation));
                if ready {
                    selected.push(pending);
                } else {
                    rest.push(pending);
                }
            }
            self.pending = rest;
            if selected.is_empty() {
                continue;
            }
            match track.target.submit(
                renderer,
                size,
                track.scale,
                Transform::Flipped180,
                Fourcc::Argb8888,
                &elements,
                [0., 0., 0., 1.],
            ) {
                Ok(download) => {
                    self.flights.insert(
                        key,
                        Flight {
                            selected,
                            download,
                            size,
                            generation,
                            time,
                        },
                    );
                }
                Err(error) => {
                    tracing::warn!(%error, "Could not submit screen download");
                    for pending in selected {
                        pending.frame.failed();
                    }
                }
            }
        }
    }
}

fn copy_pixels(
    pointer: *mut u8,
    len: usize,
    buffer: smithay::wayland::shm::BufferData,
    region: Rectangle<i32, Physical>,
    size: smithay::utils::Size<i32, Physical>,
    pixels: &[u8],
    flipped: bool,
) -> bool {
    if !Rectangle::from_size(size).contains_rect(region) || !valid_buffer(len, buffer, region) {
        return false;
    }
    for row in 0..region.size.h {
        let y = if flipped {
            size.h - 1 - (region.loc.y + row)
        } else {
            region.loc.y + row
        };
        let start = (y as usize * size.w as usize + region.loc.x as usize) * 4;
        let bytes = region.size.w as usize * 4;
        if start + bytes > pixels.len() {
            return false;
        }

        unsafe {
            std::ptr::copy_nonoverlapping(
                pixels.as_ptr().add(start),
                pointer.add(buffer.offset as usize + row as usize * buffer.stride as usize),
                bytes,
            );
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shm_copy_checks_extent_stride_format_and_offset() {
        use smithay::wayland::shm::BufferData;
        let region = Rectangle::from_size((4, 3).into());
        let buffer = BufferData {
            offset: 8,
            width: 4,
            height: 3,
            stride: 16,
            format: wl_shm::Format::Xrgb8888,
        };
        assert!(valid_buffer(56, buffer, region));
        assert!(!valid_buffer(55, buffer, region));
        assert!(!valid_buffer(
            1000,
            BufferData {
                stride: 8,
                ..buffer
            },
            region
        ));
        assert!(!valid_buffer(
            1000,
            BufferData {
                offset: -1,
                ..buffer
            },
            region
        ));
        assert!(!valid_buffer(
            1000,
            BufferData {
                format: wl_shm::Format::Argb8888,
                ..buffer
            },
            region
        ));
        assert!(!valid_buffer(
            1000,
            BufferData {
                width: 0,
                height: 0,
                stride: 0,
                ..buffer
            },
            Rectangle::default()
        ));
    }
    #[test]
    fn capture_global_is_only_advertised_to_authorized_clients() {
        use std::{os::unix::net::UnixStream, sync::atomic::AtomicBool, time::Instant};
        use wayland_client::{
            globals::{registry_queue_init, GlobalListContents},
            protocol::wl_registry,
            Connection, QueueHandle,
        };
        struct Events;
        impl wayland_client::Dispatch<wl_registry::WlRegistry, GlobalListContents> for Events {
            fn event(
                _: &mut Self,
                _: &wl_registry::WlRegistry,
                _: wl_registry::Event,
                _: &GlobalListContents,
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
        for allowed in [false, true] {
            let (server, client) = UnixStream::pair().unwrap();
            let done = Arc::new(AtomicBool::new(false));
            let end = done.clone();
            let server = std::thread::spawn(move || {
                let mut display = wayland_server::Display::<RixState>::new().unwrap();
                let mut state = RixState::new(&mut display, "capture-permissions".into());
                let _client = display
                    .handle()
                    .insert_client(
                        server,
                        Arc::new(ClientState {
                            allow_capture: allowed,
                            ..Default::default()
                        }),
                    )
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                while !end.load(Ordering::Relaxed) && Instant::now() < deadline {
                    display.dispatch_clients(&mut state).unwrap();
                    display.flush_clients().unwrap();
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
            let connection = Connection::from_socket(client).unwrap();
            let (globals, _) = registry_queue_init::<Events>(&connection).unwrap();
            let advertised = globals.contents().with_list(|list| {
                list.iter()
                    .any(|g| g.interface == "zwlr_screencopy_manager_v1")
            });
            done.store(true, Ordering::Relaxed);
            server.join().unwrap();
            assert_eq!(advertised, allowed);
        }
    }
}
