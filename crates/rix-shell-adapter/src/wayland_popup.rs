use super::*;
use i_slint_core::window::{PopupWindowLocation, WindowInner};
use smithay_client_toolkit::{
    globals::ProvidesBoundGlobal,
    shell::xdg::popup::{Popup, PopupConfigure, PopupHandler},
};
use wayland_protocols::xdg::shell::client::xdg_positioner::{
    Anchor, ConstraintAdjustment, Gravity,
};

pub(super) struct Factory {
    pub xdg: Rc<XdgShell>,
    pub compositor: Rc<CompositorState>,
    pub shm: Shm,
    pub blur: Option<OrgKdeKwinBlurManager>,
    pub effects: Option<RixSurfaceEffectsManagerV1>,
    pub viewporter: Option<WpViewporter>,
    pub fractional_scale: Option<WpFractionalScaleManagerV1>,
    pub last_press: RefCell<Option<(wl_seat::WlSeat, u32, std::time::Instant)>>,
}

pub(super) struct PopupWindow {
    pub window: Window,
    pub surface: wl_surface::WlSurface,
    parent: Rc<LayerWindow>,
    factory: Rc<Factory>,
    renderer: SoftwareRenderer,

    role: RefCell<Option<Popup>>,
    blur: Option<OrgKdeKwinBlur>,
    effects: Option<RixSurfaceEffectsV1>,
    viewport: Option<WpViewport>,
    pub fractional_scale: Option<WpFractionalScaleV1>,
    pool: RefCell<SlotPool>,
    buffers: RefCell<Vec<Buffer>>,
    region_cache: RefCell<RegionCache>,
    pub size: Cell<PhysicalSize>,
    requested: Cell<LogicalSize>,
    position: Cell<LogicalPosition>,
    pub scale: Cell<f64>,
    pub visible: Cell<bool>,
    configured: Cell<bool>,
    dirty: Cell<bool>,
    pub frame_pending: Cell<bool>,
    token: Cell<u32>,
}

