mod control;
mod outputs;
mod startup;

use crate::autostart::AutostartManager;
use crate::greeter::{BackendConfig, GreeterProcess};
use crate::state::{ClientState, RixState};
use rix_ipc::IpcServer;
use smithay::{
    backend::{
        allocator::{
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Fourcc,
        },
        drm::{
            compositor::{FrameFlags, PrimaryPlaneElement},
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
            DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, NodeType,
        },
        egl::{EGLContext, EGLDisplay},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            element::{
                render_elements,
                texture::{TextureRenderBuffer, TextureRenderElement},
            },
            gles::{GlesRenderer, GlesTexture},
        },
        session::{libseat::LibSeatSession, Event as SessionEvent, Session},
        udev::{all_gpus, primary_gpu, UdevBackend, UdevEvent},
    },
    output::{Mode, Output, PhysicalProperties},
    reexports::{
        calloop::EventLoop,
        drm::control::{connector, crtc, Device as ControlDevice, ModeTypeFlags},
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::{backend::GlobalId, Display},
    },
    utils::{DeviceFd, Point, Size, Transform},
    wayland::socket::ListeningSocketSource,
};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::os::unix::io::OwnedFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

type HardwareOutput =
    DrmOutput<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>;
type SessionHandler = Box<dyn FnMut(SessionEvent, &mut RixState)>;

struct ConnectedOutput {
    output: Output,
    crtc: crtc::Handle,
    connector: connector::Handle,
    global: GlobalId,
    drm_output: HardwareOutput,
    frame_queued: bool,
    queued_at: Option<crate::presentation::SubmissionTime>,
    submission_barrier: crate::presentation::SubmissionBarrier,
    presentations: u64,
    queued_lock_generation: Option<u64>,
    last_render: Instant,
    screen_off: bool,
    pending_submission: Option<PendingSubmission>,
    recovery: crate::presentation::Recovery,
}

struct PendingSubmission {
    sync: smithay::backend::renderer::sync::SyncPoint,
    lock_generation: Option<u64>,
    queue_failed: bool,
    prepared_at: Instant,
}

impl ConnectedOutput {
    fn frame_interval(&self) -> Duration {
        let millihertz = self
            .output
            .current_mode()
            .map(|mode| mode.refresh.max(1) as u64)
            .unwrap_or(60_000);
        Duration::from_micros((1_000_000_000 / millihertz).max(1_000))
    }
}

struct RegisteredDrmEvents {
    handle: smithay::reexports::calloop::LoopHandle<'static, RixState>,
    token: smithay::reexports::calloop::RegistrationToken,
}
impl Drop for RegisteredDrmEvents {
    fn drop(&mut self) {
        self.handle.remove(self.token);
    }
}

render_elements! {
    HardwareRenderElements<=GlesRenderer>;
    Space=crate::render::SceneElement,
    CursorSurface=smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement<GlesRenderer>,
    Texture=TextureRenderElement<GlesTexture>,
}

