use crate::state::{ClientState, RixState};
use smithay::utils::{Logical, Point, Rectangle};
use smithay::{
    backend::allocator::dmabuf::Dmabuf,
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_foreign_toplevel_list,
    delegate_layer_shell, delegate_output, delegate_seat, delegate_shm, delegate_xdg_shell,
    desktop::{
        find_popup_root_surface, get_popup_toplevel_coords, layer_map_for_output,
        LayerSurface as DesktopLayerSurface, PopupKeyboardGrab, PopupKind, PopupPointerGrab,
        WindowSurfaceType,
    },
    input::{pointer::CursorImageStatus, Seat, SeatHandler, SeatState},
    output::Output,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{protocol::wl_surface::WlSurface, Client, Resource},
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorHandler, CompositorState},
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        output::OutputHandler,
        selection::data_device::{
            ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
        },
        shell::{
            wlr_layer::{
                Layer, LayerSurface as WlrLayerSurface, WlrLayerShellHandler, WlrLayerShellState,
            },
            xdg::{PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState},
        },
        shm::{ShmHandler, ShmState},
    },
};
use tracing::{info, warn};

#[cfg(test)]
#[path = "popup_tests.rs"]
mod popup_tests;

impl smithay::wayland::foreign_toplevel_list::ForeignToplevelListHandler for RixState {
    fn foreign_toplevel_list_state(
        &mut self,
    ) -> &mut smithay::wayland::foreign_toplevel_list::ForeignToplevelListState {

        let handle = self.display_handle.clone();
        self.window_capture.toplevels.get_or_insert_with(|| {
            smithay::wayland::foreign_toplevel_list::ForeignToplevelListState::new::<Self>(&handle)
        })
    }
}

impl BufferHandler for RixState {
    fn buffer_destroyed(
        &mut self,
        _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    ) {
    }
}

impl CompositorHandler for RixState {
    fn destroyed(&mut self, surface: &WlSurface) {
        smithay::wayland::xdg_foreign::XdgForeignState::surface_destroyed(self, surface);
    }

    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a Client,
    ) -> &'a smithay::wayland::compositor::CompositorClientState {
        if let Some(state) = client.get_data::<ClientState>() {
            &state.compositor_state
        } else {
            static DEFAULT: std::sync::OnceLock<
                smithay::wayland::compositor::CompositorClientState,
            > = std::sync::OnceLock::new();
            DEFAULT.get_or_init(smithay::wayland::compositor::CompositorClientState::default)
        }
    }

    fn commit(&mut self, surface: &WlSurface) {
        use smithay::backend::renderer::utils::on_commit_buffer_handler;
        on_commit_buffer_handler::<Self>(surface);
        crate::capture_policy::surface_commit(self, surface);
        self.popups.commit(surface);
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface) {
            if !popup.is_initial_configure_sent() {

                let positioner = popup.with_pending_state(|state| state.positioner);
                self.position_popup(&popup, positioner);
                if let Err(error) = popup.send_configure() {
                    warn!(%error, "Failed to configure popup");
                }
            }
        }
        crate::surface_effects::surface_commit(surface);

        if let Some(layer) = self
            .layer_surfaces
            .iter()
            .find(|l| l.wl_surface() == surface)
            .cloned()
        {
            let mapped_output = self
                .space
                .outputs()
                .find(|output| {
                    layer_map_for_output(output)
                        .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                        .is_some()
                })
                .cloned();
            let target = mapped_output.or_else(|| self.layer_target_output(&layer));
            if let Some(output) = target {
                let mut map = layer_map_for_output(&output);
                let previous_area = map.non_exclusive_zone();
                map.cleanup();
                if map
                    .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                    .is_none()
                {
                    if let Err(e) = map.map_layer(&layer) {
                        warn!("Failed to map layer surface on commit: {}", e);
                    }
                }
                map.arrange();

                if layer.layer_surface().has_pending_changes() {
                    layer.layer_surface().send_configure();
                }
                let area_changed = map.non_exclusive_zone() != previous_area;

                drop(map);
                if area_changed {
                    self.reflow_output_layout();
                }
            }
        }

        self.commit_toplevel(surface);
        self.sync_layer_keyboard_focus();
        self.space.refresh();
    }
}

