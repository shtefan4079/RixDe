mod cursor;
pub mod drm;
pub mod nested;
mod wallpaper;

pub(crate) use cursor::CursorRenderer;
pub use drm::run_drm;
pub use nested::run_nested;

use rix_config::ConfigManager;
use smithay::{
    backend::renderer::{
        element::{
            surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
            Kind,
        },
        gles::GlesRenderer,
    },
    input::pointer::{CursorImageStatus, CursorImageSurfaceData},
    utils::{IsAlive, Logical, Point, Scale},
    wayland::compositor::with_states,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

smithay::backend::renderer::element::render_elements! {
    pub(crate) CaptureElement<=GlesRenderer>;
    Scene=crate::render::SceneElement,
    CursorSurface=WaylandSurfaceRenderElement<GlesRenderer>,
    CursorTexture=smithay::backend::renderer::element::texture::TextureRenderElement<smithay::backend::renderer::gles::GlesTexture>,
}

pub(crate) fn capture_output_elements(
    scene_renderer: &mut crate::render::SceneRenderer,
    renderer: &mut GlesRenderer,
    state: &crate::state::RixState,
    output: &smithay::output::Output,
    wallpaper: Option<
        &smithay::backend::renderer::element::texture::TextureRenderBuffer<
            smithay::backend::renderer::gles::GlesTexture,
        >,
    >,
    theme: &rix_config::appearance::MaterialEffects,
    cursor: &mut CursorRenderer,
) -> anyhow::Result<Vec<CaptureElement>> {
    anyhow::ensure!(
        !state.greeter_mode,
        "Capture is unavailable on the login display"
    );
    let scene = scene_renderer.compose_capture(renderer, state, output, wallpaper, theme)?;
    let mut elements = Vec::new();
    if let (Some(pointer), Some(geometry)) = (
        state.seat.get_pointer(),
        state.space.output_geometry(output),
    ) {
        let position = pointer.current_location();
        if geometry.to_f64().contains(position) {
            let location = position - geometry.loc.to_f64();
            let scale = output.current_scale().fractional_scale();
            let status = state.effective_cursor_status();
            if let Some(surfaces) = cursor_surface_elements(renderer, &status, location, scale) {
                let filter = crate::capture_policy::CaptureFilter::for_cursor(&status);
                elements.extend(
                    surfaces
                        .into_iter()
                        .filter(|surface| filter.allows(surface))
                        .map(CaptureElement::CursorSurface),
                );
            } else if let Some(element) = cursor.element(
                renderer,
                &status,
                location,
                scale,
                state.start_time.elapsed(),
            ) {
                elements.push(CaptureElement::CursorTexture(element));
            }
        }
    }
    elements.extend(scene.into_iter().map(CaptureElement::Scene));
    Ok(elements)
}

pub(crate) struct IpcDaemon {
    thread: Option<std::thread::JoinHandle<()>>,
    running: Arc<AtomicBool>,
}

impl Drop for IpcDaemon {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(crate) struct NativeSessionIntegration {
    pub commands: std::sync::mpsc::SyncSender<rix_ipc::DesktopCommand>,
    pub locked: tokio::sync::watch::Receiver<bool>,
    pub resumed: std::sync::mpsc::Sender<()>,
    pub inhibit_lid: tokio::sync::watch::Receiver<bool>,
    pub power_state: tokio::sync::watch::Sender<rix_ipc::PowerState>,
    pub sleep_requests: tokio::sync::mpsc::Receiver<rix_services::power::SleepRequest>,
}

pub(crate) fn start_ipc_daemon(
    server: Arc<rix_ipc::IpcServer>,
    config: Arc<ConfigManager>,
    running: Arc<AtomicBool>,
    native_session: Option<NativeSessionIntegration>,
    audio_hotkeys: tokio::sync::mpsc::Receiver<rix_services::audio::Hotkey>,
) -> anyhow::Result<IpcDaemon> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let daemon_running = running.clone();
    let thread =
        std::thread::Builder::new()
            .name("rix-ipc-daemon".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        running.store(false, Ordering::SeqCst);
                        return;
                    }
                };
                runtime.block_on(async {
                tokio::spawn(rix_services::audio::run_hotkeys(audio_hotkeys, config.clone()));
                let appearance_config = config.clone();
                tokio::spawn(async move {
                    loop {
                        match rix_services::appearance::serve(appearance_config.clone()).await {
                            Ok(()) => break,
                            Err(error) => {
                                tracing::warn!(%error, "XDG Settings portal backend unavailable");
                                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            }
                        }
                    }
                });

                if let Some(native) = native_session {
                    let NativeSessionIntegration {
                        commands: sender,
                        locked,
                        resumed,
                        inhibit_lid,
                        power_state,
                        sleep_requests,
                    } = native;
                    let power_config = config.clone();
                    let power_server = server.clone();
                    tokio::spawn(rix_services::power::run(
                        power_config,
                        power_state,
                        power_server,
                        sleep_requests,
                    ));

                    let enabled = *inhibit_lid.borrow();
                    let initial_lid = rix_services::session::LidInhibitor::new(enabled).await;
                    tokio::spawn(async move {
                        let mut guard = initial_lid;
                        loop {
                            let result = match guard {
                                Ok(guard) => guard.watch(inhibit_lid.clone()).await,
                                Err(error) => Err(error),
                            };
                            match result {
                                Ok(()) => break,
                                Err(error) => tracing::warn!(%error, "Lid policy unavailable"),
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            let enabled = *inhibit_lid.borrow();
                            guard = rix_services::session::LidInhibitor::new(enabled).await;
                        }
                    });
                    tokio::spawn(async move {
                        loop {
                            let result = rix_services::session::watch_lock_requests(
                                || {
                                    let sender = sender.clone();
                                    async move {
                                        let (responder, reply) = tokio::sync::oneshot::channel();
                                        sender
                                            .try_send(rix_ipc::DesktopCommand {
                                                action: rix_ipc::DesktopAction::LockSession,
                                                responder,
                                            })
                                            .map_err(|error| match error {
                                                std::sync::mpsc::TrySendError::Full(_) => {
                                                    anyhow::anyhow!("Lock queue busy")
                                                }
                                                std::sync::mpsc::TrySendError::Disconnected(_) => {
                                                    anyhow::anyhow!("Compositor stopped")
                                                }
                                            })?;
                                        tokio::time::timeout(
                                            std::time::Duration::from_secs(4),
                                            reply,
                                        )
                                        .await??
                                        .map_err(anyhow::Error::msg)?;
                                        Ok(())
                                    }
                                },
                                locked.clone(),
                                || {
                                    let _ = resumed.send(());
                                },
                            )
                            .await;
                            if let Err(error) = result {
                                tracing::warn!(%error, "logind session integration unavailable");
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        }
                    });
                }
                tokio::select! {
                    result = server.run_with_ready(|| { let _ = ready_tx.send(Ok(())); }) => {
                        if let Err(error) = result {
                            tracing::error!("IPC daemon stopped: {error}");
                            let _ = ready_tx.send(Err(error));
                        }
                        running.store(false, Ordering::SeqCst);
                    }
                    _ = async {
                        while running.load(Ordering::SeqCst) {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                    } => {}
                }
            });
            })?;
    let daemon = IpcDaemon {
        thread: Some(thread),
        running: daemon_running,
    };
    ready_rx.recv_timeout(std::time::Duration::from_secs(5))??;
    Ok(daemon)
}

pub(crate) fn cursor_surface_elements(
    renderer: &mut GlesRenderer,
    status: &CursorImageStatus,
    location: Point<f64, Logical>,
    scale: f64,
) -> Option<Vec<WaylandSurfaceRenderElement<GlesRenderer>>> {
    match status {
        CursorImageStatus::Hidden => Some(Vec::new()),
        CursorImageStatus::Surface(surface) if surface.alive() => {
            let hotspot = with_states(surface, |states| {
                states
                    .data_map
                    .get::<CursorImageSurfaceData>()
                    .map(|data| data.lock().unwrap().hotspot)
                    .unwrap_or_default()
            });
            Some(render_elements_from_surface_tree(
                renderer,
                surface,
                (location - hotspot.to_f64()).to_physical_precise_round(scale),
                Scale::from(scale),
                1.0,
                Kind::Cursor,
            ))
        }
        _ => None,
    }
}

fn install_shutdown_handler(
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("rix-session-signals".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    tracing::error!("Could not create signal runtime: {err}");
                    return;
                }
            };
            runtime.block_on(async {
                use tokio::signal::unix::{signal, SignalKind};
                let (Ok(mut interrupt), Ok(mut terminate)) = (
                    signal(SignalKind::interrupt()),
                    signal(SignalKind::terminate()),
                ) else {
                    tracing::error!("Could not register session signal handlers");
                    return;
                };
                tokio::select! {
                    _ = interrupt.recv() => {},
                    _ = terminate.recv() => {},
                }
                running.store(false, std::sync::atomic::Ordering::SeqCst);
            });
        })?;
    Ok(())
}
