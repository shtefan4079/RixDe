use crate::state::{ClientState, RixState};
use std::{
    collections::HashMap,
    os::{fd::AsFd, unix::net::UnixStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_buffer, wl_compositor, wl_region, wl_registry, wl_shm, wl_shm_pool, wl_subcompositor,
        wl_subsurface, wl_surface,
    },
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
};
use wayland_protocols::xdg::dialog::v1::client::{xdg_dialog_v1, xdg_wm_dialog_v1};
use wayland_protocols::xdg::shell::client::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};
use wayland_protocols_plasma::blur::client::{org_kde_kwin_blur, org_kde_kwin_blur_manager};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

type Geometry = (i32, i32, i32, i32);
#[derive(Default)]
struct Events {
    configures: HashMap<usize, Vec<Geometry>>,
    repositioned: Vec<(usize, u32)>,
    layer_size: Option<(u32, u32)>,
}
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Events {
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
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Events {
    fn event(
        _: &mut Self,
        base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for Events {
    fn event(
        _: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
        }
    }
}
impl Dispatch<xdg_popup::XdgPopup, usize> for Events {
    fn event(
        state: &mut Self,
        _: &xdg_popup::XdgPopup,
        event: xdg_popup::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_popup::Event::Configure {
                x,
                y,
                width,
                height,
            } => state
                .configures
                .entry(*index)
                .or_default()
                .push((x, y, width, height)),
            xdg_popup::Event::Repositioned { token } => state.repositioned.push((*index, token)),
            _ => {}
        }
    }
}
impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for Events {
    fn event(
        state: &mut Self,
        surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            surface.ack_configure(serial);
            state.layer_size = Some((width, height));
        }
    }
}
wayland_client::delegate_noop!(Events: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(Events: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(Events: ignore wl_region::WlRegion);
wayland_client::delegate_noop!(Events: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(Events: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(Events: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(Events: ignore xdg_toplevel::XdgToplevel);
wayland_client::delegate_noop!(Events: ignore xdg_positioner::XdgPositioner);
wayland_client::delegate_noop!(Events: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
wayland_client::delegate_noop!(Events: ignore zxdg_decoration_manager_v1::ZxdgDecorationManagerV1);
wayland_client::delegate_noop!(Events: ignore zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1);
wayland_client::delegate_noop!(Events: ignore wl_subcompositor::WlSubcompositor);
wayland_client::delegate_noop!(Events: ignore wl_subsurface::WlSubsurface);
wayland_client::delegate_noop!(Events: ignore org_kde_kwin_blur_manager::OrgKdeKwinBlurManager);
wayland_client::delegate_noop!(Events: ignore org_kde_kwin_blur::OrgKdeKwinBlur);
wayland_client::delegate_noop!(Events: ignore xdg_wm_dialog_v1::XdgWmDialogV1);
wayland_client::delegate_noop!(Events: ignore xdg_dialog_v1::XdgDialogV1);

type Inspect = Box<dyn FnOnce(&mut RixState) + Send>;
struct Desktop {
    work: mpsc::Sender<Inspect>,
    done: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    globals: wayland_client::globals::GlobalList,
    queue: wayland_client::EventQueue<Events>,
    events: Events,
    compositor: wl_compositor::WlCompositor,
    wm: xdg_wm_base::XdgWmBase,
    layers: zwlr_layer_shell_v1::ZwlrLayerShellV1,
    buffer: wl_buffer::WlBuffer,
    layer_buffer: wl_buffer::WlBuffer,
    _pool: wl_shm_pool::WlShmPool,
}
impl Desktop {
    fn new() -> Self {
        let (server_socket, client_socket) = UnixStream::pair().unwrap();
        let (work, jobs) = mpsc::channel::<Inspect>();
        let done = Arc::new(AtomicBool::new(false));
        let stopped = done.clone();
        let thread = std::thread::spawn(move || {
            let mut display = wayland_server::Display::<RixState>::new().unwrap();
            let mut state = RixState::new(&mut display, "popup-constraint-test".into());
            state
                .seat
                .add_keyboard(Default::default(), 500, 25)
                .unwrap();
            state.seat.add_pointer();
            for (name, pixels, scale, location) in [
                ("left-scaled", (1280, 800), 2, (-640, -100)),
                ("primary", (800, 600), 1, (0, 0)),
            ] {
                let output = smithay::output::Output::new(
                    name.into(),
                    smithay::output::PhysicalProperties {
                        size: (0, 0).into(),
                        subpixel: smithay::output::Subpixel::Unknown,
                        make: "test".into(),
                        model: "test".into(),
                    },
                );
                output.change_current_state(
                    Some(smithay::output::Mode {
                        size: pixels.into(),
                        refresh: 60_000,
                    }),
                    None,
                    Some(smithay::output::Scale::Integer(scale)),
                    Some(location.into()),
                );
                state.space.map_output(&output, location);
            }
            let _client = display
                .handle()
                .insert_client(server_socket, Arc::new(ClientState::default()))
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !stopped.load(Ordering::Relaxed) && Instant::now() < deadline {
                display.dispatch_clients(&mut state).unwrap();
                while let Ok(job) = jobs.try_recv() {
                    job(&mut state);
                }
                display.flush_clients().unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let conn = Connection::from_socket(client_socket).unwrap();
        let (globals, queue) = registry_queue_init::<Events>(&conn).unwrap();
        let qh = queue.handle();
        let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=4, ()).unwrap();
        let wm: xdg_wm_base::XdgWmBase = globals.bind(&qh, 3..=6, ()).unwrap();
        let layers: zwlr_layer_shell_v1::ZwlrLayerShellV1 = globals.bind(&qh, 1..=4, ()).unwrap();
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
        let file = tempfile::tempfile().unwrap();
        file.set_len(360_000).unwrap();
        let pool = shm.create_pool(file.as_fd(), 360_000, &qh, ());
        let buffer = pool.create_buffer(0, 300, 300, 1200, wl_shm::Format::Argb8888, &qh, ());
        let layer_buffer = pool.create_buffer(0, 640, 40, 2560, wl_shm::Format::Argb8888, &qh, ());
        Self {
            work,
            done,
            thread: Some(thread),
            globals,
            queue,
            events: Events::default(),
            compositor,
            wm,
            layers,
            buffer,
            layer_buffer,
            _pool: pool,
        }
    }
    fn roundtrip(&mut self) {
        self.queue.roundtrip(&mut self.events).unwrap();
    }
    fn inspect<T: Send + 'static>(
        &self,
        inspect: impl FnOnce(&mut RixState) -> T + Send + 'static,
    ) -> T {
        let (tx, rx) = mpsc::channel();
        self.work
            .send(Box::new(move |state| {
                tx.send(inspect(state)).unwrap();
            }))
            .unwrap();
        rx.recv_timeout(Duration::from_secs(3)).unwrap()
    }
    fn window(&mut self) -> (wl_surface::WlSurface, xdg_surface::XdgSurface) {
        let qh = self.queue.handle();
        let surface = self.compositor.create_surface(&qh, ());
        let xdg = self.wm.get_xdg_surface(&surface, &qh, ());
        let top = xdg.get_toplevel(&qh, ());
        top.set_title("Popup parent".into());
        surface.commit();
        self.roundtrip();
        xdg.set_window_geometry(5, 7, 80, 60);
        surface.attach(Some(&self.buffer), 0, 0);
        surface.commit();
        self.roundtrip();
        self.inspect(|state| {
            let window = state.space.elements().next().unwrap().clone();
            assert_eq!(window.geometry().loc, (5, 7).into());
            state.space.map_element(window, (-80, 240), false);
        });
        (surface, xdg)
    }
    fn small_window(
        &mut self,
        title: &str,
    ) -> (
        wl_surface::WlSurface,
        xdg_surface::XdgSurface,
        xdg_toplevel::XdgToplevel,
    ) {
        let qh = self.queue.handle();
        let surface = self.compositor.create_surface(&qh, ());
        let xdg = self.wm.get_xdg_surface(&surface, &qh, ());
        let top = xdg.get_toplevel(&qh, ());
        top.set_title(title.into());
        surface.commit();
        self.roundtrip();
        xdg.set_window_geometry(0, 0, 80, 60);
        let input = self.compositor.create_region(&qh, ());
        input.add(0, 0, 80, 60);
        surface.set_input_region(Some(&input));
        input.destroy();
        surface.attach(Some(&self.buffer), 0, 0);
        surface.commit();
        self.roundtrip();
        (surface, xdg, top)
    }
    fn positioner(
        &self,
        size: (i32, i32),
        anchor: Geometry,
        adjustment: xdg_positioner::ConstraintAdjustment,
    ) -> xdg_positioner::XdgPositioner {
        let pos = self.wm.create_positioner(&self.queue.handle(), ());
        pos.set_size(size.0, size.1);
        pos.set_anchor_rect(anchor.0, anchor.1, anchor.2, anchor.3);
        pos.set_anchor(xdg_positioner::Anchor::BottomRight);
        pos.set_gravity(xdg_positioner::Gravity::BottomRight);
        pos.set_constraint_adjustment(adjustment);
        pos
    }
    fn popup(
        &self,
        parent: Option<&xdg_surface::XdgSurface>,
        positioner: &xdg_positioner::XdgPositioner,
        index: usize,
    ) -> (
        wl_surface::WlSurface,
        xdg_surface::XdgSurface,
        xdg_popup::XdgPopup,
    ) {
        let qh = self.queue.handle();
        let surface = self.compositor.create_surface(&qh, ());
        let xdg = self.wm.get_xdg_surface(&surface, &qh, ());
        let popup = xdg.get_popup(parent, positioner, &qh, index);
        (surface, xdg, popup)
    }
    fn geometry(&self, index: usize) -> Geometry {
        *self.events.configures.get(&index).unwrap().last().unwrap()
    }
}
impl Drop for Desktop {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn blur_region_membership(desktop: &Desktop) -> Option<Option<(bool, bool)>> {
    desktop.inspect(|state| {
        let window = state.space.elements().next().unwrap();
        let surface = window.toplevel().unwrap().wl_surface();
        crate::blur::region(surface)
            .map(|region| region.map(|region| (region.contains((1, 1)), region.contains((21, 21)))))
    })
}

#[test]
fn releasing_old_blur_object_keeps_replacement_and_surface_commits_preserve_it() {
    let mut desktop = Desktop::new();
    let (surface, _xdg) = desktop.window();
    let qh = desktop.queue.handle();
    let manager: org_kde_kwin_blur_manager::OrgKdeKwinBlurManager =
        desktop.globals.bind(&qh, 1..=1, ()).unwrap();
    let first_region = desktop.compositor.create_region(&qh, ());
    first_region.add(0, 0, 10, 10);
    let first = manager.create(&surface, &qh, ());
    first.set_region(Some(&first_region));
    first.commit();
    desktop.roundtrip();
    assert_eq!(blur_region_membership(&desktop), Some(Some((true, false))));

    let replacement_region = desktop.compositor.create_region(&qh, ());
    replacement_region.add(20, 20, 10, 10);
    let replacement = manager.create(&surface, &qh, ());
    replacement.set_region(Some(&replacement_region));
    replacement.commit();
    desktop.roundtrip();
    assert_eq!(blur_region_membership(&desktop), Some(Some((false, true))));

    first.release();
    desktop.roundtrip();
    assert_eq!(
        blur_region_membership(&desktop),
        Some(Some((false, true))),
        "Releasing the previous bridge must preserve its committed replacement"
    );
    surface.commit();
    desktop.roundtrip();
    assert_eq!(blur_region_membership(&desktop), Some(Some((false, true))));

    let uncommitted = manager.create(&surface, &qh, ());
    uncommitted.set_region(Some(&first_region));
    uncommitted.release();
    desktop.roundtrip();
    assert_eq!(blur_region_membership(&desktop), Some(Some((false, true))));

    replacement.release();
    desktop.roundtrip();
    assert_eq!(blur_region_membership(&desktop), None);
}

#[test]
fn blur_owner_follows_latest_commit_and_manager_unset_remains_explicit() {
    let mut desktop = Desktop::new();
    let (surface, _xdg) = desktop.window();
    let qh = desktop.queue.handle();
    let manager: org_kde_kwin_blur_manager::OrgKdeKwinBlurManager =
        desktop.globals.bind(&qh, 1..=1, ()).unwrap();
    let first = manager.create(&surface, &qh, ());
    first.set_region(None);
    first.commit();
    let second = manager.create(&surface, &qh, ());
    let second_region = desktop.compositor.create_region(&qh, ());
    second_region.add(20, 20, 10, 10);
    second.set_region(Some(&second_region));
    second.commit();
    first.commit();
    desktop.roundtrip();
    assert_eq!(blur_region_membership(&desktop), Some(None));
    second.release();
    desktop.roundtrip();
    assert_eq!(
        blur_region_membership(&desktop),
        Some(None),
        "The last committer owns material even when its object was created first"
    );

    manager.unset(&surface);
    desktop.roundtrip();
    assert_eq!(blur_region_membership(&desktop), None);
    first.commit();
    desktop.roundtrip();
    assert_eq!(blur_region_membership(&desktop), Some(None));
    first.release();
    desktop.roundtrip();
    assert_eq!(blur_region_membership(&desktop), None);
}

#[test]
fn native_popup_and_subsurface_keep_independent_backdrop_requests() {
    use smithay::wayland::compositor::{with_surface_tree_downward, TraversalAction};
    use smithay::{backend::renderer::element::Id, desktop::PopupManager};
    let mut desktop = Desktop::new();
    let (parent_surface, parent) = desktop.window();
    let qh = desktop.queue.handle();
    let manager: org_kde_kwin_blur_manager::OrgKdeKwinBlurManager =
        desktop.globals.bind(&qh, 1..=1, ()).unwrap();
    let subs: wl_subcompositor::WlSubcompositor = desktop.globals.bind(&qh, 1..=1, ()).unwrap();
    let pos = desktop.positioner(
        (80, 60),
        (20, 20, 1, 1),
        xdg_positioner::ConstraintAdjustment::SlideX,
    );
    let (surface, _xdg, _popup) = desktop.popup(Some(&parent), &pos, 0);
    surface.commit();
    desktop.roundtrip();
    let child = desktop.compositor.create_surface(&qh, ());
    let sub = subs.get_subsurface(&child, &surface, &qh, ());
    sub.set_position(8, 9);
    sub.set_desync();
    let mut materials = Vec::new();
    for surface in [&parent_surface, &surface, &child] {
        let blur = manager.create(surface, &qh, ());
        blur.set_region(None);
        blur.commit();
        materials.push(blur);
    }
    child.attach(Some(&desktop.buffer), 0, 0);
    child.commit();
    surface.attach(Some(&desktop.buffer), 0, 0);
    surface.commit();
    desktop.roundtrip();
    desktop.inspect(|state| {
        let window = state.space.elements().next().unwrap();
        let root = window.toplevel().unwrap().wl_surface();
        let (popup, _) = PopupManager::popups_for_surface(root).next().unwrap();
        let requests = crate::render::collect_blur_requests(state);
        assert_eq!(
            requests.len(),
            3,
            "Root, native popup and child each retain their own material"
        );
        assert!(requests.contains_key(&Id::from_wayland_resource(root)));
        let mut surfaces = Vec::new();
        with_surface_tree_downward(
            popup.wl_surface(),
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |surface, _, _| surfaces.push(Id::from_wayland_resource(surface)),
            |_, _, _| true,
        );
        assert_eq!(
            surfaces.len(),
            2,
            "The real popup includes its independent subsurface"
        );
        assert!(surfaces
            .iter()
            .all(|surface| requests.contains_key(surface)));
    });
    for material in materials {
        material.release();
    }
    desktop.roundtrip();
    assert!(desktop.inspect(|state| crate::render::collect_blur_requests(state).is_empty()));
}

#[test]
fn clicking_app_popup_above_another_windows_titlebar_keeps_its_parent_focused() {
    use smithay::utils::SERIAL_COUNTER;
    use xdg_positioner::ConstraintAdjustment as Adjust;
    let mut desktop = Desktop::new();
    let (_parent_surface, parent, _parent_top) = desktop.small_window("Foreground popup parent");
    let (background_surface, _background, background_top) =
        desktop.small_window("Background window");
    let manager: zxdg_decoration_manager_v1::ZxdgDecorationManagerV1 = desktop
        .globals
        .bind(&desktop.queue.handle(), 1..=1, ())
        .unwrap();
    let _decoration = manager.get_toplevel_decoration(&background_top, &desktop.queue.handle(), ());
    desktop.roundtrip();
    background_surface.commit();
    desktop.roundtrip();
    desktop.inspect(|state| {
        let parent = state.windows.entries[0].1.clone();
        let background = state.windows.entries[1].1.clone();
        state.space.map_element(parent.clone(), (100, 100), false);
        state.space.map_element(background, (180, 150), false);
        state.focus_window(&parent);
    });

    let pos = desktop.positioner((80, 60), (70, 0, 10, 10), Adjust::empty());
    let (popup_surface, popup_xdg, _popup) = desktop.popup(Some(&parent), &pos, 0);
    popup_surface.commit();
    desktop.roundtrip();
    assert_eq!(desktop.geometry(0), (80, 10, 80, 60));
    popup_xdg.set_window_geometry(0, 0, 80, 60);
    let input = desktop
        .compositor
        .create_region(&desktop.queue.handle(), ());
    input.add(0, 0, 80, 60);
    popup_surface.set_input_region(Some(&input));
    input.destroy();
    popup_surface.attach(Some(&desktop.buffer), 0, 0);
    popup_surface.commit();
    desktop.roundtrip();

    let (parent_id, active, popup_focused) = desktop.inspect(|state| {
        let (parent_id, parent) = state.windows.entries[0].clone();
        let background = state.windows.entries[1].1.clone();
        assert!(crate::decoration::enabled(&background));
        let position = (190., 150. - state.decoration_theme.height as f64 / 2.).into();
        let hit = state
            .surface_under(position)
            .expect("The popup receives this point");
        assert!(state.popups.find_popup(&hit.0).is_some());
        assert!(
            state
                .decoration_theme
                .for_window(&background)
                .hit(state.space.element_geometry(&background).unwrap(), position,)
                .is_some(),
            "The same point is above the background titlebar"
        );
        assert!(!state
            .space
            .element_geometry(&parent)
            .unwrap()
            .to_f64()
            .contains(position));
        let pointer = state.seat.get_pointer().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        pointer.motion(
            state,
            Some(hit),
            &smithay::input::pointer::MotionEvent {
                location: position,
                serial,
                time: 0,
            },
        );
        state.pointer_pressed(0x110, serial, 0);
        (
            parent_id,
            state.windows.active,
            pointer.current_focus()
                == Some(
                    smithay::desktop::PopupManager::popups_for_surface(
                        parent.toplevel().unwrap().wl_surface(),
                    )
                    .next()
                    .unwrap()
                    .0
                    .wl_surface()
                    .clone(),
                ),
        )
    });
    assert_eq!(
        active,
        Some(parent_id),
        "Clicking an app popup must not focus the titlebar beneath it"
    );
    assert!(
        popup_focused,
        "The pointer must remain focused on the app popup"
    );
}

#[test]
fn popup_configures_honor_constraints_on_scaled_negative_output_and_nested_parent() {
    use xdg_positioner::ConstraintAdjustment as Adjust;
    let mut desktop = Desktop::new();
    let (_window, parent) = desktop.window();
    let pos = desktop.positioner((160, 100), (50, 40, 10, 10), Adjust::FlipX | Adjust::FlipY);
    let (surface, popup_xdg, _popup) = desktop.popup(Some(&parent), &pos, 0);
    desktop.roundtrip();
    assert!(
        !desktop.events.configures.contains_key(&0),
        "Initial configure must wait for the popup's initial commit"
    );
    surface.commit();
    desktop.roundtrip();
    assert_eq!(
        desktop.geometry(0),
        (-110, -60, 160, 100),
        "Both permitted flips must keep the popup on its 640×400 logical output"
    );
    popup_xdg.set_window_geometry(7, 9, 160, 100);
    surface.attach(Some(&desktop.buffer), 0, 0);
    surface.commit();
    desktop.roundtrip();
    let child_pos = desktop.positioner((120, 80), (150, 80, 10, 10), Adjust::FlipX | Adjust::FlipY);
    let (child, _, _) = desktop.popup(Some(&popup_xdg), &child_pos, 1);
    child.commit();
    desktop.roundtrip();
    assert_eq!(
        desktop.geometry(1),
        (30, 0, 120, 80),
        "Submenu constraints use its parent's configured geometry, including ancestor offsets"
    );
    let slide = desktop.positioner(
        (180, 100),
        (60, 40, 10, 10),
        Adjust::SlideX | Adjust::SlideY,
    );
    let (surface, _, sliding_popup) = desktop.popup(Some(&parent), &slide, 2);
    surface.commit();
    desktop.roundtrip();
    assert_eq!(
        desktop.geometry(2),
        (-100, -40, 180, 100),
        "Output bounds must use the XDG geometry origin, excluding the parent's shadow"
    );
    let huge = desktop.positioner(
        (1000, 1000),
        (60, 40, 10, 10),
        Adjust::SlideX | Adjust::SlideY | Adjust::ResizeX | Adjust::ResizeY,
    );
    sliding_popup.reposition(&huge, 42);
    desktop.roundtrip();
    assert_eq!(desktop.geometry(2), (-560, -340, 640, 400));
    assert_eq!(
        desktop.events.repositioned,
        [(2, 42)],
        "Reposition must acknowledge the client token alongside constrained geometry"
    );
    let none = desktop.positioner((160, 100), (50, 40, 10, 10), Adjust::empty());
    let (surface, _, _) = desktop.popup(Some(&parent), &none, 3);
    surface.commit();
    desktop.roundtrip();
    assert_eq!(
        desktop.geometry(3),
        (60, 50, 160, 100),
        "Compositor must only apply adjustments permitted by the client"
    );
}

#[test]
fn layer_popup_is_constrained_after_its_late_parent_assignment() {
    use xdg_positioner::ConstraintAdjustment as Adjust;
    let mut desktop = Desktop::new();
    let qh = desktop.queue.handle();
    let root = desktop.compositor.create_surface(&qh, ());
    let panel = desktop.layers.get_layer_surface(
        &root,
        None,
        zwlr_layer_shell_v1::Layer::Top,
        "popup-parent".into(),
        &qh,
        (),
    );
    panel.set_size(0, 40);
    panel.set_anchor(
        zwlr_layer_surface_v1::Anchor::Left
            | zwlr_layer_surface_v1::Anchor::Right
            | zwlr_layer_surface_v1::Anchor::Bottom,
    );
    root.commit();
    desktop.roundtrip();
    assert_eq!(desktop.events.layer_size, Some((640, 40)));
    root.attach(Some(&desktop.layer_buffer), 0, 0);
    root.commit();
    desktop.roundtrip();
    let pos = desktop.positioner((160, 100), (610, 20, 10, 10), Adjust::FlipX | Adjust::FlipY);
    let (surface, _, popup) = desktop.popup(None, &pos, 0);
    panel.get_popup(&popup);
    surface.commit();
    desktop.roundtrip();
    assert_eq!(
        desktop.geometry(0),
        (450, -80, 160, 100),
        "The popup's output and parent origin must be known before its only initial configure"
    );
    assert_eq!(desktop.events.configures[&0].len(), 1);
}

#[test]
fn modal_child_blocks_its_parent_but_not_other_windows() {
    let mut desktop = Desktop::new();
    let (_parent_surface, _parent_xdg, parent_top) = desktop.small_window("Modal test parent");
    let (child_surface, _child_xdg, child_top) = desktop.small_window("Modal test child");
    let (_other_surface, _other_xdg, _other_top) = desktop.small_window("Modal test other");
    let qh = desktop.queue.handle();
    let manager: xdg_wm_dialog_v1::XdgWmDialogV1 = desktop.globals.bind(&qh, 1..=1, ()).unwrap();
    let dialog = manager.get_xdg_dialog(&child_top, &qh, ());
    child_top.set_parent(Some(&parent_top));
    child_surface.commit();
    desktop.roundtrip();

    let hit = |state: &RixState, point: (f64, f64)| {
        state
            .surface_under(point.into())
            .map(|(surface, _)| surface)
    };
    let parent_surface = desktop.inspect({
        let point = (40., 30.);
        move |state| {
            let parent = state.windows.entries[0].1.clone();
            let child = state.windows.entries[1].1.clone();
            let other = state.windows.entries[2].1.clone();
            state.space.map_element(parent.clone(), (0, 0), false);
            state.space.map_element(child, (500, 500), false);
            state.space.map_element(other, (100, 100), false);
            let surface = parent.toplevel().unwrap().wl_surface().clone();
            assert_eq!(
                hit(state, point),
                Some(surface.clone()),
                "without modal children the fast path still hits the window"
            );
            surface
        }
    });

    dialog.set_modal();
    child_surface.commit();
    desktop.roundtrip();
    desktop.inspect(move |state| {

        let parent = state.windows.entries[0].1.clone();
        let child = state.windows.entries[1].1.clone();
        let other = state.windows.entries[2].1.clone();
        state.space.map_element(parent, (0, 0), false);
        state.space.map_element(child.clone(), (500, 500), false);
        state.space.map_element(other.clone(), (100, 100), false);
        assert!(
            hit(state, (40., 30.)).is_none(),
            "a modal child blocks its own parent"
        );
        assert_eq!(
            hit(state, (140., 130.)),
            Some(other.toplevel().unwrap().wl_surface().clone()),
            "a modal child must not block unrelated windows"
        );
        assert_eq!(
            hit(state, (540., 530.)),
            Some(child.toplevel().unwrap().wl_surface().clone()),
            "the modal child itself stays hittable"
        );
    });

    dialog.unset_modal();
    child_surface.commit();
    desktop.roundtrip();
    desktop.inspect(move |state| {
        let parent = state.windows.entries[0].1.clone();
        let child = state.windows.entries[1].1.clone();
        state.space.map_element(parent, (0, 0), false);
        state.space.map_element(child, (500, 500), false);
        assert_eq!(
            hit(state, (40., 30.)),
            Some(parent_surface),
            "clearing the modal hint restores the parent"
        );
    });
}

#[test]
fn popup_target_uses_nearest_output_instead_of_a_gap_in_the_layout() {
    use super::popup_target;
    use smithay::{utils::Rectangle, wayland::shell::xdg::PositionerState};
    let positioner = PositionerState {
        anchor_rect: Rectangle::new((400, 200).into(), (1, 1).into()),
        ..Default::default()
    };
    let bounds = [
        Rectangle::new((-640, -100).into(), (640, 400).into()),
        Rectangle::new((1000, 0).into(), (800, 600).into()),
    ];
    assert_eq!(
        popup_target(positioner, (0, 0).into(), bounds.into_iter()),
        Some(bounds[0])
    );
    assert_eq!(
        popup_target(positioner, (0, 0).into(), std::iter::empty()),
        None
    );
}