impl ShmHandler for RixState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl OutputHandler for RixState {}

impl SeatHandler for RixState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
    }
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        self.defer_pointer_constraint_focus();
        crate::clipboard::focus(self, seat, focused);
        smithay::wayland::selection::primary_selection::set_primary_focus(
            &self.display_handle,
            seat,
            focused
                .filter(|_| !self.lock.active && !self.greeter_mode)
                .and_then(|s| s.client()),
        );
    }
}

impl smithay::wayland::pointer_constraints::PointerConstraintsHandler for RixState {
    fn new_constraint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &smithay::input::pointer::PointerHandle<Self>,
    ) {
        self.update_pointer_constraint();
    }

    fn cursor_position_hint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &smithay::input::pointer::PointerHandle<Self>,
        _location: Point<f64, Logical>,
    ) {

    }
}

pub fn get_toplevel_info(surface: &ToplevelSurface) -> (Option<String>, Option<String>) {
    smithay::wayland::compositor::with_states(surface.wl_surface(), |states| {
        if let Some(data) = states
            .data_map
            .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
        {
            match data.lock() {
                Ok(lock) => (lock.title.clone(), lock.app_id.clone()),
                Err(poisoned) => {
                    let lock = poisoned.into_inner();
                    (lock.title.clone(), lock.app_id.clone())
                }
            }
        } else {
            (None, None)
        }
    })
}

fn popup_target(
    positioner: PositionerState,
    parent_origin: Point<i32, Logical>,
    outputs: impl Iterator<Item = Rectangle<i32, Logical>>,
) -> Option<Rectangle<i32, Logical>> {
    let anchor = positioner.get_anchor_point() + parent_origin;
    let output = outputs
        .filter(|bounds| bounds.size.w > 0 && bounds.size.h > 0)
        .min_by_key(|bounds| {
            let x = i64::from(anchor.x);
            let y = i64::from(anchor.y);
            let left = i64::from(bounds.loc.x);
            let top = i64::from(bounds.loc.y);
            let dx = x - x.clamp(left, left + i64::from(bounds.size.w) - 1);
            let dy = y - y.clamp(top, top + i64::from(bounds.size.h) - 1);
            i128::from(dx) * i128::from(dx) + i128::from(dy) * i128::from(dy)
        })?;
    Some(Rectangle::new(output.loc - parent_origin, output.size))
}

impl RixState {
    fn position_popup(&self, surface: &PopupSurface, positioner: PositionerState) {
        let kind = PopupKind::Xdg(surface.clone());
        let parent_origin = find_popup_root_surface(&kind)
            .ok()
            .and_then(|root| {
                self.space
                    .elements()
                    .find_map(|window| {
                        window.toplevel().filter(|top| top.wl_surface() == &root)?;
                        self.space.element_location(window)
                    })
                    .or_else(|| {
                        self.space.outputs().find_map(|output| {
                            let bounds = self.space.output_geometry(output)?;
                            let map = layer_map_for_output(output);
                            let layer =
                                map.layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)?;
                            let geometry = map.layer_geometry(layer)?;
                            Some(
                                bounds.loc + geometry.loc - layer.bbox().loc
                                    + crate::layer_layout::offset(layer),
                            )
                        })
                    })
            })
            .map(|root_origin| root_origin + get_popup_toplevel_coords(&kind));
        let geometry = parent_origin
            .and_then(|origin| {
                popup_target(
                    positioner,
                    origin,
                    self.space
                        .outputs()
                        .filter_map(|output| self.space.output_geometry(output)),
                )
            })
            .map(|target| positioner.get_unconstrained_geometry(target))
            .unwrap_or_else(|| positioner.get_geometry());
        surface.with_pending_state(|state| {
            state.geometry = geometry;
            state.positioner = positioner;
        });
    }
}