impl PopupWindow {
    pub fn new(parent: Rc<LayerWindow>) -> Result<Rc<Self>, PlatformError> {
        let factory = parent
            .popup_factory
            .clone()
            .ok_or_else(|| error("XDG popups unavailable"))?;
        let qh = parent.qh.clone();
        let surface = factory.compositor.create_surface(&qh);
        let pool = SlotPool::new(4096, &factory.shm).map_err(error)?;
        let window: Rc<Self> = Rc::<Self>::new_cyclic(|weak| Self {
            window: Window::new(weak.clone()),
            renderer: SoftwareRenderer::new_with_repaint_buffer_type(RepaintBufferType::NewBuffer),
            blur: factory.blur.as_ref().map(|m| m.create(&surface, &qh, ())),
            effects: factory
                .effects
                .as_ref()
                .map(|m| m.get_surface(&surface, &qh, ())),
            viewport: factory
                .viewporter
                .as_ref()
                .map(|m| m.get_viewport(&surface, &qh, ())),
            fractional_scale: factory
                .viewporter
                .as_ref()
                .and_then(|_| factory.fractional_scale.as_ref())
                .map(|m| m.get_fractional_scale(&surface, &qh, ())),
            scale: Cell::new(parent.scale.get()),
            surface,
            role: RefCell::new(None),
            factory,
            parent,
            pool: RefCell::new(pool),
            buffers: RefCell::new(Vec::new()),
            region_cache: RefCell::new(RegionCache::default()),
            size: Cell::new(PhysicalSize::new(1, 1)),
            requested: Cell::new(LogicalSize::new(1., 1.)),
            position: Cell::new(LogicalPosition::new(0., 0.)),
            visible: Cell::new(false),
            configured: Cell::new(false),
            dirty: Cell::new(true),
            frame_pending: Cell::new(false),
            token: Cell::new(0),
        });
        Ok(window)
    }
    fn positioner(
        &self,
    ) -> Result<wayland_protocols::xdg::shell::client::xdg_positioner::XdgPositioner, PlatformError>
    {
        let positioner = <XdgShell as ProvidesBoundGlobal<
            wayland_protocols::xdg::shell::client::xdg_wm_base::XdgWmBase,
            5,
        >>::bound_global(&*self.factory.xdg)
        .map_err(error)?
        .create_positioner(&self.parent.qh, ());
        let size = self.requested.get();
        let position = self.position.get();
        positioner.set_size(
            size.width.round().max(1.) as i32,
            size.height.round().max(1.) as i32,
        );
        positioner.set_anchor_rect(position.x.round() as i32, position.y.round() as i32, 1, 1);
        positioner.set_anchor(Anchor::TopLeft);
        positioner.set_gravity(Gravity::BottomRight);
        positioner
            .set_constraint_adjustment(ConstraintAdjustment::SlideX | ConstraintAdjustment::SlideY);
        Ok(positioner)
    }
    fn reposition(&self) {
        if let Some(role) = self.role.borrow().as_ref() {
            if role.xdg_popup().version() >= 3 {
                if let Ok(positioner) = self.positioner() {
                    let token = self.token.get().wrapping_add(1);
                    self.token.set(token);
                    role.reposition(&positioner, token);
                    positioner.destroy();
                }
            }
        }
        self.dirty.set(true);
    }
    pub fn resize(&self, width: u32, height: u32) {
        let scale = self.scale.get();
        if let Some(viewport) = &self.viewport {
            self.surface.set_buffer_scale(1);
            viewport.set_destination(width.max(1) as i32, height.max(1) as i32);
        } else {
            self.surface.set_buffer_scale(scale.round().max(1.) as i32);
        }
        let size = PhysicalSize::new(
            (width as f64 * scale).ceil() as u32,
            (height as f64 * scale).ceil() as u32,
        );
        if self.size.replace(size) != size {
            self.buffers.borrow_mut().clear();
        }
        self.window.dispatch_event(WindowEvent::ScaleFactorChanged {
            scale_factor: scale as f32,
        });
        self.window.dispatch_event(WindowEvent::Resized {
            size: LogicalSize::new(width as f32, height as f32),
        });
        self.dirty.set(true);
    }
    pub fn draw(&self) -> Result<(), PlatformError> {
        if !self.visible.get()
            || !self.configured.get()
            || self.frame_pending.get()
            || !(self.dirty.replace(false) || self.window.has_active_animations())
        {
            return Ok(());
        }
        let size = self.size.get();
        let scale = self.scale.get() as f32;
        let logical = size.to_logical(scale);
        let radius = self
            .parent
            .control
            .0
            .borrow()
            .popup_radius
            .clone()
            .map(|f| f())
            .unwrap_or(4.)
            .max(0.)
            .min(logical.width.min(logical.height) / 2.);
        let rect = crate::SurfaceRect {
            x: 0.,
            y: 0.,
            width: logical.width,
            height: logical.height,
            radius,
            opacity: 1.,
            clip: None,
        };

        let (region, region_changed) = self
            .region_cache
            .borrow_mut()
            .region(&self.factory.compositor, Some(&[rect]))?;
        self.surface.set_input_region(region.as_ref());
        if region_changed {
            if let Some(blur) = &self.blur {
                blur.set_region(region.as_ref());
                blur.commit();
            }
            if let Some(effects) = &self.effects {
                effects.clear();
                effects.add_rectangle(
                    0.,
                    0.,
                    logical.width as f64,
                    logical.height as f64,
                    radius as f64,
                    1.,
                );
                effects.commit();
            }
        }
        let mut pixels =
            vec![PremultipliedRgbaColor::default(); size.width as usize * size.height as usize];
        self.renderer.render(&mut pixels, size.width as usize);
        let mut pool = self.pool.borrow_mut();
        let mut buffers = self.buffers.borrow_mut();
        let index = if let Some(i) = buffers.iter().position(|b| b.canvas(&mut pool).is_some()) {
            i
        } else {

            if buffers.len() >= 3 {
                self.dirty.set(true);
                return Ok(());
            }
            let (buffer, _) = pool
                .create_buffer(
                    size.width as i32,
                    size.height as i32,
                    size.width as i32 * 4,
                    wl_shm::Format::Argb8888,
                )
                .map_err(error)?;
            buffers.push(buffer);
            buffers.len() - 1
        };
        let canvas = buffers[index]
            .canvas(&mut pool)
            .ok_or_else(|| error("Popup buffer busy"))?;
        let r = radius * scale;
        for (index, (pixel, bytes)) in pixels.iter().zip(canvas.chunks_exact_mut(4)).enumerate() {
            let x = (index % size.width as usize) as f32 + 0.5;
            let y = (index / size.width as usize) as f32 + 0.5;
            let dx = (r - x).max(x - (size.width as f32 - r)).max(0.);
            let dy = (r - y).max(y - (size.height as f32 - r)).max(0.);
            let coverage = if dx > 0. && dy > 0. {
                (r + 0.5 - dx.hypot(dy)).clamp(0., 1.)
            } else {
                1.
            };
            bytes.copy_from_slice(
                &[pixel.blue, pixel.green, pixel.red, pixel.alpha]
                    .map(|c| (c as f32 * coverage).round() as u8),
            );
        }
        self.surface
            .damage_buffer(0, 0, size.width as i32, size.height as i32);
        self.surface.frame(&self.parent.qh, self.surface.clone());
        self.frame_pending.set(true);
        buffers[index].attach_to(&self.surface).map_err(error)?;
        self.surface.commit();
        Ok(())
    }
    pub fn done(&self) {
        let parent = WindowInner::from_pub(self.parent.window());
        let id = parent
            .active_popups
            .borrow()
            .iter()
            .find_map(|p| match &p.location {
                PopupWindowLocation::TopLevel(adapter)
                    if std::ptr::eq(adapter.window(), &self.window) =>
                {
                    Some(p.popup_id)
                }
                _ => None,
            });
        if let Some(id) = id {
            parent.close_popup(id);
        }
    }
    pub fn set_cursor(&self, serial: u32) {
        if let Some(device) = self.parent.cursor_device.borrow().as_ref() {
            device.set_shape(serial, Shape::Default);
        }
    }
}
impl WindowAdapterInternal for PopupWindow {
    fn get_parent(&self) -> Option<Rc<dyn WindowAdapter>> {
        Some(self.parent.clone())
    }
    fn set_mouse_cursor(&self, cursor: MouseCursor) {
        self.parent.cursor.set(cursor);
        self.parent.update_cursor();
    }
}
impl Drop for PopupWindow {
    fn drop(&mut self) {

        if let Some(blur) = &self.blur {
            blur.release();
        }
        if let Some(effects) = &self.effects {
            effects.destroy();
        }
        if let Some(viewport) = &self.viewport {
            viewport.destroy();
        }
        if let Some(scale) = &self.fractional_scale {
            scale.destroy();
        }
    }
}
impl WindowAdapter for PopupWindow {
    fn internal(&self, _: i_slint_core::InternalToken) -> Option<&dyn WindowAdapterInternal> {
        Some(self)
    }
    fn window(&self) -> &Window {
        &self.window
    }
    fn renderer(&self) -> &dyn platform::Renderer {
        &self.renderer
    }
    fn size(&self) -> PhysicalSize {
        self.size.get()
    }
    fn set_size(&self, size: WindowSize) {
        let size = size.to_logical(self.scale.get() as f32);
        if self.requested.replace(size) != size {
            self.reposition();
        }
    }
    fn set_position(&self, position: slint::WindowPosition) {
        let position = match position {
            slint::WindowPosition::Logical(p) => p,
            slint::WindowPosition::Physical(p) => p.to_logical(self.scale.get() as f32),
        };
        if self.position.replace(position) != position {
            self.reposition();
        }
    }
    fn position(&self) -> Option<slint::PhysicalPosition> {
        Some(self.position.get().to_physical(self.scale.get() as f32))
    }
    fn request_redraw(&self) {
        self.dirty.set(true);
    }
    fn set_visible(&self, visible: bool) -> Result<(), PlatformError> {
        if self.visible.replace(visible) == visible {
            return Ok(());
        }
        self.parent.full_redraw.set(true);
        self.parent.request_redraw();
        if visible {
            let positioner = self.positioner()?;
            let popup = Popup::from_surface(
                None,
                &positioner,
                &self.parent.qh,
                self.surface.clone(),
                &*self.factory.xdg,
            )
            .map_err(error)?;
            self.parent.layer.get_popup(popup.xdg_popup());

            if let Some((seat, serial, time)) = self.factory.last_press.borrow_mut().take() {
                if time.elapsed() < std::time::Duration::from_secs(1) {
                    popup.xdg_popup().grab(&seat, serial);
                }
            }
            popup.xdg_surface().set_window_geometry(
                0,
                0,
                self.requested.get().width.round().max(1.) as i32,
                self.requested.get().height.round().max(1.) as i32,
            );
            positioner.destroy();
            *self.role.borrow_mut() = Some(popup);
            self.surface.commit();
        } else {
            self.surface.attach(None, 0, 0);
            self.surface.commit();
            self.configured.set(false);
            self.role.borrow_mut().take();
        }
        Ok(())
    }
    fn update_window_properties(&self, properties: WindowProperties<'_>) {
        self.set_size(WindowSize::Logical(
            properties.layout_constraints().preferred,
        ));
    }
}
impl PopupHandler for WaylandState {
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        popup: &Popup,
        configure: PopupConfigure,
    ) {
        if let Some(window) = self.popup_for_surface(popup.wl_surface()) {
            window.configured.set(true);
            window.resize(
                configure.width.max(1) as u32,
                configure.height.max(1) as u32,
            );
        }
    }
    fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup) {
        if let Some(window) = self.popup_for_surface(popup.wl_surface()) {
            window.done();
        }
    }
}

impl smithay_client_toolkit::shell::xdg::window::WindowHandler for WaylandState {
    fn request_close(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &smithay_client_toolkit::shell::xdg::window::Window,
    ) {
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &smithay_client_toolkit::shell::xdg::window::Window,
        _: smithay_client_toolkit::shell::xdg::window::WindowConfigure,
        _: u32,
    ) {
    }
}