pub fn run_drm(socket_name: &str, greeter_command: Option<&std::path::Path>) -> anyhow::Result<()> {
    let greeter_mode = greeter_command.is_some();
    use crate::watchdog::{mark, OutputDiagnostic, Phase};
    let mut heartbeat = (!greeter_mode).then(crate::watchdog::SessionHeartbeat::from_environment);
    if let Some(heartbeat) = heartbeat.as_mut() {
        heartbeat.output_snapshot("drm", false, false, Vec::new);
    }
    info!("Initializing Direct Hardware DRM/KMS backend...");
    std::env::set_var("RIX_NATIVE_SESSION", "1");

    let mut event_loop: EventLoop<RixState> = EventLoop::try_new()?;
    let mut display: Display<RixState> = Display::new()?;
    let mut state = RixState::new_with_mode(&mut display, socket_name.to_string(), greeter_mode);
    state.native_session = true;

    let _keyboard = state.seat.add_keyboard(Default::default(), 200, 200)?;
    let _pointer = state.seat.add_pointer();
    let _touch = state.seat.add_touch();

    let config = BackendConfig::new(greeter_mode)?;
    state.set_decoration_theme(&config.get_theme());
    state.set_window_appearance(config.get_desktop().window_appearance);
    state.set_server_side_decorations(config.get_desktop().server_side_decorations);
    state.configure_keyboard(config.get_layout().keyboard)?;
    let mut layout_changes = config.subscribe();
    let mut theme_changes = config.subscribe_theme();
    let mut desktop_changes = config.subscribe_desktop();
    let (locked_tx, locked_rx) = tokio::sync::watch::channel(false);
    if !greeter_mode {
        state.lock.logind_hint = Some(locked_tx);
    }
    let (resumed_tx, resumed_rx) = std::sync::mpsc::channel();
    let (lid_tx, lid_rx) = tokio::sync::watch::channel(
        config
            .get_desktop()
            .session
            .keep_running_with_external_display
            && !greeter_mode
            && rix_services::session::external_display_connected(),
    );
    let mut lid_policy = rix_services::session::LidPolicy::new(*lid_tx.borrow());
    let (power_tx, power_rx) = tokio::sync::watch::channel(rix_ipc::PowerState {
        native: true,
        ..Default::default()
    });
    let (sleep_tx, sleep_rx) = tokio::sync::mpsc::channel(4);
    let mut dim_buffer =
        smithay::backend::renderer::element::solid::SolidColorBuffer::new((1, 1), [0., 0., 0., 0.]);
    let mut dim_level = 0.;
    let ipc_thread = if let Some(desktop_config) = config.desktop() {
        let ipc_server = Arc::new(IpcServer::new(desktop_config.clone()));
        ipc_server.set_screenshot_sender(state.screenshot_sender.clone());
        ipc_server.set_flip_sender(state.flip_sender.clone());
        ipc_server.set_display_mode_sender(state.display_mode_sender.clone());
        ipc_server.set_window_sender(state.window_sender.clone());
        ipc_server.set_desktop_sender(state.desktop_sender.clone());
        state.ipc_event_sender = Some(ipc_server.event_sender());
        Some(super::start_ipc_daemon(
            ipc_server,
            desktop_config.clone(),
            state.running.clone(),
            Some(super::NativeSessionIntegration {
                commands: state.lock_sender.clone(),
                locked: locked_rx,
                resumed: resumed_tx,
                inhibit_lid: lid_rx,
                power_state: power_tx,
                sleep_requests: sleep_rx,
            }),
            state.audio_hotkey_rx.take().unwrap_or_else(|| {
                tracing::warn!("Desktop audio hotkey receiver is unavailable");
                tokio::sync::mpsc::channel(1).1
            }),
        )?)
    } else {
        None
    };

    mark(heartbeat.as_ref(), Phase::SeatInitialization);
    let (mut session, session_notifier) = match LibSeatSession::new() {
        Ok(s) => s,
        Err(err) => {
            anyhow::bail!(
                "Failed to initialize libseat session: {}. Ensure RixDe is run with seat access or from a TTY.",
                err
            );
        }
    };

    let seat_name = session.seat();
    info!(seat = %seat_name, active = session.is_active(), "libseat session opened");

    let active = Rc::new(Cell::new(session.is_active()));
    let session_handler: Rc<RefCell<Option<SessionHandler>>> = Rc::new(RefCell::new(None));
    let handler_for_notifier = session_handler.clone();
    let active_for_bootstrap = active.clone();
    event_loop
        .handle()
        .insert_source(session_notifier, move |event, _, state: &mut RixState| {
            if let Some(handler) = handler_for_notifier.borrow_mut().as_mut() {
                handler(event, state);
            } else {
                active_for_bootstrap.set(matches!(event, SessionEvent::ActivateSession));
            }
        })
        .map_err(|e| anyhow::anyhow!("Failed to register session notifier: {:?}", e))?;

    mark(heartbeat.as_ref(), Phase::GpuInitialization);
    let gpu_path: PathBuf = match primary_gpu(&seat_name) {
        Ok(Some(path)) => path,
        _ => match all_gpus(&seat_name)?.into_iter().next() {
            Some(path) => path,
            None => anyhow::bail!("No suitable DRM GPU device found on seat '{}'", seat_name),
        },
    };
    let primary_gpu_node = DrmNode::from_path(&gpu_path)?;
    info!(
        "Primary GPU discovered: {:?} at '{}'",
        primary_gpu_node,
        gpu_path.display()
    );

    let drm_fd = startup::initialize_when_active(
        &mut event_loop,
        &mut state,
        &mut session,
        Duration::from_secs(10),
        |session| {
            let owned: OwnedFd = session
                .open(&gpu_path, OFlags::RDWR | OFlags::CLOEXEC)
                .map_err(|err| {
                    anyhow::anyhow!(
                        "Failed to open DRM device '{}' via libseat session: {:?}. \
                 Check seat permissions and that /dev/dri/* exists.",
                        gpu_path.display(),
                        err
                    )
                })?;
            Ok(DrmDeviceFd::new(DeviceFd::from(owned)))
        },
    )?;
    info!(seat = %seat_name, "libseat session active; initializing DRM");
    let (drm_device, drm_notifier) = DrmDevice::new(drm_fd.clone(), true).map_err(|err| {
        anyhow::anyhow!(
            "Failed to initialize DrmDevice on '{}': {}. A DRM master-capable GPU is required for the drm backend.",
            gpu_path.display(),
            err
        )
    })?;
    info!(
        "DrmDevice ready on '{}' (atomic modesetting: {})",
        gpu_path.display(),
        drm_device.is_atomic()
    );

    let gbm = GbmDevice::new(drm_fd.clone()).map_err(|err| {
        anyhow::anyhow!(
            "Failed to initialize GBM device on '{}': {}. The GPU driver must support GBM.",
            gpu_path.display(),
            err
        )
    })?;
    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );

    let egl_display = unsafe { EGLDisplay::new(gbm.clone()) }.map_err(|err| {
        anyhow::anyhow!(
            "Failed to create EGLDisplay for '{}': {}. EGL on the GBM platform is required.",
            gpu_path.display(),
            err
        )
    })?;
    let egl_context = EGLContext::new(&egl_display).map_err(|err| {
        anyhow::anyhow!(
            "Failed to create EGLContext: {}. Check Mesa/EGL drivers.",
            err
        )
    })?;
    let mut renderer = unsafe { GlesRenderer::new(egl_context) }
        .map_err(|err| anyhow::anyhow!("Failed to initialize GLES renderer: {}", err))?;

    let _dmabuf_global =
        crate::dmabuf::install(&mut state.dmabuf_state, &renderer, &display.handle());
    info!("Registered zwp_linux_dmabuf_v1 global for hardware accelerated GPU buffer sharing");

    mark(heartbeat.as_ref(), Phase::OutputInitialization);
    let render_formats = renderer.egl_context().dmabuf_render_formats().clone();
    let render_node = primary_gpu_node
        .node_with_type(NodeType::Render)
        .and_then(Result::ok);
    let framebuffer_exporter = GbmFramebufferExporter::new(gbm.clone(), render_node);
    let output_manager = DrmOutputManager::new(
        drm_device,
        allocator,
        framebuffer_exporter,
        Some(gbm),
        [Fourcc::Abgr8888, Fourcc::Argb8888],
        render_formats.iter().copied(),
    );
    let output_manager = Rc::new(RefCell::new(output_manager));
    let mut outputs = Vec::new();
    let mut retired_globals = Vec::new();
    outputs::reconcile_outputs(
        &mut output_manager.borrow_mut(),
        &display,
        &mut state,
        &mut renderer,
        &mut outputs,
        &mut retired_globals,
        &config.get_display(),
    );
    if outputs.is_empty() {
        warn!("No usable outputs yet; waiting for a connector event");
    }
    let output_count = outputs.len();
    let outputs = Rc::new(RefCell::new(outputs));

    let mut wallpapers = super::wallpaper::Wallpapers::default();
    let mut cursor = super::cursor::CursorRenderer::new(&config.get_theme());
    cursor.preload(
        outputs
            .borrow()
            .iter()
            .map(|output| output.output.current_scale().fractional_scale())
            .fold(1.0, f64::max),
    );

    let socket_source = if greeter_mode {
        None
    } else if socket_name.is_empty() {
        Some(ListeningSocketSource::new_auto()?)
    } else {
        Some(ListeningSocketSource::with_name(socket_name)?)
    };

    let socket_name_str = socket_source
        .as_ref()
        .map(|socket| socket.socket_name().to_string_lossy().to_string())
        .unwrap_or_default();
    state.socket_name = socket_name_str.clone();
    if greeter_mode {
        info!("Native login compositor uses a private inherited Wayland socket");
    } else {
        info!(
            "DRM/KMS Wayland compositor listening on socket: '{}'",
            socket_name_str
        );
    }
    if !greeter_mode {
        std::env::set_var("WAYLAND_DISPLAY", &socket_name_str);
    }
    std::env::set_var("RIX_NATIVE_SESSION", "1");

    let handle = event_loop.handle();

    let mut libinput_context =
        Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
    libinput_context
        .udev_assign_seat(&seat_name)
        .map_err(|_| anyhow::anyhow!("Failed to assign libinput seat '{}'", seat_name))?;
    let libinput_backend = LibinputInputBackend::new(libinput_context.clone());
    let outputs_dirty = Rc::new(Cell::new(false));
    let outputs_dirty_for_session = outputs_dirty.clone();
    let active_for_session = active.clone();
    let output_manager_for_session = Rc::downgrade(&output_manager);
    let outputs_for_session = Rc::downgrade(&outputs);
    let mut paused_focus = None;
    let mut libinput_for_session = libinput_context.clone();
    *session_handler.borrow_mut() = Some(Box::new(move |event, state: &mut RixState| {
        let (Some(output_manager_for_session), Some(outputs_for_session)) = (
            output_manager_for_session.upgrade(),
            outputs_for_session.upgrade(),
        ) else {
            return;
        };
        match event {
            SessionEvent::ActivateSession => {
                info!("libseat: session activated, resuming DRM and libinput");
                if let Err(err) = output_manager_for_session.borrow_mut().activate(false) {
                    warn!("Failed to re-activate DRM device: {}", err);
                    return;
                }
                for output in outputs_for_session.borrow_mut().iter_mut() {
                    reset_output(output);
                }
                if let Err(err) = libinput_for_session.resume() {
                    warn!("Failed to resume libinput: {err:?}");
                }
                outputs_dirty_for_session.set(true);
                state.idle.activity();
                active_for_session.set(true);
                if let Some(keyboard) = state.seat.get_keyboard() {
                    let focus = if state.lock.active {
                        state
                            .lock
                            .surfaces
                            .first()
                            .map(|(_, surface)| surface.wl_surface().clone())
                    } else {
                        paused_focus.take()
                    };
                    keyboard.set_focus(state, focus, smithay::utils::SERIAL_COUNTER.next_serial());
                }
            }
            SessionEvent::PauseSession => {
                info!("libseat: session paused, stopping DRM and libinput");
                active_for_session.set(false);
                paused_focus = state
                    .seat
                    .get_keyboard()
                    .and_then(|keyboard| keyboard.current_focus());
                state.cancel_input();
                libinput_for_session.suspend();
                output_manager_for_session.borrow_mut().pause();
            }
        }
    }));

    let outputs_for_vblank = Rc::downgrade(&outputs);
    let drm_events = handle
        .insert_source(drm_notifier, move |event, _, state: &mut RixState| {
            let Some(outputs_for_vblank) = outputs_for_vblank.upgrade() else {
                return;
            };
            match event {
                DrmEvent::VBlank(crtc) => {
                    if let Some(output) = outputs_for_vblank
                        .borrow_mut()
                        .iter_mut()
                        .find(|output| output.crtc == crtc)
                    {
                        if output.frame_queued {
                            let submitted = match output.drm_output.frame_submitted() {
                                Ok(Some(_)) => {
                                    output.presentations = output.presentations.wrapping_add(1);
                                    output.recovery.presented(Instant::now());
                                    true
                                }
                                Ok(None) => {
                                    warn!(?crtc, "Page flip has no pending frame");
                                    false
                                }
                                Err(err) => {
                                    warn!(?crtc, "Could not release submitted frame: {err}");
                                    output.drm_output.reset_buffers();
                                    false
                                }
                            };
                            output.frame_queued = false;
                            output.queued_at = None;
                            if let Some(generation) = output.queued_lock_generation.take() {
                                if submitted {
                                    state.lock_frame_presented(&output.output, generation);
                                }
                            }
                        }
                    }
                }
                DrmEvent::Error(err) => {
                    warn!("DRM device error: {}", err);
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("Failed to register DRM notifier: {:?}", e))?;

    let _drm_events = RegisteredDrmEvents {
        handle: handle.clone(),
        token: drm_events,
    };

    let udev_backend = UdevBackend::new(&seat_name).map_err(|e| {
        anyhow::anyhow!(
            "Failed to initialize udev monitor on seat '{}': {}",
            seat_name,
            e
        )
    })?;
    for (device_id, path) in udev_backend.device_list() {
        info!(
            "udev: tracking DRM device #{} at '{}'",
            device_id,
            path.display()
        );
    }
    let outputs_dirty_for_udev = outputs_dirty.clone();
    let primary_id = primary_gpu_node.dev_id();
    handle
        .insert_source(
            udev_backend,
            move |event, _, state: &mut RixState| match event {
                UdevEvent::Added { device_id, .. } | UdevEvent::Changed { device_id }
                    if device_id == primary_id =>
                {
                    outputs_dirty_for_udev.set(true)
                }
                UdevEvent::Removed { device_id } if device_id == primary_id => {
                    warn!("Primary DRM device removed; ending session");
                    state.running.store(false, Ordering::SeqCst);
                }
                _ => {}
            },
        )
        .map_err(|e| anyhow::anyhow!("Failed to register udev monitor: {:?}", e))?;

    let mut session_for_input = session.clone();
    handle
        .insert_source(libinput_backend, move |event, _, state: &mut RixState| {
            handle_libinput_event(state, event);
            if let Some(vt) = state.vt_switch.take() {
                if let Err(error) = session_for_input.change_vt(vt) {
                    warn!(vt, ?error, "Could not switch VT");
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("Failed to register libinput backend: {:?}", e))?;

    if let Some(socket_source) = socket_source {
        handle
            .insert_source(
                socket_source,
                |client_stream, _, state: &mut RixState| match state
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState::default()))
                {
                    Ok(client) => {
                        tracing::info!("Successfully connected & registered Wayland client!");
                        state.clients.push(client);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to insert Wayland client: {}", e);
                    }
                },
            )
            .map_err(|e| anyhow::anyhow!("Failed to register socket listener: {:?}", e))?;
    }

    let mut scene_renderer = crate::render::SceneRenderer::default();
    let mut autostart = AutostartManager::new();
    let theme_cfg = config.get_theme();
    autostart.set_cursor_environment(&theme_cfg.cursor_theme, theme_cfg.cursor_size);
    let desktop_settings = config.get_desktop();
    if let Some(desktop_config) = config.desktop() {
        state.refresh_shell_targets(&config.get_layout());
        autostart.spawn_shells(
            &socket_name_str,
            true,
            &desktop_settings.language,
            &state.shell_targets,
        );
        autostart.spawn_native_services(
            &socket_name_str,
            &desktop_settings.session,
            &desktop_settings.language,
        );
        autostart.spawn_session_apps(
            desktop_config.config_dir(),
            &socket_name_str,
            true,
            &desktop_settings.language,
        );
    }
    let mut greeter = greeter_command
        .map(|command| GreeterProcess::spawn(command, &mut state))
        .transpose()?;
    let mut session_language = desktop_settings.language.clone();
    super::install_shutdown_handler(state.running.clone())?;

    info!(
        greeter_mode,
        "DRM/KMS hardware pipeline established on '{}' with {} output(s)",
        gpu_path.display(),
        output_count
    );

    let mut output_control = control::OutputControl::default();
    let mut pending_mode: Option<rix_ipc::DisplayModeCommand> = None;
    let mut pending_flip: Option<rix_ipc::FlipCommand> = None;
    while state.running.load(Ordering::SeqCst) {
        mark(heartbeat.as_ref(), Phase::LoopStart);
        if let Some(heartbeat) = heartbeat.as_mut() {
            heartbeat.output_snapshot("drm", active.get(), state.lock.active, || {
                outputs
                    .borrow()
                    .iter()
                    .map(|output| OutputDiagnostic {
                        name: output.output.name(),
                        screen_off: output.screen_off,
                        frame_queued: output.frame_queued,
                        pending_submission: output.pending_submission.is_some(),
                        queue_failed: output
                            .pending_submission
                            .as_ref()
                            .is_some_and(|pending| pending.queue_failed),
                        last_render: output.last_render,
                        presentations: output.presentations,
                    })
                    .collect()
            });
        }
        if let Some(greeter) = greeter.as_mut() {
            greeter.tick(&mut state)?;
            if !state.running.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
        }
        if !greeter_mode {

            if state.space.outputs().next().is_some() {
                state.refresh_shell_targets(&config.get_layout());
                autostart.sync_shells(
                    &socket_name_str,
                    true,
                    &session_language,
                    &state.shell_targets,
                );
            }
            autostart.reap();
        }
        while let Ok(desktop) = desktop_changes.try_recv() {
            state.set_server_side_decorations(desktop.server_side_decorations);
            state.set_window_appearance(desktop.window_appearance);
            if session_language != desktop.language {
                session_language = desktop.language.clone();
                autostart.update_language(&socket_name_str, &session_language);
            }
        }
        while let Ok(theme) = theme_changes.try_recv() {
            state.set_decoration_theme(&theme);
            cursor.update(&theme);
            cursor.preload(
                outputs
                    .borrow()
                    .iter()
                    .map(|output| output.output.current_scale().fractional_scale())
                    .fold(1.0, f64::max),
            );
            autostart.set_cursor_environment(&theme.cursor_theme, theme.cursor_size);
        }
        let timeout = if active.get() {
            outputs
                .borrow()
                .iter()
                .filter(|output| !output.screen_off && !output.frame_queued)
                .map(|output| {
                    if output.pending_submission.is_some() {
                        Duration::from_millis(1)
                    } else {
                        output
                            .frame_interval()
                            .saturating_sub(output.last_render.elapsed())
                    }
                })
                .min()
                .unwrap_or(Duration::from_millis(100))
        } else {
            Duration::from_millis(100)
        };
        let drain_tokens: Vec<_> = outputs
            .borrow()
            .iter()
            .map(|output| (output.crtc, output.submission_barrier.drain_token()))
            .collect();
        mark(heartbeat.as_ref(), Phase::EventDispatch);
        event_loop.dispatch(Some(timeout), &mut state)?;

        for output in outputs.borrow_mut().iter_mut() {
            if let Some((_, token)) = drain_tokens.iter().find(|(crtc, _)| *crtc == output.crtc) {
                output.submission_barrier.events_dispatched(*token);
            }
        }

        if !active.get()
            || outputs.borrow().iter().all(|output| {
                output.screen_off || (!output.frame_queued && output.pending_submission.is_none())
            })
        {
            if let Some(heartbeat) = heartbeat.as_mut() {
                heartbeat.beat();
            }
        }
        let mut resumed = false;
        while resumed_rx.try_recv().is_ok() {
            resumed = true;
        }
        if resumed {
            mark(heartbeat.as_ref(), Phase::OutputRecovery);
            state.idle.activity();

            let focus = state
                .seat
                .get_keyboard()
                .and_then(|keyboard| keyboard.current_focus());
            state.cancel_input();
            libinput_context.suspend();
            if active.get() {
                match libinput_context.resume() {
                    Ok(()) => info!("Input devices reopened after sleep"),
                    Err(error) => warn!(?error, "Could not reopen input devices after sleep"),
                }
                if let Some(keyboard) = state.seat.get_keyboard() {
                    keyboard.set_focus(
                        &mut state,
                        focus,
                        smithay::utils::SERIAL_COUNTER.next_serial(),
                    );
                }
                for output in outputs.borrow_mut().iter_mut() {
                    reset_output(output);
                }
            }
            outputs_dirty.set(true);
        }
        mark(heartbeat.as_ref(), Phase::WaylandDispatch);
        display.dispatch_clients(&mut state)?;
        crate::remote_input::refresh(&mut state);
        state.apply_pending_pointer_constraint_focus();

        state.tick();
        if active.get() && outputs_dirty.replace(false) {
            mark(heartbeat.as_ref(), Phase::OutputConfiguration);
            outputs::reconcile_outputs(
                &mut output_manager.borrow_mut(),
                &display,
                &mut state,
                &mut renderer,
                &mut outputs.borrow_mut(),
                &mut retired_globals,
                &config.get_display(),
            );
            state
                .screencopy
                .prune(&state.space.outputs().cloned().collect::<Vec<_>>());
        }
        let inhibit_lid = lid_policy.update(
            !greeter_mode
                && config
                    .get_desktop()
                    .session
                    .keep_running_with_external_display,
            outputs.borrow().iter().map(|output| output.output.name()),
        );
        lid_tx.send_if_modified(|value| {
            if *value == inhibit_lid {
                return false;
            }
            *value = inhibit_lid;
            true
        });
        retired_globals.retain(|(deadline, global)| {
            if Instant::now() < *deadline {
                return true;
            }
            display.handle().remove_global::<RixState>(global.clone());
            false
        });
        if !greeter_mode {
            mark(heartbeat.as_ref(), Phase::CapturePoll);
            state.screenshot_writer.poll(
                &mut renderer,
                &state.space.outputs().cloned().collect::<Vec<_>>(),
            );
            state.poll_previews(&mut renderer);
            state.screencopy.poll(&mut renderer);
        }
        mark(heartbeat.as_ref(), Phase::GpuImport);
        state.dmabuf_imports.process(&mut renderer);
        if !greeter_mode {
            mark(heartbeat.as_ref(), Phase::WindowCommands);
            state.plugins.tick(
                &state.display_handle,
                &state.socket_name,
                &config.get_desktop(),
            );
            state.process_window_commands();
        }
        state.switcher_settings = config.get_desktop().switcher;
        while let Ok(layout) = layout_changes.try_recv() {
            if layout.keyboard != state.keyboard_config {
                if let Err(error) = state.configure_keyboard(layout.keyboard) {
                    tracing::warn!(%error, "Invalid keyboard configuration");
                }
            }
        }
        if let Some(desktop_config) = config.desktop() {
            mark(heartbeat.as_ref(), Phase::DesktopCommands);
            state.process_desktop_commands(&mut renderer, desktop_config);
        }
        if active.get() && !greeter_mode {
            mark(heartbeat.as_ref(), Phase::PresentationControl);
            let desktop = config.get_desktop();
            let power = power_rx.borrow().clone();
            let policy = desktop
                .power
                .as_ref()
                .map(|p| p.active(power.on_battery).idle.clone())
                .unwrap_or(rix_config::power::IdlePolicy {
                    lock_minutes: desktop.session.idle_lock_minutes,
                    ..Default::default()
                });
            let decision = state.tick_power(
                &policy,
                desktop.session.respect_idle_inhibitors,
                power.sleep_inhibited,
                power.idle_inhibited,
            );
            dim_level = if decision.dim {
                1. - policy.dim_percent as f32 / 100.
            } else {
                0.
            };
            let lock_ready = state
                .lock
                .logind_hint
                .as_ref()
                .is_some_and(|hint| *hint.borrow());
            if decision.sleep
                && lock_ready
                && sleep_tx
                    .try_send(rix_services::power::SleepRequest {
                        action: policy.sleep_action,
                        valid: state.idle.sleep_valid.clone(),
                    })
                    .is_ok()
            {
                state.idle.sleep_sent = true;
            }

            let off =
                decision.screen_off && (!state.lock.active && !state.lock.requested || lock_ready);
            if !output_control.pending() {
                for output in outputs.borrow_mut().iter_mut() {
                    if off && !output.screen_off {
                        match output.drm_output.with_compositor(|c| c.clear()) {
                            Ok(()) => {
                                state.screencopy.fail_output(&output.output);
                                output.screen_off = true;
                                output.frame_queued = false;
                                output.queued_at = None;
                                output.pending_submission = None;
                                output.queued_lock_generation = None;
                                output.submission_barrier.block_until_events_drained();
                            }
                            Err(error) => warn!(%error, "Could not turn off idle output"),
                        }
                    } else if !off && output.screen_off {
                        output.screen_off = false;
                        reset_output(output);
                    }
                }
            }
        }
        state.apply_pending_pointer_constraint_focus();
        mark(heartbeat.as_ref(), Phase::ClientFlush);
        display.flush_clients()?;

        mark(heartbeat.as_ref(), Phase::PresentationControl);
        output_control.tick(
            &mut state,
            &mut outputs.borrow_mut(),
            &output_manager.borrow(),
            active.get(),
        );

        while let Ok(command) = state.flip_rx.try_recv() {
            if let Some(old) = pending_flip.replace(command) {
                let _ = old
                    .responder
                    .send(Err("Superseded by a newer flip request".into()));
            }
        }
        if let Some(command) = pending_flip.take().filter(|c| !c.responder.is_closed()) {
            let mut connected = outputs.borrow_mut();
            if state.lock.active || !active.get() || output_control.pending() {
                let _ = command.responder.send(Err(
                    "Session is locked, inactive, or awaiting mode confirmation".into(),
                ));
            } else if let Some(output) = connected.first_mut() {
                if output.frame_queued || output.pending_submission.is_some() {
                    pending_flip = Some(command);
                } else if let Some(mut profile) = crate::output_control::profile(&output.output) {
                    profile.flipped = !profile.flipped;
                    output.output.change_current_state(
                        None,
                        Some(crate::output_control::transform(&profile)),
                        None,
                        None,
                    );
                    output.drm_output.reset_buffers();
                    state.reflow_output_layout();
                    let _ = command.responder.send(Ok(()));
                } else {
                    warn!(output = %output.output.name(), "Output has no active mode; flip skipped");
                    let _ = command
                        .responder
                        .send(Err("Output has no active mode".into()));
                }
            } else {
                let _ = command.responder.send(Err("No connected output".into()));
            }
        }

        while let Ok(command) = state.display_mode_rx.try_recv() {
            if let Some(old) = pending_mode.replace(command) {
                let _ = old
                    .responder
                    .send(Err("Superseded by a newer display request".into()));
            }
        }

        if let Some(command) = pending_mode.take() {
            if !command.responder.is_closed() {
                let mut connected = outputs.borrow_mut();
                if !active.get() || state.lock.active || output_control.pending() {
                    let _ = command.responder.send(Err(
                        "Session is locked, inactive, or awaiting mode confirmation".into(),
                    ));
                } else if let Some(output) = connected.first_mut() {
                    if output.frame_queued || output.pending_submission.is_some() {
                        pending_mode = Some(command);
                    } else {
                        let result =
                            outputs::apply_mode(&output_manager.borrow(), output, &command)
                                .map_err(|error| error.to_string());
                        if result.is_ok() {
                            outputs::arrange_outputs(&mut state, &connected);
                        }
                        let _ = command.responder.send(result);
                    }
                } else {
                    let _ = command.responder.send(Err("No connected output".into()));
                }
            }
        }

        if greeter_mode
            || state.lock.active
            || !active.get()
            || outputs.borrow().is_empty()
            || outputs.borrow().iter().all(|o| o.screen_off)
        {
            while let Ok(request) = state.screenshot_rx.try_recv() {
                let _ = request
                    .responder
                    .send(Err("Session is locked, inactive, or has no output".into()));
            }
            for request in state.screenshot_pending.drain(..) {
                let _ = request
                    .responder
                    .send(Err("Session is locked, inactive, or has no output".into()));
            }
        }

        let wallpaper_outputs: Vec<_> = if active.get() && !state.lock.active && !greeter_mode {
            outputs
                .borrow()
                .iter()
                .filter(|output| !output.screen_off)
                .map(|output| output.output.name())
                .collect()
        } else {
            Vec::new()
        };
        wallpapers.retain_outputs(&wallpaper_outputs);
        if active.get() {
            mark(heartbeat.as_ref(), Phase::LayoutRefresh);
            state.space.refresh();
            state.refresh_fractional_scales();
            crate::toplevel_observer::refresh(&mut state);
            for output in outputs.borrow_mut().iter_mut() {
                if output.screen_off {
                    continue;
                }
                if !output.submission_barrier.can_submit() {
                    continue;
                }

                let lock_generation = state.lock.active.then_some(state.lock.generation);
                if output
                    .pending_submission
                    .as_ref()
                    .is_some_and(|p| p.lock_generation != lock_generation)
                {
                    mark(heartbeat.as_ref(), Phase::OutputRecovery);
                    reset_output(output);
                }
                if !output.submission_barrier.can_submit() {
                    continue;
                }
                let now = Instant::now();
                let stalled = output
                    .queued_at
                    .as_ref()
                    .is_some_and(|queued| queued.stalled(now))
                    || output.pending_submission.as_ref().is_some_and(|pending| {
                        now.saturating_duration_since(pending.prepared_at)
                            > crate::presentation::STALL_TIMEOUT
                    });
                if stalled && output.recovery.attempt(Instant::now()) {
                    mark(heartbeat.as_ref(), Phase::OutputRecovery);
                    warn!(output = %output.output.name(), "Recovering stalled output while preserving scanout");
                    recover_output(output);
                }
                if !output.submission_barrier.can_submit() {
                    continue;
                }
                mark(heartbeat.as_ref(), Phase::PresentationControl);
                if output
                    .pending_submission
                    .as_ref()
                    .is_some_and(|p| !p.queue_failed && p.sync.is_reached())
                {
                    mark(heartbeat.as_ref(), Phase::PageFlipQueue);
                    queue_prepared(output);
                }
                if output.frame_queued
                    || output.pending_submission.is_some()
                    || output.last_render.elapsed() < output.frame_interval()
                {
                    continue;
                }
                output.last_render = Instant::now();
                mark(heartbeat.as_ref(), Phase::GpuImport);
                let wallpaper = if state.lock.active || greeter_mode {
                    None
                } else {
                    wallpapers.for_output(
                        &mut renderer,
                        &output.output,
                        &config.get_desktop().wallpaper,
                    )
                };
                if render_output(
                    &mut scene_renderer,
                    &config.get_effects(),
                    &mut renderer,
                    &mut state,
                    output,
                    wallpaper,
                    &mut cursor,
                    &mut dim_buffer,
                    dim_level,
                    heartbeat.as_ref(),
                ) {
                    mark(heartbeat.as_ref(), Phase::FrameCallbacks);
                    state.send_frame_callbacks(&output.output, state.start_time.elapsed());
                }
            }
            mark(heartbeat.as_ref(), Phase::ClientFlush);
            display.flush_clients()?;
        }
        mark(heartbeat.as_ref(), Phase::LoopComplete);
    }

    autostart.shutdown();
    state.running.store(false, Ordering::SeqCst);
    drop(ipc_thread);
    Ok(())
}

fn frame_callbacks_allowed(committed: bool, queue_failed: bool) -> bool {
    committed && !queue_failed
}

#[allow(clippy::too_many_arguments)]
fn render_output(
    scene_renderer: &mut crate::render::SceneRenderer,
    theme: &rix_config::appearance::MaterialEffects,
    renderer: &mut GlesRenderer,
    state: &mut RixState,
    connected: &mut ConnectedOutput,
    wallpaper: Option<&TextureRenderBuffer<GlesTexture>>,
    cursor: &mut super::cursor::CursorRenderer,
    dim_buffer: &mut smithay::backend::renderer::element::solid::SolidColorBuffer,
    dim_level: f32,
    heartbeat: Option<&crate::watchdog::SessionHeartbeat>,
) -> bool {
    use crate::watchdog::{mark, Phase};
    mark(heartbeat, Phase::SceneCompose);
    let output = &connected.output;
    let (elements, fullscreen_fast_path) = match scene_renderer
        .compose_fullscreen(renderer, state, output, wallpaper)
    {
        Ok(Some(elements)) => (elements, true),
        Ok(None) => match scene_renderer.compose(renderer, state, output, wallpaper, theme) {
            Ok(elements) => (elements, false),
            Err(err) => {
                warn!(output = %output.name(), "Could not prepare windows for rendering: {err}");
                return false;
            }
        },
        Err(err) => {
            warn!(output = %output.name(), "Could not prepare fullscreen windows for rendering: {err}");
            return false;
        }
    };

    let mut render_elements: Vec<HardwareRenderElements> = Vec::new();
    if let (Some(pointer), Some(geometry)) = (
        state.seat.get_pointer(),
        state.space.output_geometry(output),
    ) {
        let cursor_position = pointer.current_location();
        if geometry.to_f64().contains(cursor_position) {
            let relative = cursor_position - geometry.loc.to_f64();
            let scale = output.current_scale().fractional_scale();
            if let Some(surfaces) = super::cursor_surface_elements(
                renderer,
                &state.effective_cursor_status(),
                relative,
                scale,
            ) {
                render_elements.extend(
                    surfaces
                        .into_iter()
                        .map(HardwareRenderElements::CursorSurface),
                );
            } else if let Some(element) = cursor.element(
                renderer,
                &state.effective_cursor_status(),
                relative,
                scale,
                state.start_time.elapsed(),
            ) {
                render_elements.push(HardwareRenderElements::Texture(element));
            }
        }
    }
    render_elements.extend(elements.into_iter().map(HardwareRenderElements::Space));
    let requests = state.screenshots_for_output(output);
    state
        .screencopy
        .prune(&state.space.outputs().cloned().collect::<Vec<_>>());
    if !requests.is_empty() || state.screencopy.needs_frame(output) {
        mark(heartbeat, Phase::CaptureCompose);
        match super::capture_output_elements(
            scene_renderer,
            renderer,
            state,
            output,
            wallpaper,
            theme,
            cursor,
        ) {
            Ok(capture) => {
                if !requests.is_empty() {
                    state
                        .screenshot_writer
                        .submit(renderer, output, &capture, requests);
                }
                let without_cursor: Vec<_> = capture
                    .iter()
                    .filter(|element| matches!(element, super::CaptureElement::Scene(_)))
                    .collect();
                state.screencopy.render(
                    renderer,
                    output,
                    &capture,
                    &without_cursor,
                    state.start_time.elapsed(),
                );
            }
            Err(error) => {
                warn!(%error, output = %output.name(), "Could not prepare capture scene");
                state.screencopy.fail_output(output);
                for request in requests {
                    let _ = request.responder.send(Err(error.to_string()));
                }
            }
        }
    }
    if !state.greeter_mode {
        mark(heartbeat, Phase::CaptureCompose);
        crate::window_capture::render(state, renderer, cursor);
    }
    if dim_level > 0. {
        if let Some(geometry) = state.space.output_geometry(output) {
            dim_buffer.update(geometry.size, [0., 0., 0., dim_level]);
            let overlay =
                smithay::backend::renderer::element::solid::SolidColorRenderElement::from_buffer(
                    dim_buffer,
                    (0, 0),
                    output.current_scale().fractional_scale(),
                    1.,
                    smithay::backend::renderer::element::Kind::Unspecified,
                );
            render_elements.insert(
                0,
                HardwareRenderElements::Space(crate::render::SceneElement::Solid(overlay)),
            );
        }
    }
    let frame_flags = if fullscreen_fast_path && dim_level == 0. {
        FrameFlags::DEFAULT | FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT_ANY
    } else {
        FrameFlags::DEFAULT
    };
    mark(heartbeat, Phase::GpuRender);
    let frame = match connected.drm_output.render_frame(
        renderer,
        &render_elements,
        [0.08, 0.08, 0.10, 1.0],
        frame_flags,
    ) {
        Ok(frame) => frame,
        Err(err) => {
            warn!(output = %output.name(), "KMS render failed: {err}");
            return false;
        }
    };
    if frame.is_empty {
        return true;
    }
    let sync = if frame.needs_sync() {
        match &frame.primary_element {
            PrimaryPlaneElement::Swapchain(element) => Some(element.sync.clone()),
            _ => None,
        }
    } else {
        None
    };
    drop(frame);
    connected.pending_submission = Some(PendingSubmission {
        sync: sync.unwrap_or_default(),
        lock_generation: state.lock.active.then_some(state.lock.generation),
        queue_failed: false,
        prepared_at: Instant::now(),
    });
    mark(heartbeat, Phase::PresentationControl);
    if connected
        .pending_submission
        .as_ref()
        .is_some_and(|pending| pending.sync.is_reached())
    {
        mark(heartbeat, Phase::PageFlipQueue);
        queue_prepared(connected);
    }
    let queue_failed = connected
        .pending_submission
        .as_ref()
        .is_some_and(|pending| pending.queue_failed);
    frame_callbacks_allowed(true, queue_failed)
}

fn queue_prepared(output: &mut ConnectedOutput) {
    let Some(mut pending) = output.pending_submission.take() else {
        return;
    };
    let queued_at = crate::presentation::SubmissionTime::now();
    match output.drm_output.queue_frame(()) {
        Ok(()) => {
            output.frame_queued = true;
            output.queued_at = Some(queued_at);
            output.queued_lock_generation = pending.lock_generation;
        }
        Err(error) => {

            pending.queue_failed = true;
            output.pending_submission = Some(pending);
            warn!(output = %output.output.name(), %error, "KMS page flip failed");
        }
    }
}

fn recover_output(output: &mut ConnectedOutput) {
    match output
        .drm_output
        .with_compositor(|compositor| compositor.recover_scanout())
    {
        Ok(true) => {
            output.frame_queued = false;
            output.queued_at = None;
            output.pending_submission = None;
            output.queued_lock_generation = None;
            output.submission_barrier.block_until_events_drained();
            output.last_render = Instant::now() - output.frame_interval();
        }
        Ok(false) => {

            if discard_failed_submission(&mut output.pending_submission) {
                output.last_render = Instant::now() - output.frame_interval();
            }
            warn!(output = %output.output.name(), "No retained frame ready for output recovery")
        }
        Err(error) => {
            if discard_failed_submission(&mut output.pending_submission) {
                output.last_render = Instant::now() - output.frame_interval();
            }
            warn!(output = %output.output.name(), %error, "Output recovery failed; retaining scanout buffers")
        }
    }
}

fn discard_failed_submission(pending: &mut Option<PendingSubmission>) -> bool {
    if pending.as_ref().is_some_and(|pending| pending.queue_failed) {
        *pending = None;
        true
    } else {
        false
    }
}

fn reset_output(output: &mut ConnectedOutput) {
    let reset = output.drm_output.with_compositor(|compositor| {

        compositor.clear()?;
        compositor.reset_state()?;
        compositor.reset_buffers();
        Ok::<(), smithay::backend::drm::DrmError>(())
    });
    match reset {
        Ok(()) => {
            output.frame_queued = false;
            output.queued_at = None;
            output.pending_submission = None;
            output.queued_lock_generation = None;
            output.submission_barrier.block_until_events_drained();
            output.last_render = Instant::now() - output.frame_interval();
        }
        Err(error) => warn!(output = %output.output.name(), %error, "Output recovery failed"),
    }
}

fn handle_libinput_event(state: &mut RixState, event: InputEvent<LibinputInputBackend>) {
    use smithay::backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, Event, KeyboardKeyEvent, PointerAxisEvent,
        PointerButtonEvent, PointerMotionEvent, TouchEvent,
    };
    use smithay::input::pointer::{AxisFrame, MotionEvent, RelativeMotionEvent};
    use smithay::utils::SERIAL_COUNTER;

    if !matches!(
        &event,
        InputEvent::DeviceAdded { .. } | InputEvent::DeviceRemoved { .. }
    ) {
        state.idle.activity();
    }
    match event {
        InputEvent::DeviceAdded { device } => info!(
            device = device.sysname(),
            name = device.name(),
            keyboard = device.has_capability(smithay::reexports::input::DeviceCapability::Keyboard),
            pointer = device.has_capability(smithay::reexports::input::DeviceCapability::Pointer),
            "libinput device added"
        ),
        InputEvent::DeviceRemoved { device } => {
            use smithay::reexports::input::DeviceCapability;
            let keyboard = device.has_capability(DeviceCapability::Keyboard);
            let pointer = device.has_capability(DeviceCapability::Pointer);
            let touch = device.has_capability(DeviceCapability::Touch);
            info!(
                device = device.sysname(),
                keyboard, pointer, touch, "libinput device removed"
            );
            state.cancel_removed_device_input(keyboard, pointer, touch);
        }
        InputEvent::Keyboard { event } => {
            state.handle_keyboard_key(
                event.key_code(),
                event.state(),
                SERIAL_COUNTER.next_serial(),
                event.time_msec(),
                true,
            );
        }
        InputEvent::PointerMotion { event } => {
            if let Some(pointer) = state.seat.get_pointer() {
                let current = pointer.current_location();
                let delta = event.delta();
                if state.pointer_is_locked() {
                    pointer.relative_motion(
                        state,
                        state.surface_under(current),
                        &RelativeMotionEvent {
                            delta,
                            delta_unaccel: event.delta_unaccel(),
                            utime: event.time(),
                        },
                    );
                } else if let Some((min_x, min_y, max_x, max_y)) = state.desktop_bounds() {
                    let pos = Point::from((
                        (current.x + delta.x).clamp(min_x, max_x - 1.0),
                        (current.y + delta.y).clamp(min_y, max_y - 1.0),
                    ));
                    let pos = state.constrain_pointer_location(pos);
                    let focus = state.surface_under(pos);
                    pointer.relative_motion(
                        state,
                        focus.clone(),
                        &RelativeMotionEvent {
                            delta,
                            delta_unaccel: event.delta_unaccel(),
                            utime: event.time(),
                        },
                    );
                    pointer.motion(
                        state,
                        focus,
                        &MotionEvent {
                            location: pos,
                            serial: SERIAL_COUNTER.next_serial(),
                            time: event.time_msec(),
                        },
                    );
                    state.update_pointer_constraint();
                }
                pointer.frame(state);
            }
        }
        InputEvent::PointerMotionAbsolute { event } => {
            if let (Some(pointer), Some((min_x, min_y, max_x, max_y))) =
                (state.seat.get_pointer(), state.desktop_bounds())
            {
                if state.pointer_is_locked() {

                    pointer.frame(state);
                } else {
                    let transformed = event.position_transformed(Size::from((
                        (max_x - min_x) as i32,
                        (max_y - min_y) as i32,
                    )));
                    let pos = Point::from((
                        (transformed.x + min_x).clamp(min_x, max_x - 1.0),
                        (transformed.y + min_y).clamp(min_y, max_y - 1.0),
                    ));
                    let pos = state.constrain_pointer_location(pos);
                    let focus = state.surface_under(pos);
                    pointer.motion(
                        state,
                        focus,
                        &MotionEvent {
                            location: pos,
                            serial: SERIAL_COUNTER.next_serial(),
                            time: event.time_msec(),
                        },
                    );
                    state.update_pointer_constraint();
                    pointer.frame(state);
                }
            }
        }
        InputEvent::PointerButton { event } => {
            state.handle_pointer_button(
                event.button_code(),
                event.state(),
                SERIAL_COUNTER.next_serial(),
                event.time_msec(),
            );
        }
        InputEvent::PointerAxis { event } => {
            if let Some(pointer) = state.seat.get_pointer() {
                let mut frame = AxisFrame::new(event.time_msec()).source(event.source());
                for axis in [Axis::Horizontal, Axis::Vertical] {
                    if let Some(value) = event.amount(axis) {
                        frame = frame.value(axis, value);
                        if event.source() == AxisSource::Finger && value == 0.0 {
                            frame = frame.stop(axis);
                        }
                    }
                    if let Some(v120) = event.amount_v120(axis) {
                        frame = frame.v120(axis, v120.round() as i32);
                    }
                    frame = frame.relative_direction(axis, event.relative_direction(axis));
                }
                pointer.axis(state, frame);
                pointer.frame(state);
            }
        }
        InputEvent::TouchDown { event } => {
            if let (Some(touch), Some((min_x, min_y, max_x, max_y))) =
                (state.seat.get_touch(), state.desktop_bounds())
            {
                let transformed = event.position_transformed(Size::from((
                    (max_x - min_x) as i32,
                    (max_y - min_y) as i32,
                )));
                let pos = Point::from((
                    (transformed.x + min_x).clamp(min_x, max_x - 1.0),
                    (transformed.y + min_y).clamp(min_y, max_y - 1.0),
                ));
                let serial = SERIAL_COUNTER.next_serial();
                touch.down(
                    state,
                    state.surface_under(pos),
                    &smithay::input::touch::DownEvent {
                        slot: event.slot(),
                        location: pos,
                        serial,
                        time: event.time_msec(),
                    },
                );
                if state.focus_layer_at(pos, serial).is_none() {
                    let clicked = state.space.element_under(pos).map(|(w, _)| w.clone());
                    if let Some(window) = clicked {
                        state.focus_window(&window);
                    }
                }
            }
        }
        InputEvent::TouchMotion { event } => {
            if let (Some(touch), Some((min_x, min_y, max_x, max_y))) =
                (state.seat.get_touch(), state.desktop_bounds())
            {
                let transformed = event.position_transformed(Size::from((
                    (max_x - min_x) as i32,
                    (max_y - min_y) as i32,
                )));
                let pos = Point::from((
                    (transformed.x + min_x).clamp(min_x, max_x - 1.0),
                    (transformed.y + min_y).clamp(min_y, max_y - 1.0),
                ));
                if state.drag.touch_slot == Some(event.slot()) {
                    state.drag.touch_location = Some(pos);
                }
                touch.motion(
                    state,
                    state.surface_under(pos),
                    &smithay::input::touch::MotionEvent {
                        slot: event.slot(),
                        location: pos,
                        time: event.time_msec(),
                    },
                );
            }
        }
        InputEvent::TouchUp { event } => {
            if let Some(touch) = state.seat.get_touch() {
                touch.up(
                    state,
                    &smithay::input::touch::UpEvent {
                        slot: event.slot(),
                        serial: SERIAL_COUNTER.next_serial(),
                        time: event.time_msec(),
                    },
                );
            }
        }
        InputEvent::TouchCancel { .. } => {
            if let Some(touch) = state.seat.get_touch() {
                touch.cancel(state);
            }
        }
        InputEvent::TouchFrame { .. } => {
            if let Some(touch) = state.seat.get_touch() {
                touch.frame(state);
            }
        }
        _ => {}
    }
    let _ = state.display_handle.flush_clients();
}

impl RixState {

    fn cancel_removed_device_input(&mut self, keyboard: bool, pointer: bool, touch: bool) {
        use smithay::{
            backend::input::{ButtonState, KeyState},
            input::keyboard::FilterResult,
            utils::SERIAL_COUNTER,
        };
        let time = self.start_time.elapsed().as_millis() as u32;
        if keyboard {
            if let Some(handle) = self.seat.get_keyboard() {
                for code in handle.pressed_keys() {
                    handle.input::<(), _>(
                        self,
                        code,
                        KeyState::Released,
                        SERIAL_COUNTER.next_serial(),
                        time,
                        |_, _, _| FilterResult::Forward,
                    );
                }
                self.keyboard_focus_lost();
                handle.set_focus(self, None, SERIAL_COUNTER.next_serial());
            }
            self.finish_switcher(false);
            self.clear_intercepted_keys();
            self.logo_tap = None;
        }
        if pointer {
            if let Some(handle) = self.seat.get_pointer() {
                for button in std::mem::take(&mut self.pressed_buttons) {
                    handle.button(
                        self,
                        &smithay::input::pointer::ButtonEvent {
                            serial: SERIAL_COUNTER.next_serial(),
                            time,
                            button,
                            state: ButtonState::Released,
                        },
                    );
                }
                handle.unset_grab(self, SERIAL_COUNTER.next_serial(), time);
                handle.frame(self);
            }
            self.cancel_drag();
            self.selection_start = None;
            self.resize_cursor = None;
        }
        if touch {
            if let Some(handle) = self.seat.get_touch() {
                handle.cancel(self);
                handle.unset_grab(self);
            }
        }
        self.publish_keyboard_state();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::{backend::input::KeyState, utils::SERIAL_COUNTER};

    #[test]
    fn frame_callbacks_follow_successful_submission_only() {
        assert!(frame_callbacks_allowed(true, false));
        assert!(
            !frame_callbacks_allowed(false, false),
            "Failed composition must not release client throttles"
        );
        assert!(
            !frame_callbacks_allowed(true, true),
            "A failed page flip must not release client throttles"
        );
    }

    #[test]
    fn recovery_without_scanout_discards_a_failed_queue_submission() {

        let mut pending = Some(PendingSubmission {
            sync: Default::default(),
            lock_generation: None,
            queue_failed: true,
            prepared_at: Instant::now(),
        });
        assert!(discard_failed_submission(&mut pending));
        assert!(
            pending.is_none(),
            "A never-queued frame must not freeze the output"
        );

        let mut healthy = Some(PendingSubmission {
            sync: Default::default(),
            lock_generation: None,
            queue_failed: false,
            prepared_at: Instant::now(),
        });
        assert!(!discard_failed_submission(&mut healthy));
        assert!(healthy.is_some());
    }

    #[test]
    fn removed_keyboard_releases_keys_but_live_devices_keep_their_input() {
        let mut display = smithay::reexports::wayland_server::Display::<RixState>::new().unwrap();
        let mut state = RixState::new(&mut display, "device-removal-test".into());
        state
            .seat
            .add_keyboard(Default::default(), 500, 30)
            .unwrap();
        let handle = state.seat.get_keyboard().unwrap();
        handle.input::<(), _>(
            &mut state,
            30u32.into(),
            KeyState::Pressed,
            SERIAL_COUNTER.next_serial(),
            1,
            |_, _, _| smithay::input::keyboard::FilterResult::Forward,
        );
        assert_eq!(handle.pressed_keys().len(), 1);

        state.cancel_removed_device_input(false, true, false);
        assert_eq!(handle.pressed_keys().len(), 1);

        state.cancel_removed_device_input(true, false, false);
        assert!(handle.pressed_keys().is_empty());
    }
}