impl XdgShellHandler for RixState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.register_toplevel(surface);
    }

    fn parent_changed(&mut self, surface: ToplevelSurface) {
        self.position_attached_dialog(&surface);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.destroy_toplevel(&surface);
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        surface.with_pending_state(|state| {
            state.positioner = positioner;
        });

        if let Err(e) = self.popups.track_popup(surface.clone().into()) {
            warn!("Failed to track popup: {}", e);
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        self.position_popup(&surface, positioner);
        surface.send_repositioned(token);
    }

    fn grab(
        &mut self,
        surface: PopupSurface,
        seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        serial: smithay::utils::Serial,
    ) {
        if self.lock.active {
            return;
        }
        let seat: Seat<Self> = match Seat::from_resource(&seat) {
            Some(s) => s,
            None => return,
        };
        let popup_kind = PopupKind::Xdg(surface);
        let root = match find_popup_root_surface(&popup_kind) {
            Ok(r) => r,
            Err(_) => return,
        };
        match self.popups.grab_popup(root, popup_kind, &seat, serial) {
            Ok(grab) => {
                self.active_popup_grab = Some(grab.clone());
                if let Some(keyboard) = seat.get_keyboard() {
                    keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
                    keyboard.set_focus(self, grab.current_grab(), serial);
                }
                if let Some(pointer) = seat.get_pointer() {
                    pointer.set_grab(
                        self,
                        PopupPointerGrab::new(&grab),
                        serial,
                        smithay::input::pointer::Focus::Clear,
                    );
                }
                info!("Popup grab started");
            }
            Err(e) => {
                warn!("Failed to grab popup: {}", e);
            }
        }
    }

    fn move_request(
        &mut self,
        surface: ToplevelSurface,
        seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        serial: smithay::utils::Serial,
    ) {
        if self.lock.active {
            return;
        }
        let seat: Seat<Self> = match Seat::from_resource(&seat) {
            Some(s) => s,
            None => return,
        };
        let pointer = match seat.get_pointer() {
            Some(p) => p,
            None => return,
        };
        if !pointer.has_grab(serial) {
            return;
        }
        let Some(start_data) = pointer.grab_start_data() else {
            return;
        };
        if !start_data
            .focus
            .as_ref()
            .is_some_and(|(focus, _)| focus == surface.wl_surface())
        {
            return;
        }
        let window_opt = self
            .space
            .elements()
            .find(|w| w.toplevel() == Some(&surface))
            .cloned();
        if let Some(window) = window_opt {
            if crate::window_manager::is_fullscreen(&window) {
                return;
            }
            let initial_window_location = self.space.element_location(&window).unwrap_or_default();
            let grab = crate::grabs::MoveSurfaceGrab {
                start_data,
                window,
                initial_window_location,
                snap_output: None,
            };
            pointer.set_grab(self, grab, serial, smithay::input::pointer::Focus::Clear);
            info!("Started interactive window move grab for window");
        }
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        serial: smithay::utils::Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        if self.lock.active {
            return;
        }
        if edges == xdg_toplevel::ResizeEdge::None {
            return;
        }
        let Some(seat) = Seat::<Self>::from_resource(&seat) else {
            return;
        };
        let Some(pointer) = seat.get_pointer() else {
            return;
        };
        if !pointer.has_grab(serial) {
            return;
        }
        let Some(start_data) = pointer.grab_start_data() else {
            return;
        };
        if !start_data
            .focus
            .as_ref()
            .is_some_and(|(focus, _)| focus == surface.wl_surface())
        {
            return;
        }
        let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel() == Some(&surface))
            .cloned()
        else {
            return;
        };
        if crate::window_manager::is_fullscreen(&window) {
            return;
        }
        let Some(location) = self.space.element_location(&window) else {
            return;
        };

        let size = window.geometry().size;
        if size.w <= 0 || size.h <= 0 {
            return;
        }
        self.resize_cursor = Some(crate::grabs::resize_cursor(edges));
        let grab = crate::grabs::ResizeSurfaceGrab {
            start_data,
            window,
            edges,
            initial_window_location: location,
            initial_window_size: size,
        };
        pointer.set_grab(self, grab, serial, smithay::input::pointer::Focus::Clear);
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self
            .windows
            .entries
            .iter()
            .find(|(_, w)| w.toplevel() == Some(&surface))
            .map(|(id, _)| *id)
        {
            if !self.windows.maximized.contains_key(&id) {
                let _ = self.apply_window_action(rix_ipc::WindowAction::Maximize(id));
            }
        } else {
            self.request_initial_maximize(&surface, true);
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self
            .windows
            .entries
            .iter()
            .find(|(_, w)| w.toplevel() == Some(&surface))
            .map(|(id, _)| *id)
        {
            if self.windows.maximized.contains_key(&id) {
                let _ = self.apply_window_action(rix_ipc::WindowAction::Maximize(id));
            }
        } else {
            self.request_initial_maximize(&surface, false);
        }
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        self.set_fullscreen(&surface, output);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.unset_fullscreen(&surface);
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self
            .windows
            .entries
            .iter()
            .find(|(_, w)| w.toplevel() == Some(&surface))
            .map(|(id, _)| *id)
        {
            let _ = self.apply_window_action(rix_ipc::WindowAction::Minimize(id));
        }
    }

    fn title_changed(&mut self, _surface: ToplevelSurface) {
        self.emit_window_change();
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        if self.windows.entries.iter().any(|(_, window)| {
            window
                .toplevel()
                .is_some_and(|top| top.wl_surface() == surface.wl_surface())
        }) {

            self.reflow_output_layout();
        }
        self.emit_window_change();
    }
}

