use smithay::{
    backend::{allocator::dmabuf::Dmabuf, renderer::ImportDma},
    wayland::dmabuf::ImportNotifier,
};

#[derive(Default)]
pub(crate) struct PendingImports(Vec<(Dmabuf, ImportNotifier)>);

impl PendingImports {
    pub fn push(&mut self, buffer: Dmabuf, notifier: ImportNotifier) {
        self.0.push((buffer, notifier));
    }

    pub fn process(&mut self, renderer: &mut impl ImportDma) {
        for (buffer, notifier) in self.0.drain(..) {
            match renderer.import_dmabuf(&buffer, None) {
                Ok(_) => {

                    let _ = notifier.successful::<crate::state::RixState>();
                }
                Err(error) => {
                    tracing::warn!("Client dmabuf could not be imported: {error}");

                    notifier.failed();
                }
            }
        }
    }
}

pub fn install(
    state: &mut smithay::wayland::dmabuf::DmabufState,
    renderer: &smithay::backend::renderer::gles::GlesRenderer,
    display: &wayland_server::DisplayHandle,
) -> smithay::wayland::dmabuf::DmabufGlobal {
    use smithay::{backend::egl::EGLDevice, wayland::dmabuf::DmabufFeedbackBuilder};
    let formats = renderer.dmabuf_formats();
    let device = EGLDevice::device_for_display(renderer.egl_context().display())
        .ok()
        .and_then(|d| d.try_get_render_node().ok().flatten());
    if let Some(node) = device {
        if let Ok(feedback) = DmabufFeedbackBuilder::new(node.dev_id(), formats.clone()).build() {
            tracing::info!(device=?node,"Registered DMA-BUF v4 renderer feedback");
            return state
                .create_global_with_default_feedback::<crate::state::RixState>(display, &feedback);
        }
    }
    tracing::warn!("Renderer has no DRM device; DMA-BUF feedback unavailable");
    state.create_global::<crate::state::RixState>(display, formats)
}

#[cfg(test)]
mod tests {
    use crate::state::{ClientState, RixState};
    use smithay::backend::{
        allocator::{Format, Fourcc, Modifier},
        egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay},
        renderer::gles::GlesRenderer,
    };
    use std::{
        os::{fd::AsFd, unix::net::UnixStream},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };
    use wayland_client::{
        globals::{registry_queue_init, GlobalListContents},
        protocol::{wl_buffer, wl_registry},
        Connection, Dispatch, QueueHandle,
    };
    use wayland_protocols::wp::linux_dmabuf::zv1::client::{
        zwp_linux_buffer_params_v1 as params, zwp_linux_dmabuf_v1 as dmabuf,
    };

    #[derive(Default)]
    struct ClientEvents {
        failed: bool,
        created: bool,
    }
    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ClientEvents {
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
    wayland_client::delegate_noop!(ClientEvents: ignore dmabuf::ZwpLinuxDmabufV1);
    wayland_client::delegate_noop!(ClientEvents: ignore wl_buffer::WlBuffer);
    impl Dispatch<params::ZwpLinuxBufferParamsV1, ()> for ClientEvents {
        fn event(
            state: &mut Self,
            _: &params::ZwpLinuxBufferParamsV1,
            event: params::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                params::Event::Failed => state.failed = true,
                params::Event::Created { .. } => state.created = true,
                _ => {}
            }
        }
        wayland_client::event_created_child!(ClientEvents, params::ZwpLinuxBufferParamsV1, [
            params::EVT_CREATED_OPCODE => (wl_buffer::WlBuffer, ())
        ]);
    }

    #[test]
    #[ignore = "requires EGL; run with --ignored"]
    fn invalid_dmabuf_is_rejected_over_wayland() {
        for immediate in [false, true] {
            let (server_socket, client_socket) = UnixStream::pair().unwrap();
            let done = Arc::new(AtomicBool::new(false));
            let server_done = done.clone();
            let server = std::thread::spawn(move || {
                let egl = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
                let context = EGLContext::new(&egl).unwrap();
                let mut renderer = unsafe { GlesRenderer::new(context) }.unwrap();
                let mut display = wayland_server::Display::<RixState>::new().unwrap();
                let mut state = RixState::new(&mut display, "dmabuf-test".into());
                state.dmabuf_state.create_global::<RixState>(
                    &display.handle(),
                    [Format {
                        code: Fourcc::Argb8888,
                        modifier: Modifier::Linear,
                    }],
                );
                let _client = display
                    .handle()
                    .insert_client(server_socket, Arc::new(ClientState::default()))
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                while !server_done.load(Ordering::Relaxed) && Instant::now() < deadline {
                    display.dispatch_clients(&mut state).unwrap();
                    state.dmabuf_imports.process(&mut renderer);
                    display.flush_clients().unwrap();
                    std::thread::sleep(Duration::from_millis(2));
                }
            });
            let connection = Connection::from_socket(client_socket).unwrap();
            let (globals, mut queue) = registry_queue_init::<ClientEvents>(&connection).unwrap();
            let qh = queue.handle();
            let global: dmabuf::ZwpLinuxDmabufV1 = globals.bind(&qh, 3..=3, ()).unwrap();
            let buffer_params = global.create_params(&qh, ());
            let file = tempfile::tempfile().unwrap();
            file.set_len(64).unwrap();
            buffer_params.add(file.as_fd(), 0, 0, 16, 0, 0);
            if immediate {
                buffer_params.create_immed(
                    4,
                    4,
                    Fourcc::Argb8888 as u32,
                    params::Flags::empty(),
                    &qh,
                    (),
                );
            } else {
                buffer_params.create(4, 4, Fourcc::Argb8888 as u32, params::Flags::empty());
            }
            let mut events = ClientEvents::default();
            while !events.failed && !events.created {
                if queue.roundtrip(&mut events).is_err() {
                    break;
                }
            }
            done.store(true, Ordering::Relaxed);
            server.join().unwrap();
            assert!(!events.created, "Invalid GPU buffer was accepted");
            if immediate {
                let error = connection
                    .protocol_error()
                    .expect("create_immed must report a protocol error");
                assert_eq!(error.code, params::Error::InvalidWlBuffer as u32);
            } else {
                assert!(events.failed, "create must emit failed instead of created");
                assert!(connection.protocol_error().is_none());
            }
        }
    }
}
