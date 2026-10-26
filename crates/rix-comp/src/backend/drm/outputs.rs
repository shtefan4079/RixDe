use super::*;

pub(super) fn apply_mode(
    manager: &DrmOutputManager<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        (),
        DrmDeviceFd,
    >,
    connected: &mut ConnectedOutput,
    request: &rix_ipc::DisplayModeCommand,
) -> anyhow::Result<()> {
    let connector = manager.device().get_connector(connected.connector, true)?;
    let mode = connector
        .modes()
        .iter()
        .copied()
        .find(|mode| {
            let actual = Mode::from(*mode);
            actual.size.w as u32 == request.width
                && actual.size.h as u32 == request.height
                && ((actual.refresh + 500) / 1000) as u32 == request.refresh_hz
        })
        .ok_or_else(|| anyhow::anyhow!("The monitor does not advertise the requested mode"))?;
    let mut profile = crate::output_control::profile(&connected.output)
        .ok_or_else(|| anyhow::anyhow!("Output has no active mode"))?;
    profile.mode = crate::output_control::mode(Mode::from(mode));
    apply_profile(manager, connected, profile)
}

pub(super) fn apply_profile(
    manager: &DrmOutputManager<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        (),
        DrmDeviceFd,
    >,
    connected: &mut ConnectedOutput,
    profile: rix_config::OutputProfile,
) -> anyhow::Result<()> {
    profile.validate().map_err(anyhow::Error::msg)?;
    let connector = manager.device().get_connector(connected.connector, true)?;
    let mode = connector
        .modes()
        .iter()
        .copied()
        .find(|mode| crate::output_control::mode(Mode::from(*mode)) == profile.mode)
        .ok_or_else(|| anyhow::anyhow!("The monitor does not advertise the requested mode"))?;

    connected
        .drm_output
        .with_compositor(|compositor| compositor.use_mode(mode))
        .map_err(|error| anyhow::anyhow!("KMS rejected display mode: {error}"))?;
    connected.output.change_current_state(
        Some(Mode::from(mode)),
        Some(crate::output_control::transform(&profile)),
        Some(smithay::output::Scale::Fractional(
            f64::from(profile.scale_percent) / 100.,
        )),
        None,
    );
    connected
        .output
        .user_data()
        .insert_if_missing(crate::output_control::Placement::default);

    let location = connected.output.current_location();
    let position = profile.position.or(Some(rix_config::OutputPosition {
        x: location.x,
        y: location.y,
    }));
    *connected
        .output
        .user_data()
        .get::<crate::output_control::Placement>()
        .unwrap()
        .0
        .lock()
        .unwrap() = position;
    connected.drm_output.reset_buffers();
    Ok(())
}

