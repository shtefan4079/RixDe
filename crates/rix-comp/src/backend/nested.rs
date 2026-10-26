use crate::autostart::AutostartManager;
use crate::greeter::{BackendConfig, GreeterProcess};
use crate::state::{ClientState, RixState};
use rix_ipc::IpcServer;
use smithay::{
    backend::{
        renderer::{
            damage::OutputDamageTracker,
            element::{render_elements, texture::TextureRenderElement},
            gles::{GlesRenderer, GlesTexture},
        },
        winit::{self, WinitEvent},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{calloop::EventLoop, wayland_server::Display},
    utils::{Size, Transform},
    wayland::socket::ListeningSocketSource,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::info;
mod test_input;

render_elements! {
    NestedRenderElements<=GlesRenderer>;
    Space=crate::render::SceneElement,
    CursorSurface=smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement<GlesRenderer>,
    Texture=TextureRenderElement<GlesTexture>,
}

pub fn run_nested(
    socket_name: &str,
    greeter_command: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let greeter_mode = greeter_command.is_some();
    use crate::watchdog::{mark, OutputDiagnostic, Phase};
    let mut heartbeat = (!greeter_mode).then(crate::watchdog::SessionHeartbeat::from_environment);
    if let Some(heartbeat) = heartbeat.as_mut() {
        heartbeat.output_snapshot("nested", false, false, Vec::new);
    }
    let mut event_loop: EventLoop<RixState> = EventLoop::try_new()?;
    let mut display: Display<RixState> = Display::new()?;
    let mut state = RixState::new_with_mode(&mut display, socket_name.to_string(), greeter_mode);
    let mut test_input = if greeter_mode {
        None
    } else {
        test_input::TestInput::from_environment()?
    };

    mark(heartbeat.as_ref(), Phase::SeatInitialization);
    let _keyboard = state.seat.add_keyboard(Default::default(), 200, 200)?;
    let _pointer = state.seat.add_pointer();

    let config = BackendConfig::new(greeter_mode)?;
    state.set_decoration_theme(&config.get_theme());
    state.set_window_appearance(config.get_desktop().window_appearance);
    state.set_server_side_decorations(config.get_desktop().server_side_decorations);
    state.configure_keyboard(config.get_layout().keyboard)?;
    let mut layout_changes = config.subscribe();
    let mut theme_changes = config.subscribe_theme();
    let mut desktop_changes = config.subscribe_desktop();
    let theme_cfg = config.get_theme();
    let display_cfg = config.get_display();

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
            None,
            state.audio_hotkey_rx.take().unwrap_or_else(|| {
                tracing::warn!("Desktop audio hotkey receiver is unavailable");
                tokio::sync::mpsc::channel(1).1
            }),
        )?)
    } else {
        None
    };

    mark(heartbeat.as_ref(), Phase::GpuInitialization);
    let attributes = ::winit::window::Window::default_attributes()
        .with_title(if greeter_mode {
            "Rix Login — Preview".into()
        } else {
            format!("RixDE — {socket_name}")
        })
        .with_inner_size(::winit::dpi::LogicalSize::new(
            display_cfg.width,
            display_cfg.height,
        ));
    let (mut backend, winit_event_loop) =
        match winit::init_from_attributes::<GlesRenderer>(attributes) {
            Ok(res) => res,
            Err(err) => {
                anyhow::bail!("Failed to initialize Winit backend: {}", err);
            }
        };
    let host_size = backend.window_size();
    if display_cfg.width != host_size.w as u32 || display_cfg.height != host_size.h as u32 {
        let mut actual_display = display_cfg.clone();
        actual_display.width = host_size.w as u32;
        actual_display.height = host_size.h as u32;
        config.save_display(actual_display)?;
    }

    let _dmabuf_global = crate::dmabuf::install(
        &mut state.dmabuf_state,
        backend.renderer(),
        &display.handle(),
    );
    info!("Registered zwp_linux_dmabuf_v1 global for hardware accelerated GPU buffer sharing");

    mark(heartbeat.as_ref(), Phase::OutputInitialization);

    let base_transform = if display_cfg.flipped {
        Transform::Flipped180
    } else {
        Transform::Normal
    };
    let output_mode = Mode {
        size: host_size,
        refresh: (display_cfg.refresh_hz.clamp(30, 360) * 1000) as i32,
    };
    info!(
        "Nested output mode: {}x{} @ {}Hz, transform: {:?} (config flipped={})",
        host_size.w, host_size.h, display_cfg.refresh_hz, base_transform, display_cfg.flipped
    );
    let output = Output::new(
        "nested-1".to_string(),
        PhysicalProperties {
            size: Size::from((0, 0)),
            subpixel: Subpixel::Unknown,
            make: "RixDe".into(),
            model: "Virtual Display".into(),
        },
    );
    let _global = output.create_global::<RixState>(&display.handle());
    output.change_current_state(
        Some(output_mode),
        Some(base_transform),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(output_mode);

    output.user_data().insert_if_missing(|| {
        crate::output_control::Placement(std::sync::Mutex::new(Some(rix_config::OutputPosition {
            x: 0,
            y: 0,
        })))
    });
    state.space.map_output(&output, (0, 0));

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
        info!("Nested login compositor uses a private inherited Wayland socket");
    } else {
        info!(
            "Nested Wayland compositor listening on socket: '{}'",
            socket_name_str
        );
    }
    if !greeter_mode {
        std::env::set_var("WAYLAND_DISPLAY", &socket_name_str);
    }

    let handle = event_loop.handle();
    let fixture_input_enabled = test_input.is_some();

    if let Some(socket_source) = socket_source {
        handle
            .insert_source(
                socket_source,
                |client_stream, _, state: &mut RixState| match state
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState::default()))
                {
                    Ok(client) => {
                        info!("Successfully connected & registered Wayland client!");
                        state.clients.push(client);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to insert Wayland client: {}", e);
                    }
                },
            )
            .map_err(|e| anyhow::anyhow!("Failed to register socket listener: {:?}", e))?;
    }

    let output_clone = output.clone();
    let config_for_resize = config.clone();
    let mut last_host_pointer = None;
    handle
        .insert_source(
            winit_event_loop,
            move |event, &mut (), state: &mut RixState| {
                match event {
                    WinitEvent::Resized { size, .. } => {
                        last_host_pointer = None;
                        let pending = state.pending_display_mode.take();
                        let requested_size_reached = pending.as_ref().is_some_and(|(cmd, _)| {
                            cmd.width == size.w as u32 && cmd.height == size.h as u32
                        });
                        let refresh = if requested_size_reached {
                            pending.as_ref().unwrap().0.refresh_hz as i32 * 1000
                        } else {
                            output_clone
                                .current_mode()
                                .map(|m| m.refresh)
                                .unwrap_or(60_000)
                        };
                        let mode = Mode {
                            size: Size::from((size.w, size.h)),
                            refresh,
                        };

                        let current_transform = output_clone.current_transform();
                        output_clone.change_current_state(
                            Some(mode),
                            Some(current_transform),
                            None,
                            None,
                        );
                        state.reflow_output_layout();
                        if let Some((cmd, _)) = pending {
                            if requested_size_reached {
                                let _ = cmd.responder.send(Ok(()));
                            } else {
                                let _ = cmd.responder.send(Err(format!(
                                    "Host window resized to {}x{} instead of requested {}x{}",
                                    size.w, size.h, cmd.width, cmd.height
                                )));
                            }
                        }
                        if !requested_size_reached {
                            let mut actual_display = config_for_resize.get_display();
                            if actual_display.width != size.w as u32
                                || actual_display.height != size.h as u32
                            {
                                actual_display.width = size.w as u32;
                                actual_display.height = size.h as u32;
                                if let Err(err) = config_for_resize.save_display(actual_display) {
                                    tracing::warn!("Could not save host window dimensions: {err}");
                                }
                            }
                        }
                    }

                    WinitEvent::Input(_) if fixture_input_enabled => {}
                    WinitEvent::Input(input_event) => match input_event {
                        smithay::backend::input::InputEvent::PointerMotionAbsolute { event } => {
                            use smithay::backend::input::{AbsolutePositionEvent, Event};

                            let output_scale = output_clone.current_scale().integer_scale();
                            let output_geo = output_clone
                                .current_mode()
                                .map(|m| m.size.to_logical(output_scale))
                                .unwrap_or_else(|| (1280, 720).into());
                            let raw_pos = event.position_transformed(output_geo);

                            let pos = if output_clone.current_transform() == Transform::Flipped180 {
                                smithay::utils::Point::from((
                                    output_geo.w as f64 - raw_pos.x,
                                    output_geo.h as f64 - raw_pos.y,
                                ))
                            } else {
                                raw_pos
                            };
                            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                            let previous = last_host_pointer.replace(pos);
                            if let Some(pointer) = state.seat.get_pointer() {
                                if state.pointer_is_locked() {
                                    if let Some(previous) = previous {
                                        let delta = pos - previous;
                                        pointer.relative_motion(
                                            state,
                                            state.surface_under(pointer.current_location()),
                                            &smithay::input::pointer::RelativeMotionEvent {
                                                delta,
                                                delta_unaccel: delta,
                                                utime: event.time(),
                                            },
                                        );
                                    }
                                } else {
                                    let pos = state.constrain_pointer_location(pos);
                                    let under = state.surface_under(pos);
                                    pointer.motion(
                                        state,
                                        under,
                                        &smithay::input::pointer::MotionEvent {
                                            location: pos,
                                            serial,
                                            time: event.time_msec(),
                                        },
                                    );
                                    state.update_pointer_constraint();
                                }
                                pointer.frame(state);
                                let _ = state.display_handle.flush_clients();
                            }
                        }
                        smithay::backend::input::InputEvent::PointerButton { event } => {
                            use smithay::backend::input::{Event, PointerButtonEvent};
                            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                            state.handle_pointer_button(
                                event.button_code(),
                                event.state(),
                                serial,
                                event.time_msec(),
                            );
                            let _ = state.display_handle.flush_clients();
                        }
                        smithay::backend::input::InputEvent::PointerAxis { event } => {
                            use smithay::backend::input::{
                                Axis, AxisSource, Event, PointerAxisEvent,
                            };
                            if let Some(pointer) = state.seat.get_pointer() {
                                let mut frame =
                                    smithay::input::pointer::AxisFrame::new(event.time_msec())
                                        .source(event.source());
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
                                    frame = frame
                                        .relative_direction(axis, event.relative_direction(axis));
                                }
                                pointer.axis(state, frame);
                                pointer.frame(state);
                                let _ = state.display_handle.flush_clients();
                            }
                        }
                        smithay::backend::input::InputEvent::Keyboard { event } => {
                            use smithay::backend::input::{Event, KeyboardKeyEvent};
                            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                            let time = event.time_msec();
                            state.handle_keyboard_key(
                                event.key_code(),
                                event.state(),
                                serial,
                                time,
                                true,
                            );
                            let _ = state.display_handle.flush_clients();
                        }
                        _ => {}
                    },
                    WinitEvent::CloseRequested => {
                        info!("Nested window close requested. Exiting compositor.");
                        state
                            .running
                            .store(false, std::sync::atomic::Ordering::SeqCst);
                    }
                    _ => {}
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("Failed to register winit event source: {:?}", e))?;

    let mut autostart = AutostartManager::new();
    autostart.set_cursor_environment(&theme_cfg.cursor_theme, theme_cfg.cursor_size);
    let desktop_settings = config.get_desktop();
    if let Some(desktop_config) = config.desktop() {
        state.refresh_shell_targets(&config.get_layout());
        autostart.spawn_shells(
            &socket_name_str,
            false,
            &desktop_settings.language,
            &state.shell_targets,
        );
        autostart.spawn_session_apps(
            desktop_config.config_dir(),
            &socket_name_str,
            false,
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
        "Nested compositor initialized; running event loop"
    );

    let mut scene_renderer = crate::render::SceneRenderer::default();
    let mut damage_tracker = nested_damage_tracker(&output);
    let mut change_tracker = nested_damage_tracker(&output);
    let mut render_geometry = (output.current_mode(), output.current_transform());
    let mut last_render = Instant::now();
    let mut presentations = 0u64;
    let mut presentation_failed = false;
    let mut wallpapers = super::wallpaper::Wallpapers::default();
    let mut cursor = super::cursor::CursorRenderer::new(&config.get_theme());
    cursor.preload(output.current_scale().fractional_scale());

    let mut last_scene: Vec<crate::render::SceneElement> = Vec::new();
    let mut force_full_repaint = true;

    while state.running.load(std::sync::atomic::Ordering::SeqCst) {
        mark(heartbeat.as_ref(), Phase::LoopStart);
        if let Some(heartbeat) = heartbeat.as_mut() {
            heartbeat.output_snapshot("nested", true, state.lock.active, || {
                vec![OutputDiagnostic {
                    name: output.name(),
                    screen_off: false,
                    frame_queued: false,
                    pending_submission: false,
                    queue_failed: presentation_failed,
                    last_render,
                    presentations,
                }]
            });
        }
        if let Some(greeter) = greeter.as_mut() {
            greeter.tick(&mut state)?;
            if !state.running.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
        }
        if !greeter_mode {
            state.refresh_shell_targets(&config.get_layout());
            autostart.sync_shells(
                &socket_name_str,
                false,
                &session_language,
                &state.shell_targets,
            );
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
            cursor.preload(output.current_scale().fractional_scale());
            autostart.set_cursor_environment(&theme.cursor_theme, theme.cursor_size);
        }

        while let Ok(command) = state.flip_rx.try_recv() {
            let current = output.current_transform();
            let new_transform = if current == Transform::Flipped180 {
                Transform::Normal
            } else {
                Transform::Flipped180
            };
            info!(
                " Toggled output transform: {:?} -> {:?}",
                current, new_transform
            );
            output.change_current_state(None, Some(new_transform), None, None);
            let _ = command.responder.send(Ok(()));
        }

        while let Ok(command) = state.display_mode_rx.try_recv() {
            let (w, h, hz) = (command.width, command.height, command.refresh_hz);
            if state.pending_display_mode.is_some() {
                let _ = command
                    .responder
                    .send(Err("Another display resize is pending".into()));
                continue;
            }
            let current_size = backend.window_size();
            if current_size.w as u32 == w && current_size.h as u32 == h {
                output.change_current_state(
                    Some(Mode {
                        size: current_size,
                        refresh: (hz * 1000) as i32,
                    }),
                    None,
                    None,
                    None,
                );
                let _ = command.responder.send(Ok(()));
            } else {
                let requested = backend
                    .window()
                    .request_inner_size(::winit::dpi::PhysicalSize::new(w, h));
                if let Some(actual) = requested {
                    if actual.width == w && actual.height == h {
                        output.change_current_state(
                            Some(Mode {
                                size: Size::from((w as i32, h as i32)),
                                refresh: (hz * 1000) as i32,
                            }),
                            None,
                            None,
                            None,
                        );
                        let _ = command.responder.send(Ok(()));
                    } else {
                        let _ = command.responder.send(Err(format!(
                            "Host window accepted {}x{} instead of requested {}x{}",
                            actual.width, actual.height, w, h
                        )));
                    }
                } else {
                    state.pending_display_mode = Some((command, Instant::now()));
                }
            }
        }
        if state
            .pending_display_mode
            .as_ref()
            .is_some_and(|(_, since)| since.elapsed() >= Duration::from_secs(4))
        {
            if let Some((command, _)) = state.pending_display_mode.take() {
                let _ = command
                    .responder
                    .send(Err("Host did not confirm the requested window size".into()));
            }
        }

        let refresh_hz = output
            .current_mode()
            .map(|m| (m.refresh / 1000).max(30))
            .unwrap_or(60) as u64;
        let frame_interval = Duration::from_micros(1_000_000 / refresh_hz);
        let elapsed = last_render.elapsed();
        let timeout = if elapsed < frame_interval {
            frame_interval - elapsed
        } else {
            Duration::ZERO
        };

        mark(heartbeat.as_ref(), Phase::EventDispatch);
        if let Err(e) = event_loop.dispatch(Some(timeout), &mut state) {
            tracing::warn!("Calloop event loop dispatch error: {}", e);
        }
        if let Some(heartbeat) = heartbeat.as_mut() {
            heartbeat.beat();
        }

        mark(heartbeat.as_ref(), Phase::WaylandDispatch);
        if let Err(e) = display.dispatch_clients(&mut state) {
            tracing::debug!("Wayland dispatch_clients error: {}", e);
        }
        crate::remote_input::refresh(&mut state);
        state.apply_pending_pointer_constraint_focus();

        state.tick();
        if let Some(input) = test_input.as_mut() {
            input.poll(&mut state);
        }
        if !greeter_mode {
            mark(heartbeat.as_ref(), Phase::CapturePoll);
            state.screenshot_writer.poll(
                backend.renderer(),
                &state.space.outputs().cloned().collect::<Vec<_>>(),
            );
            state.poll_previews(backend.renderer());
            state.screencopy.poll(backend.renderer());
        }
        mark(heartbeat.as_ref(), Phase::GpuImport);
        state.dmabuf_imports.process(backend.renderer());
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
            state.process_desktop_commands(backend.renderer(), desktop_config);
        }
        state.apply_pending_pointer_constraint_focus();
        mark(heartbeat.as_ref(), Phase::ClientFlush);
        let _ = state.display_handle.flush_clients();

        mark(heartbeat.as_ref(), Phase::LayoutRefresh);
        state.space.refresh();
        state.refresh_fractional_scales();
        crate::toplevel_observer::refresh(&mut state);

        if last_render.elapsed() >= frame_interval {
            last_render = Instant::now();
            let current_geometry = (output.current_mode(), output.current_transform());
            if render_geometry != current_geometry {
                render_geometry = current_geometry;
                damage_tracker = nested_damage_tracker(&output);
                change_tracker = nested_damage_tracker(&output);

                force_full_repaint = true;
            }

            mark(heartbeat.as_ref(), Phase::GpuImport);
            let wallpaper = if state.lock.active || greeter_mode {
                wallpapers.retain_outputs(&[]);
                None
            } else {
                wallpapers.for_output(backend.renderer(), &output, &config.get_desktop().wallpaper)
            };
            mark(heartbeat.as_ref(), Phase::SceneCompose);
            let composed = match scene_renderer.compose_fullscreen(
                backend.renderer(),
                &state,
                &output,
                wallpaper,
            ) {
                Ok(Some(elems)) => Some(elems),
                Ok(None) => match scene_renderer.compose(
                    backend.renderer(),
                    &state,
                    &output,
                    wallpaper,
                    &config.get_effects(),
                ) {
                    Ok(elems) => Some(elems),
                    Err(e) => {
                        tracing::warn!("space_render_elements error: {}", e);
                        None
                    }
                },
                Err(e) => {
                    tracing::warn!("fullscreen scene error: {}", e);
                    None
                }
            };
            if let Some(elements) = composed {
                last_scene = elements;
            }

            let output_scale = output.current_scale().fractional_scale();
            let mut render_elements: Vec<NestedRenderElements> = Vec::new();
            if let Some(pointer) = state.seat.get_pointer() {
                let location = pointer.current_location();
                if let Some(surfaces) = super::cursor_surface_elements(
                    backend.renderer(),
                    &state.effective_cursor_status(),
                    location,
                    output_scale,
                ) {
                    render_elements.extend(
                        surfaces
                            .into_iter()
                            .map(NestedRenderElements::CursorSurface),
                    );
                } else if let Some(element) = cursor.element(
                    backend.renderer(),
                    &state.effective_cursor_status(),
                    location,
                    output_scale,
                    state.start_time.elapsed(),
                ) {
                    render_elements.push(NestedRenderElements::Texture(element));
                }
            }
            render_elements.extend(last_scene.drain(..).map(NestedRenderElements::Space));
            let mut screenshots = state.screenshots_for_output(&output);
            if state.lock.active || greeter_mode {
                for request in screenshots.drain(..) {
                    let _ = request.responder.send(Err("Session is locked".into()));
                }
            }
            state.screencopy.prune(std::slice::from_ref(&output));
            if !screenshots.is_empty() || state.screencopy.needs_frame(&output) {
                mark(heartbeat.as_ref(), Phase::CaptureCompose);
                match super::capture_output_elements(
                    &mut scene_renderer,
                    backend.renderer(),
                    &state,
                    &output,
                    wallpaper,
                    &config.get_effects(),
                    &mut cursor,
                ) {
                    Ok(capture) => {
                        if !screenshots.is_empty() {
                            state.screenshot_writer.submit(
                                backend.renderer(),
                                &output,
                                &capture,
                                std::mem::take(&mut screenshots),
                            );
                        }
                        let without_cursor: Vec<_> = capture
                            .iter()
                            .filter(|element| matches!(element, super::CaptureElement::Scene(_)))
                            .collect();
                        state.screencopy.render(
                            backend.renderer(),
                            &output,
                            &capture,
                            &without_cursor,
                            state.start_time.elapsed(),
                        );
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Could not prepare capture scene");
                        state.screencopy.fail_output(&output);
                        for request in screenshots.drain(..) {
                            let _ = request.responder.send(Err(error.to_string()));
                        }
                    }
                }
            }
            if !greeter_mode {
                mark(heartbeat.as_ref(), Phase::CaptureCompose);
                crate::window_capture::render(&mut state, backend.renderer(), &mut cursor);
            }
            mark(heartbeat.as_ref(), Phase::GpuRender);
            let changed = change_tracker
                .damage_output(1, &render_elements)
                .map(|(damage, _)| damage.is_some())
                .unwrap_or(true);
            if !changed {

                restore_scene(&mut last_scene, &mut render_elements);
                mark(heartbeat.as_ref(), Phase::FrameCallbacks);
                state.send_frame_callbacks(&output, state.start_time.elapsed());
                mark(heartbeat.as_ref(), Phase::ClientFlush);
                let _ = state.display_handle.flush_clients();
                mark(heartbeat.as_ref(), Phase::LoopComplete);
                continue;
            }

            let age = if force_full_repaint {
                0
            } else {
                backend.buffer_age().unwrap_or(0)
            };
            let mut submitted_damage = None;
            let mut render_succeeded = false;
            let mut bind_failed = None;
            {
                if let Ok((renderer, mut framebuffer)) = backend.bind().map_err(|err| {
                    bind_failed = Some(err);
                }) {
                    let render_res = damage_tracker.render_output(
                        renderer,
                        &mut framebuffer,
                        age,
                        &render_elements,
                        [0.08, 0.08, 0.10, 1.0],
                    );

                    unsafe {
                        use smithay::backend::egl::ffi::egl;
                        egl::SwapInterval(egl::GetCurrentDisplay(), 0);
                    }

                    match render_res {
                        Ok(res) => {
                            submitted_damage = res.damage;
                            render_succeeded = true;
                        }
                        Err(err) => {
                            tracing::warn!("Could not render nested output: {err}");
                            force_full_repaint = true;
                        }
                    }
                }
            }

            if let Some(err) = bind_failed {
                tracing::warn!("Could not bind nested output: {err}");
                force_full_repaint = true;
                if matches!(err, smithay::backend::SwapBuffersError::ContextLost(_)) {
                    anyhow::bail!("Nested renderer context was lost: {err}");
                }
            }
            if render_succeeded {
                mark(heartbeat.as_ref(), Phase::PageFlipQueue);
                match backend.submit(submitted_damage.as_ref().map(|damage| damage.as_slice())) {
                    Ok(()) => {
                        presentations = presentations.wrapping_add(1);
                        presentation_failed = false;
                        force_full_repaint = false;
                        state.lock_frame_presented(&output, state.lock.generation);

                        mark(heartbeat.as_ref(), Phase::FrameCallbacks);
                        state.send_frame_callbacks(&output, state.start_time.elapsed());
                    }
                    Err(err) => {
                        presentation_failed = true;
                        force_full_repaint = true;
                        mark(heartbeat.as_ref(), Phase::OutputRecovery);
                        tracing::warn!("Could not present nested output: {err}");
                        damage_tracker = nested_damage_tracker(&output);
                        change_tracker = nested_damage_tracker(&output);
                        if matches!(err, smithay::backend::SwapBuffersError::ContextLost(_)) {
                            anyhow::bail!("Nested renderer context was lost: {err}");
                        }
                    }
                }
            }

            restore_scene(&mut last_scene, &mut render_elements);
            mark(heartbeat.as_ref(), Phase::ClientFlush);
            let _ = state.display_handle.flush_clients();
        }
        mark(heartbeat.as_ref(), Phase::LoopComplete);
    }

    autostart.shutdown();
    state
        .running
        .store(false, std::sync::atomic::Ordering::SeqCst);
    drop(ipc_thread);
    Ok(())
}

fn nested_gl_transform(output: &Output) -> Transform {
    if output.current_transform() == Transform::Normal {
        Transform::Flipped180
    } else {
        Transform::Normal
    }
}

fn restore_scene(
    scene: &mut Vec<crate::render::SceneElement>,
    elements: &mut Vec<NestedRenderElements>,
) {
    scene.extend(elements.drain(..).filter_map(|element| match element {
        NestedRenderElements::Space(element) => Some(element),
        NestedRenderElements::CursorSurface(_) | NestedRenderElements::Texture(_) => None,
    }));
}

fn nested_damage_tracker(output: &Output) -> OutputDamageTracker {

    let size = output
        .current_mode()
        .map(|mode| mode.size)
        .unwrap_or_else(|| Size::from((1280, 720)));
    OutputDamageTracker::new(
        size,
        output.current_scale().fractional_scale(),
        nested_gl_transform(output),
    )
}