impl smithay::wayland::xdg_foreign::XdgForeignHandler for RixState {
    fn xdg_foreign_state(&mut self) -> &mut smithay::wayland::xdg_foreign::XdgForeignState {

        &mut self.xdg_foreign_state
    }
}

impl smithay::wayland::shell::xdg::dialog::XdgDialogHandler for RixState {
    fn modal_changed(&mut self, surface: ToplevelSurface, modal: bool) {
        if modal {
            self.position_attached_dialog(&surface);
            let window = self
                .space
                .elements()
                .find(|window| window.toplevel() == Some(&surface))
                .cloned();
            if let Some(window) = window {
                self.focus_window(&window);
            }
            self.refresh_modal_pointer_focus();
        } else if let Some(pointer) = self.seat.get_pointer() {
            let location = pointer.current_location();
            pointer.motion(
                self,
                self.surface_under(location),
                &smithay::input::pointer::MotionEvent {
                    location,
                    serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                    time: self.start_time.elapsed().as_millis() as u32,
                },
            );
            pointer.frame(self);
        }
    }
}

impl WlrLayerShellHandler for RixState {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        info!(namespace, "New layer-shell surface");

        let layer = DesktopLayerSurface::new(surface.clone(), namespace);
        if !self
            .layer_surfaces
            .iter()
            .any(|l| l.wl_surface() == layer.wl_surface())
        {
            self.layer_surfaces.push(layer.clone());
        }