pub(super) fn reconcile_outputs(
    manager: &mut DrmOutputManager<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        (),
        DrmDeviceFd,
    >,
    display: &Display<RixState>,
    state: &mut RixState,
    renderer: &mut GlesRenderer,
    outputs: &mut Vec<ConnectedOutput>,
    retired: &mut Vec<(Instant, GlobalId)>,
    display_settings: &rix_config::DisplaySettings,
) {
    let resources = match manager.device().resource_handles() {
        Ok(resources) => resources,
        Err(err) => {
            warn!("Failed to query DRM resources, no outputs created: {}", err);
            return;
        }
    };

    let probes: Vec<_> = resources
        .connectors()
        .iter()
        .filter_map(
            |connector| match manager.device().get_connector(*connector, true) {
                Ok(info) => Some(info),
                Err(error) => {
                    warn!(?connector, %error, "Connector probe failed");
                    None
                }
            },
        )
        .collect();
    let mut changed = false;
    outputs.retain(|attached| {
        let removed = !resources.connectors().contains(&attached.connector)
            || probes.iter().any(|probe| {
                probe.handle() == attached.connector
                    && probe.state() == connector::State::Disconnected
            });
        if removed {
            info!(output = %attached.output.name(), "Output disconnected");
            state.detach_output(&attached.output);
            display
                .handle()
                .disable_global::<RixState>(attached.global.clone());
            retired.push((
                Instant::now() + Duration::from_secs(5),
                attached.global.clone(),
            ));
            changed = true;
        }
        !removed
    });
    let mut used_crtcs: HashSet<_> = outputs.iter().map(|output| output.crtc).collect();
    let mut x_offset: i32 = outputs
        .iter()
        .filter_map(|o| state.space.output_geometry(&o.output))
        .map(|r| r.loc.x + r.size.w)
        .max()
        .unwrap_or(0);

    for conn_info in probes {
        if conn_info.state() != connector::State::Connected
            || outputs
                .iter()
                .any(|output| output.connector == conn_info.handle())
        {
            continue;
        }
        let kms_modes = conn_info.modes();
        if kms_modes.is_empty() {
            warn!(connector = %conn_info, "connected but reports no modes, skipping");
            continue;
        }

        let kms_mode = match kms_modes
            .iter()
            .max_by_key(|mode| {
                let (w, h) = mode.size();
                (
                    mode.mode_type().contains(ModeTypeFlags::PREFERRED),
                    w as u32 * h as u32,
                    mode.vrefresh(),
                )
            })
            .copied()
        {
            Some(mode) => mode,
            None => continue,
        };
        let (width, height) = kms_mode.size();
        let mode = Mode::from(kms_mode);

        let compatible_crtcs: HashSet<crtc::Handle> = conn_info
            .encoders()
            .iter()
            .filter_map(|encoder| manager.device().get_encoder(*encoder).ok())
            .flat_map(|encoder| resources.filter_crtcs(encoder.possible_crtcs()))
            .collect();
        let crtc = conn_info
            .current_encoder()
            .and_then(|encoder| manager.device().get_encoder(encoder).ok())
            .and_then(|encoder_info| encoder_info.crtc())
            .filter(|crtc| compatible_crtcs.contains(crtc) && !used_crtcs.contains(crtc))
            .or_else(|| {
                resources
                    .crtcs()
                    .iter()
                    .find(|crtc| compatible_crtcs.contains(*crtc) && !used_crtcs.contains(*crtc))
                    .copied()
            });
        let crtc = match crtc {
            Some(crtc) => crtc,
            None => {
                warn!(connector = %conn_info, "no free CRTC available, skipping");
                continue;
            }
        };

        let name = format!(
            "{}-{}",
            conn_info.interface().as_str(),
            conn_info.interface_id()
        );
        let (mm_width, mm_height) = conn_info.size().unwrap_or((0, 0));
        let monitor = crate::monitor::Monitor::read(manager.device(), conn_info.handle());
        let output = Output::new(
            name.clone(),
            PhysicalProperties {
                size: Size::from((mm_width as i32, mm_height as i32)),
                subpixel: conn_info.subpixel().into(),
                make: monitor
                    .as_ref()
                    .map_or_else(|| "Unknown".into(), |m| m.make.clone()),
                model: monitor
                    .as_ref()
                    .map_or_else(|| conn_info.to_string(), |m| m.model.clone()),
            },
        );
        if let Some(monitor) = monitor {
            output.user_data().insert_if_missing(|| monitor);
        }
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            None,
            Some((x_offset, 0).into()),
        );
        output.set_preferred(mode);
        for mode in kms_modes {
            output.add_mode(Mode::from(*mode));
        }

        let planes = match manager.device().planes(&crtc) {
            Ok(planes) => planes,
            Err(err) => {
                warn!(connector = %conn_info, "Could not probe CRTC planes: {err}");
                continue;
            }
        };
        let drm_output = match manager.initialize_output::<GlesRenderer, HardwareRenderElements>(
            crtc,
            kms_mode,
            &[conn_info.handle()],
            &output,
            Some(planes),
            renderer,
            &DrmOutputRenderElements::default(),
        ) {
            Ok(output) => output,
            Err(err) => {
                warn!(connector = %conn_info, "Could not create KMS output: {err}");
                continue;
            }
        };

        used_crtcs.insert(crtc);
        let global = output.create_global::<RixState>(&display.handle());

        output.user_data().insert_if_missing(|| {
            crate::output_control::Placement(std::sync::Mutex::new(Some(
                rix_config::OutputPosition { x: x_offset, y: 0 },
            )))
        });
        state.space.map_output(&output, (x_offset, 0));

        info!(
            "DRM output '{}' -> CRTC {:?}, mode {}x{}@{}Hz ({} probed mode(s))",
            name,
            crtc,
            width,
            height,
            kms_mode.vrefresh(),
            kms_modes.len(),
        );

        x_offset += width as i32;
        let mut connected = ConnectedOutput {
            output,
            connector: conn_info.handle(),
            global,
            crtc,
            drm_output,
            frame_queued: false,
            queued_at: None,
            submission_barrier: Default::default(),
            presentations: 0,
            queued_lock_generation: None,
            last_render: Instant::now() - Duration::from_secs(1),
            pending_submission: None,
            recovery: Default::default(),
            screen_off: false,
        };
        if let Some(profile) =
            crate::output_control::saved_profile(&connected.output, display_settings)
        {
            if let Err(error) = apply_profile(manager, &mut connected, profile) {
                warn!(%name, %error, "Saved mode unavailable; keeping monitor preferred mode");
            }
        }
        outputs.push(connected);
        changed = true;
    }

    if changed {
        arrange_outputs(state, outputs);
    }
}

pub(super) fn arrange_outputs(state: &mut RixState, outputs: &[ConnectedOutput]) {

    let mut x = outputs
        .iter()
        .filter_map(|o| {
            let placement = crate::output_control::profile(&o.output)?.position?;
            let size = state.space.output_geometry(&o.output)?.size;
            Some(placement.x.saturating_add(size.w))
        })
        .max()
        .unwrap_or(0)
        .max(0);
    for output in outputs {
        let placement = crate::output_control::profile(&output.output).and_then(|p| p.position);
        let location = placement.map_or((x, 0), |p| (p.x, p.y));
        output
            .output
            .change_current_state(None, None, None, Some(location.into()));
        state.space.map_output(&output.output, location);
        if placement.is_none() {
            if let Some(geometry) = state.space.output_geometry(&output.output) {
                x += geometry.size.w;
            }
        }
    }
    state.reflow_output_layout();
}