        if let Some(name) = output
            .as_ref()
            .and_then(Output::from_resource)
            .map(|out| out.name())
        {
            self.layer_outputs.insert(layer.wl_surface().clone(), name);
        }
        match self.layer_target_output(&layer) {
            Some(out) => {
                let mut map = layer_map_for_output(&out);
                map.cleanup();
                if let Err(e) = map.map_layer(&layer) {
                    warn!("Failed to map layer surface: {}", e);
                    self.pending_layers.push(layer);
                }
            }
            None => {

                self.pending_layers.push(layer);
            }
        }
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let wl = surface.wl_surface().clone();
        self.layer_outputs.remove(&wl);
        self.pending_layers
            .retain(|layer| layer.wl_surface() != &wl);
        if let Some(pos) = self
            .layer_surfaces
            .iter()
            .position(|l| l.wl_surface() == &wl)
        {
            let layer = self.layer_surfaces.remove(pos);
            for output in self.space.outputs().cloned().collect::<Vec<_>>() {
                layer_map_for_output(&output).unmap_layer(&layer);
            }
            self.reflow_output_layout();
            self.sync_layer_keyboard_focus();
            info!("Layer surface destroyed and unmapped from layer maps");
        }
    }
}

impl DataDeviceHandler for RixState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }

    fn action_choice(
        &mut self,
        available: wayland_server::protocol::wl_data_device_manager::DndAction,
        preferred: wayland_server::protocol::wl_data_device_manager::DndAction,
    ) -> wayland_server::protocol::wl_data_device_manager::DndAction {
        use wayland_server::protocol::wl_data_device_manager::DndAction;
        let mods = self
            .seat
            .get_keyboard()
            .map(|keyboard| keyboard.modifier_state())
            .unwrap_or_default();
        let preferred = if mods.ctrl {
            DndAction::Copy
        } else if mods.shift {
            DndAction::Move
        } else {
            preferred
        };

        smithay::wayland::selection::data_device::default_action_chooser(available, preferred)
    }
}

impl ClientDndGrabHandler for RixState {
    fn started(
        &mut self,
        source: Option<wayland_server::protocol::wl_data_source::WlDataSource>,
        icon: Option<WlSurface>,
        seat: Seat<Self>,
    ) {
        let pointer_origin = seat
            .get_pointer()
            .and_then(|pointer| pointer.grab_start_data())
            .and_then(|start| start.focus.map(|(surface, _)| surface));
        let touch = pointer_origin
            .is_none()
            .then(|| seat.get_touch().and_then(|touch| touch.grab_start_data()))
            .flatten();
        let origin = pointer_origin.or_else(|| {
            touch
                .as_ref()
                .and_then(|start| start.focus.as_ref().map(|(surface, _)| surface.clone()))
        });
        self.selection_start = None;
        self.decoration_button_press = None;
        self.drag = crate::drag_drop::DragState {
            active: true,
            source,
            icon,
            origin,
            touch_slot: touch.as_ref().map(|start| start.slot),
            touch_location: touch.map(|start| start.location),
            started: Some(std::time::Instant::now()),
            finished: None,
        };
    }

    fn dropped(&mut self, _: Option<WlSurface>, accepted: bool, _: Seat<Self>) {
        let finished = self
            .drag
            .icon
            .as_ref()
            .map(|icon| crate::drag_drop::DragFinish {
                icon: smithay::backend::renderer::element::Id::from_wayland_resource(icon),
                ended: std::time::Instant::now(),
                accepted,
            });
        self.drag = crate::drag_drop::DragState {
            finished,
            ..Default::default()
        };
        self.cursor_status = CursorImageStatus::default_named();
    }
}
impl ServerDndGrabHandler for RixState {}

impl DmabufHandler for RixState {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {

        self.dmabuf_imports.push(dmabuf, notifier);
    }
}

delegate_compositor!(RixState);
delegate_xdg_shell!(RixState);
smithay::delegate_xdg_foreign!(RixState);
smithay::delegate_xdg_dialog!(RixState);
delegate_layer_shell!(RixState);
delegate_shm!(RixState);
delegate_dmabuf!(RixState);
delegate_seat!(RixState);
delegate_output!(RixState);
delegate_data_device!(RixState);
delegate_foreign_toplevel_list!(RixState);

impl smithay::wayland::tablet_manager::TabletSeatHandler for RixState {}
smithay::delegate_cursor_shape!(RixState);
smithay::delegate_relative_pointer!(RixState);
smithay::delegate_pointer_constraints!(RixState);
