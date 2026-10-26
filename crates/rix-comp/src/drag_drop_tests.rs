use super::TestDesktop;
use crate::state::ClientState;
use rix_protocols::capture_client::{rix_capture_policy_manager_v1, rix_capture_policy_surface_v1};
use std::{
    io::{Read, Write},
    os::{fd::AsFd, unix::net::UnixStream},
    sync::Arc,
    time::Duration,
};
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_data_device, wl_data_device_manager,
        wl_data_offer, wl_data_source, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
        wl_surface, wl_touch,
    },
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols::xdg::{
    dialog::v1::client::{xdg_dialog_v1, xdg_wm_dialog_v1},
    foreign::zv2::client::{
        zxdg_exported_v2, zxdg_exporter_v2, zxdg_imported_v2, zxdg_importer_v2,
    },
};

const MIME: &str = "text/uri-list";
const PAYLOAD: &[u8] = b"file:///tmp/dnd-fixture.txt\r\n";
type Action = wl_data_device_manager::DndAction;

#[derive(Default)]
struct Events {
    foreign_handle: Option<String>,
    parent_destroyed: usize,
    press_serial: Option<u32>,
    entered: Option<wl_data_offer::WlDataOffer>,
    dropped: usize,
    cancelled: usize,
    performed: usize,
    finished: usize,
    preferred: Option<Action>,
    negotiated: Vec<Action>,
    offered: Vec<String>,
    frames: usize,
}
wayland_client::delegate_noop!(Events: ignore zxdg_exporter_v2::ZxdgExporterV2);
wayland_client::delegate_noop!(Events: ignore zxdg_importer_v2::ZxdgImporterV2);
wayland_client::delegate_noop!(Events: ignore xdg_wm_dialog_v1::XdgWmDialogV1);
wayland_client::delegate_noop!(Events: ignore xdg_dialog_v1::XdgDialogV1);
impl Dispatch<zxdg_exported_v2::ZxdgExportedV2, ()> for Events {
    fn event(
        state: &mut Self,
        _: &zxdg_exported_v2::ZxdgExportedV2,
        event: zxdg_exported_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_exported_v2::Event::Handle { handle } = event {
            state.foreign_handle = Some(handle);
        }
    }
}
impl Dispatch<zxdg_imported_v2::ZxdgImportedV2, ()> for Events {
    fn event(
        state: &mut Self,
        _: &zxdg_imported_v2::ZxdgImportedV2,
        event: zxdg_imported_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_imported_v2::Event::Destroyed = event {
            state.parent_destroyed += 1;
        }
    }
}
wayland_client::delegate_noop!(Events: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(Events: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(Events: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(Events: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(Events: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(Events: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(Events: ignore wl_data_device_manager::WlDataDeviceManager);
wayland_client::delegate_noop!(Events: ignore xdg_toplevel::XdgToplevel);
wayland_client::delegate_noop!(Events: ignore rix_capture_policy_manager_v1::RixCapturePolicyManagerV1);
wayland_client::delegate_noop!(Events: ignore rix_capture_policy_surface_v1::RixCapturePolicySurfaceV1);
impl Dispatch<wl_callback::WlCallback, ()> for Events {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.frames += 1;
        }
    }
}
impl Dispatch<wl_touch::WlTouch, ()> for Events {
    fn event(
        state: &mut Self,
        _: &wl_touch::WlTouch,
        event: wl_touch::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_touch::Event::Down { serial, .. } = event {
            state.press_serial = Some(serial);
        }
    }
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
        wm: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for Events {
    fn event(
        _: &mut Self,
        xdg: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg.ack_configure(serial);
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for Events {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_pointer::Event::Button {
            serial,
            state: WEnum::Value(wl_pointer::ButtonState::Pressed),
            ..
        } = event
        {
            state.press_serial = Some(serial);
        }
    }
}
impl Dispatch<wl_data_source::WlDataSource, ()> for Events {
    fn event(
        state: &mut Self,
        _: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                assert_eq!(mime_type, MIME);
                std::fs::File::from(fd).write_all(PAYLOAD).unwrap();
            }
            wl_data_source::Event::Cancelled => state.cancelled += 1,
            wl_data_source::Event::DndDropPerformed => state.performed += 1,
            wl_data_source::Event::DndFinished => state.finished += 1,
            wl_data_source::Event::Action {
                dnd_action: WEnum::Value(action),
            } => state.negotiated.push(action),
            _ => {}
        }
    }
}
impl Dispatch<wl_data_offer::WlDataOffer, ()> for Events {
    fn event(
        state: &mut Self,
        _: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_offer::Event::Offer { mime_type } => state.offered.push(mime_type),
            wl_data_offer::Event::Action {
                dnd_action: WEnum::Value(action),
            } => state.negotiated.push(action),
            _ => {}
        }
    }
}
impl Dispatch<wl_data_device::WlDataDevice, ()> for Events {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::Enter {
                serial,
                id: Some(offer),
                ..
            } => {
                if let Some(preferred) = state.preferred {
                    offer.accept(serial, Some(MIME.into()));
                    if offer.version() >= 3 {
                        offer.set_actions(Action::Copy | Action::Move, preferred);
                    }
                } else {
                    offer.accept(serial, None);
                    if offer.version() >= 3 {
                        offer.set_actions(Action::empty(), Action::empty());
                    }
                }
                state.entered = Some(offer);
            }
            wl_data_device::Event::Drop => state.dropped += 1,
            _ => {}
        }
    }
    wayland_client::event_created_child!(Events, wl_data_device::WlDataDevice, [0 => (wl_data_offer::WlDataOffer, ())]);
}

struct Client {
    connection: Connection,
    queue: wayland_client::EventQueue<Events>,
    events: Events,
    surface: wl_surface::WlSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    icon: wl_surface::WlSurface,
    manager: wl_data_device_manager::WlDataDeviceManager,
    device: wl_data_device::WlDataDevice,
    seat: wl_seat::WlSeat,
}
impl Client {
    fn new(desktop: &TestDesktop, app: &str) -> Self {
        Self::new_version(desktop, app, 3)
    }
    fn new_version(desktop: &TestDesktop, app: &str, version: u32) -> Self {
        let (server, client) = UnixStream::pair().unwrap();
        desktop.inspect(move |state| {
            state
                .display_handle
                .insert_client(server, Arc::new(ClientState::default()))
                .unwrap();
        });
        let connection = Connection::from_socket(client).unwrap();
        let (globals, mut queue) = registry_queue_init::<Events>(&connection).unwrap();
        let qh = queue.handle();
        let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=4, ()).unwrap();
        let wm: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=1, ()).unwrap();
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
        let seat: wl_seat::WlSeat = globals.bind(&qh, 5..=7, ()).unwrap();
        let _pointer = seat.get_pointer(&qh, ());
        let manager: wl_data_device_manager::WlDataDeviceManager =
            globals.bind(&qh, version..=version, ()).unwrap();
        let device = manager.get_data_device(&seat, &qh, ());
        let surface = compositor.create_surface(&qh, ());
        let xdg = wm.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg.get_toplevel(&qh, ());
        toplevel.set_app_id(app.into());
        surface.commit();
        let mut events = Events::default();
        queue.roundtrip(&mut events).unwrap();
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&[0, 0, 255, 255].repeat(10000)).unwrap();
        let pool = shm.create_pool(file.as_fd(), 40000, &qh, ());
        let buffer = pool.create_buffer(0, 100, 100, 400, wl_shm::Format::Argb8888, &qh, ());
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        let icon = compositor.create_surface(&qh, ());
        icon.attach(Some(&buffer), 0, 0);
        icon.commit();
        queue.roundtrip(&mut events).unwrap();
        Self {
            connection,
            queue,
            events,
            surface,
            toplevel,
            icon,
            manager,
            device,
            seat,
        }
    }
    fn roundtrip(&mut self) {
        self.queue.roundtrip(&mut self.events).unwrap();
    }
    fn exclude(
        &mut self,
        surface: &wl_surface::WlSurface,
    ) -> rix_capture_policy_surface_v1::RixCapturePolicySurfaceV1 {
        let (globals, mut queue) = registry_queue_init::<Events>(&self.connection).unwrap();
        let manager: rix_capture_policy_manager_v1::RixCapturePolicyManagerV1 =
            globals.bind(&queue.handle(), 1..=1, ()).unwrap();
        let policy = manager.get_surface(surface, &queue.handle(), ());
        policy.set_policy(rix_capture_policy_surface_v1::Policy::Exclude, 1);
        surface.commit();
        queue.roundtrip(&mut self.events).unwrap();
        policy
    }
}

fn map_fixture(desktop: &TestDesktop) {
    desktop.inspect(|state| {
        for (_, window) in state.windows.entries.clone() {
            let (_, app) = crate::state::get_toplevel_info(window.toplevel().unwrap());
            let location = if app.as_deref() == Some("org.rix.DragSourceFixture") {
                (50, 100)
            } else {
                (300, 100)
            };
            state.space.map_element(window, location, false);
        }
        state.space.refresh();
    });
}

fn start_drag(desktop: &TestDesktop, source: &mut Client) -> wl_data_source::WlDataSource {
    motion(desktop, 70., 130.);
    button(desktop, true);
    source.roundtrip();
    let drag_source = source
        .manager
        .create_data_source(&source.queue.handle(), ());
    drag_source.offer(MIME.into());
    if source.manager.version() >= 3 {
        drag_source.set_actions(Action::Copy | Action::Move);
    }
    source.device.start_drag(
        Some(&drag_source),
        &source.surface,
        Some(&source.icon),
        source.events.press_serial.unwrap(),
    );
    source.roundtrip();
    drag_source
}

fn motion(desktop: &TestDesktop, x: f64, y: f64) {
    desktop.inspect(move |state| {
        let pointer = state.seat.get_pointer().unwrap();
        let location = (x, y).into();
        pointer.motion(
            state,
            state.surface_under(location),
            &smithay::input::pointer::MotionEvent {
                location,
                serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                time: 10,
            },
        );
        pointer.frame(state);
    });
}
fn button(desktop: &TestDesktop, pressed: bool) {
    desktop.inspect(move |state| {
        let pointer = state.seat.get_pointer().unwrap();
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        pointer.button(
            state,
            &smithay::input::pointer::ButtonEvent {
                serial,
                time: 10,
                button: 0x110,
                state: if pressed {
                    smithay::backend::input::ButtonState::Pressed
                } else {
                    smithay::backend::input::ButtonState::Released
                },
            },
        );
        if pressed {
            state.pointer_pressed(0x110, serial, 10);
        } else {
            state.pointer_released(0x110);
        }
        pointer.frame(state);
    });
}

#[test]
fn separate_clients_negotiate_transfer_and_finish_copy_and_move() {
    for action in [Action::Copy, Action::Move] {
        exercise_drag(Some(action), false);
    }
}

#[test]
fn foreign_picker_parent_centers_and_modal_input_stays_with_its_family() {
    for close_parent in [false, true] {
        let desktop = TestDesktop::new();
        desktop.inspect(|state| {
            state.seat.add_pointer();
        });
        let mut parent = Client::new(&desktop, "org.rix.PickerParentFixture");
        let mut picker = Client::new(&desktop, "org.rix.PickerFixture");
        let _unrelated = Client::new(&desktop, "org.rix.UnrelatedFixture");
        desktop.inspect(|state| {
            for (_, window) in state.windows.entries.clone() {
                let (_, app) = crate::state::get_toplevel_info(window.toplevel().unwrap());
                let position = if app.as_deref() == Some("org.rix.PickerParentFixture") {
                    (100, 100)
                } else {
                    (650, 120)
                };
                state.space.map_element(window, position, false);
            }
        });
        let file = tempfile::tempfile().unwrap();
        file.set_len(1024).unwrap();
        let (globals, _) = registry_queue_init::<Events>(&picker.connection).unwrap();
        let qh = picker.queue.handle();
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
        let pool = shm.create_pool(file.as_fd(), 1024, &qh, ());
        let buffer = pool.create_buffer(0, 16, 16, 64, wl_shm::Format::Argb8888, &qh, ());
        picker.surface.attach(Some(&buffer), 0, 0);
        picker.surface.commit();
        picker.roundtrip();
        let (globals, _) = registry_queue_init::<Events>(&parent.connection).unwrap();
        let exporter: zxdg_exporter_v2::ZxdgExporterV2 =
            globals.bind(&parent.queue.handle(), 1..=1, ()).unwrap();
        let exported = exporter.export_toplevel(&parent.surface, &parent.queue.handle(), ());
        parent.roundtrip();
        let handle = parent.events.foreign_handle.clone().unwrap();
        let (globals, _) = registry_queue_init::<Events>(&picker.connection).unwrap();
        let importer: zxdg_importer_v2::ZxdgImporterV2 = globals.bind(&qh, 1..=1, ()).unwrap();
        let imported = importer.import_toplevel(handle.clone(), &qh, ());
        motion(&desktop, 110., 145.);
        imported.set_parent_of(&picker.surface);
        let manager: xdg_wm_dialog_v1::XdgWmDialogV1 = globals.bind(&qh, 1..=1, ()).unwrap();
        let dialog = manager.get_xdg_dialog(&picker.toplevel, &qh, ());
        dialog.set_modal();
        picker.surface.commit();
        picker.roundtrip();
        desktop.inspect(|state| {
            let by_app = |app: &str| {
                state
                    .windows
                    .entries
                    .iter()
                    .find(|(_, window)| {
                        crate::state::get_toplevel_info(window.toplevel().unwrap())
                            .1
                            .as_deref()
                            == Some(app)
                    })
                    .unwrap()
                    .1
                    .clone()
            };
            let parent = by_app("org.rix.PickerParentFixture");
            let picker = by_app("org.rix.PickerFixture");
            let unrelated = by_app("org.rix.UnrelatedFixture");
            assert_eq!(
                picker.toplevel().unwrap().parent(),
                Some(parent.toplevel().unwrap().wl_surface().clone())
            );
            let parent_bounds = state.space.element_geometry(&parent).unwrap();
            let child_body = state.space.element_geometry(&picker).unwrap();
            let child_bounds = if crate::decoration::enabled(&picker) {
                state.decoration_for_window(&picker).bounds(child_body)
            } else {
                child_body
            };
            assert!(
                (parent_bounds.loc.x + parent_bounds.size.w / 2
                    - child_bounds.loc.x
                    - child_bounds.size.w / 2)
                    .abs()
                    <= 1
            );
            assert!(
                (parent_bounds.loc.y + parent_bounds.size.h / 2
                    - child_bounds.loc.y
                    - child_bounds.size.h / 2)
                    .abs()
                    <= 1
            );
            assert_eq!(state.modal_child(&parent), Some(picker.clone()));
            state.focus_window(&parent);
            assert_eq!(
                state.seat.get_keyboard().unwrap().current_focus(),
                Some(picker.toplevel().unwrap().wl_surface().clone())
            );
            assert!(
                state.surface_under((110., 145.).into()).is_none(),
                "A parent received input behind its modal picker"
            );
            assert!(
                state.seat.get_pointer().unwrap().current_focus().is_none(),
                "Mapping a modal picker kept stale parent pointer focus"
            );
            state.pointer_pressed(0x110, smithay::utils::SERIAL_COUNTER.next_serial(), 50);
            assert!(
                state.selection_start.is_none(),
                "Clicking a blocked parent started desktop selection"
            );
            state.focus_window(&unrelated);
            assert_eq!(
                state.seat.get_keyboard().unwrap().current_focus(),
                Some(unrelated.toplevel().unwrap().wl_surface().clone()),
                "The picker blocked another application"
            );
        });
        dialog.unset_modal();
        picker.roundtrip();
        desktop.inspect(|state| {
            let parent = state
                .windows
                .entries
                .iter()
                .find(|(_, window)| {
                    crate::state::get_toplevel_info(window.toplevel().unwrap())
                        .1
                        .as_deref()
                        == Some("org.rix.PickerParentFixture")
                })
                .unwrap()
                .1
                .clone();
            assert!(state.modal_child(&parent).is_none());
            assert_eq!(
                state.seat.get_pointer().unwrap().current_focus(),
                Some(parent.toplevel().unwrap().wl_surface().clone())
            );
            state.focus_window(&parent);
            assert_eq!(
                state.seat.get_keyboard().unwrap().current_focus(),
                Some(parent.toplevel().unwrap().wl_surface().clone())
            );
        });
        if close_parent {
            parent.surface.destroy();
        } else {
            exported.destroy();
        }
        parent.roundtrip();
        picker.roundtrip();
        assert_eq!(picker.events.parent_destroyed, 1);
        let stale = importer.import_toplevel(handle, &qh, ());
        picker.roundtrip();
        assert_eq!(
            picker.events.parent_destroyed, 2,
            "A revoked handle could still be imported"
        );
        stale.destroy();
        imported.destroy();
        dialog.destroy();
    }
}

#[test]
#[ignore = "Requires a surfaceless EGL renderer"]
fn drag_drop_motion_retains_gpu_icon_only_locally_and_reduced_motion_and_lock_clear_it() {
    use smithay::backend::{
        egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay},
        renderer::{
            damage::OutputDamageTracker, element::Element, gles::GlesRenderer, Bind, ExportMem,
            Offscreen,
        },
    };
    use std::cell::RefCell;
    thread_local! { static GPU: RefCell<Option<(GlesRenderer, crate::render::SceneRenderer)>> = const { RefCell::new(None) }; }
    let desktop = TestDesktop::new();
    desktop.inspect(|state| {
        state.seat.add_pointer();
        state.animation_duration = 160;
    });
    let mut source = Client::new(&desktop, "org.rix.DragSourceFixture");
    let mut target = Client::new(&desktop, "org.rix.DragTargetFixture");
    target.events.preferred = Some(Action::Copy);
    map_fixture(&desktop);
    let _source = start_drag(&desktop, &mut source);
    motion(&desktop, 320., 130.);
    target.roundtrip();
    target.roundtrip();
    source.roundtrip();
    let icon = desktop.inspect(|state| {
        smithay::backend::renderer::element::Id::from_wayland_resource(
            state.drag.icon.as_ref().unwrap(),
        )
    });
    let render = move |state: &mut crate::state::RixState| {
        GPU.with(|gpu| {
            let mut gpu = gpu.borrow_mut();
            let (renderer, scene) = gpu.get_or_insert_with(|| {
                let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
                let context = EGLContext::new(&display).unwrap();
                (
                    unsafe { GlesRenderer::new(context) }.unwrap(),
                    Default::default(),
                )
            });
            let output = state.space.outputs().next().unwrap().clone();
            let effects = state.decoration_theme.material_effects();
            let displayed = scene
                .compose(renderer, state, &output, None, &effects)
                .unwrap();
            let captured = scene
                .compose_capture(renderer, state, &output, None, &effects)
                .unwrap();
            let icons: Vec<_> = displayed
                .iter()
                .filter(|element| element.id() == &icon)
                .collect();
            let mut buffer: smithay::backend::renderer::gles::GlesTexture = renderer
                .create_buffer(
                    smithay::backend::allocator::Fourcc::Abgr8888,
                    (1000, 700).into(),
                )
                .unwrap();
            let mut framebuffer = renderer.bind(&mut buffer).unwrap();
            OutputDamageTracker::new((1000, 700), 1., smithay::utils::Transform::Normal)
                .render_output(renderer, &mut framebuffer, 0, &icons, [0., 0., 0., 1.])
                .unwrap();
            let mapping = renderer
                .copy_framebuffer(
                    &framebuffer,
                    smithay::utils::Rectangle::new((370, 180).into(), (1, 1).into()),
                    smithay::backend::allocator::Fourcc::Abgr8888,
                )
                .unwrap();
            let red = renderer.map_texture(&mapping).unwrap()[0];
            (
                icons
                    .iter()
                    .map(|element| (element.geometry(1.0.into()), element.alpha()))
                    .collect::<Vec<_>>(),
                captured
                    .iter()
                    .filter(|element| element.id() == &icon)
                    .count(),
                red,
            )
        })
    };
    let initial = desktop.inspect(render.clone());
    assert_eq!(initial.0.len(), 1);
    assert_eq!(initial.1, 1);
    assert!(
        initial.2 > 200,
        "The native drag icon did not render its actual pixels"
    );
    assert!(
        initial.0[0].0.loc.x >= 320 && initial.0[0].0.loc.x <= 324,
        "Pickup animation lagged behind the pointer"
    );
    button(&desktop, false);
    source.roundtrip();
    target.roundtrip();
    desktop.inspect(|state| {
        let finish = state.drag.finished.as_mut().unwrap();
        assert!(finish.accepted);
        finish.ended = std::time::Instant::now() - Duration::from_millis(70);
    });
    let dropped = desktop.inspect(render.clone());
    assert_eq!(
        dropped.0.len(),
        1,
        "Dropping discarded the icon before its transition"
    );
    assert!(
        dropped.0[0].1 > 0.15 && dropped.0[0].1 < 0.75,
        "The dissolve did not update its actual alpha"
    );
    assert_eq!(dropped.1, 0, "A retained drag snapshot leaked into capture");
    assert!(
        dropped.2 > 30 && dropped.2 < initial.2 - 30,
        "The GPU dissolve did not blend the retained pixels"
    );
    desktop.inspect(|state| {
        state.animation_duration = 0;
    });
    assert!(
        desktop.inspect(render.clone()).0.is_empty(),
        "Reduced motion retained the animation"
    );
    desktop.inspect(|state| {
        state.animation_duration = 160;
        state.lock.active = true;
    });
    assert!(
        desktop.inspect(render).0.is_empty(),
        "A drag snapshot remained visible on lock"
    );
}
#[test]
fn rejecting_target_cancels_instead_of_performing_drop() {
    exercise_drag(None, false);
}
#[test]
fn escape_cancels_a_negotiated_drag_and_removes_its_icon() {
    exercise_drag(Some(Action::Move), true);
}

#[test]
fn legacy_clients_can_copy_without_v3_actions_or_finish_requests() {
    for versions in [(3, 1), (3, 2), (1, 3), (2, 3), (1, 1), (2, 2)] {
        exercise_drag_versions(Some(Action::Copy), false, versions);
    }
}

#[test]
fn destroying_the_source_cancels_an_offer_and_removes_drag_visuals() {
    let desktop = TestDesktop::new();
    desktop.inspect(|state| {
        state.seat.add_pointer();
    });
    let mut source = Client::new(&desktop, "org.rix.DragSourceFixture");
    let mut target = Client::new(&desktop, "org.rix.DragTargetFixture");
    target.events.preferred = Some(Action::Move);
    map_fixture(&desktop);
    let drag_source = start_drag(&desktop, &mut source);
    motion(&desktop, 320., 130.);
    target.roundtrip();
    target.roundtrip();
    drag_source.destroy();
    source.roundtrip();
    button(&desktop, false);
    target.roundtrip();
    assert_eq!(target.events.dropped, 0);
    desktop.inspect(|state| {
        assert!(!state.drag.active);
        assert!(state.drag.icon.is_none());
    });
}

#[test]
fn locking_input_cancels_a_negotiated_offer_instead_of_dropping_it() {
    let desktop = TestDesktop::new();
    desktop.inspect(|state| {
        state.seat.add_pointer();
    });
    let mut source = Client::new(&desktop, "org.rix.DragSourceFixture");
    let mut target = Client::new(&desktop, "org.rix.DragTargetFixture");
    target.events.preferred = Some(Action::Move);
    map_fixture(&desktop);
    let _drag_source = start_drag(&desktop, &mut source);
    motion(&desktop, 320., 130.);
    target.roundtrip();
    target.roundtrip();
    desktop.inspect(|state| {
        state.cancel_input();
        state.lock.active = true;
    });
    target.roundtrip();
    source.roundtrip();
    assert_eq!(target.events.dropped, 0);
    assert_eq!(source.events.performed, 0);
    assert_eq!(source.events.cancelled, 1);
    desktop.inspect(|state| {
        assert!(!state.drag.active);
        assert!(state.drag.icon.is_none());
    });
}

#[test]
fn modifier_changes_renegotiate_the_action_without_pointer_movement() {
    let desktop = TestDesktop::new();
    desktop.inspect(|state| {
        state.seat.add_pointer();
    });
    let mut source = Client::new(&desktop, "org.rix.DragSourceFixture");
    let mut target = Client::new(&desktop, "org.rix.DragTargetFixture");
    target.events.preferred = Some(Action::Copy);
    map_fixture(&desktop);
    let _drag_source = start_drag(&desktop, &mut source);
    motion(&desktop, 320., 130.);
    target.roundtrip();
    target.roundtrip();
    source.roundtrip();
    assert_eq!(source.events.negotiated.last(), Some(&Action::Copy));
    for (code, pressed, action) in [
        (50u32, true, Action::Move),
        (37, true, Action::Copy),
        (37, false, Action::Move),
        (50, false, Action::Copy),
    ] {
        desktop.inspect(move |state| {
            state.handle_keyboard_key(
                code.into(),
                if pressed {
                    smithay::backend::input::KeyState::Pressed
                } else {
                    smithay::backend::input::KeyState::Released
                },
                smithay::utils::SERIAL_COUNTER.next_serial(),
                20,
                false,
            )
        });
        source.roundtrip();
        target.roundtrip();
        assert_eq!(source.events.negotiated.last(), Some(&action));
        assert_eq!(target.events.negotiated.last(), Some(&action));
    }
    desktop.inspect(|state| state.cancel_drag());
}

#[test]
fn cancelling_a_touch_drag_invalidates_the_negotiated_drop() {
    let desktop = TestDesktop::new();
    desktop.inspect(|state| {
        state.seat.add_touch();
    });
    let mut source = Client::new(&desktop, "org.rix.DragSourceFixture");
    let mut target = Client::new(&desktop, "org.rix.DragTargetFixture");
    let _touch = source.seat.get_touch(&source.queue.handle(), ());
    source.roundtrip();
    target.events.preferred = Some(Action::Move);
    map_fixture(&desktop);
    desktop.inspect(|state| {
        let touch = state.seat.get_touch().unwrap();
        let location = (70., 130.).into();
        touch.down(
            state,
            state.surface_under(location),
            &smithay::input::touch::DownEvent {
                slot: Some(0).into(),
                location,
                serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                time: 10,
            },
        );
        touch.frame(state);
    });
    source.roundtrip();
    let drag_source = source
        .manager
        .create_data_source(&source.queue.handle(), ());
    drag_source.offer(MIME.into());
    drag_source.set_actions(Action::Copy | Action::Move);
    source.device.start_drag(
        Some(&drag_source),
        &source.surface,
        Some(&source.icon),
        source.events.press_serial.unwrap(),
    );
    source.roundtrip();
    desktop.inspect(|state| {
        assert_eq!(state.drag.touch_slot, Some(Some(0).into()));
        let touch = state.seat.get_touch().unwrap();
        let location = (320., 130.).into();
        state.drag.touch_location = Some(location);
        touch.motion(
            state,
            state.surface_under(location),
            &smithay::input::touch::MotionEvent {
                slot: Some(0).into(),
                location,
                time: 20,
            },
        );
    });
    target.roundtrip();
    target.roundtrip();
    source.roundtrip();
    assert_eq!(source.events.negotiated.last(), Some(&Action::Move));
    desktop.inspect(|state| state.cancel_drag());
    source.roundtrip();
    target.roundtrip();
    assert_eq!(source.events.cancelled, 1);
    assert_eq!(source.events.performed, 0);
    assert_eq!(target.events.dropped, 0);
    desktop.inspect(|state| {
        assert!(!state.drag.active);
        assert!(state.drag.icon.is_none());
    });
}

#[test]
#[ignore = "Requires a surfaceless EGL renderer"]
fn drag_icon_renders_at_pointer_and_protected_icons_never_enter_capture() {
    use smithay::backend::{
        egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay},
        renderer::{element::Element, gles::GlesRenderer},
    };
    use std::cell::RefCell;
    thread_local! { static GPU: RefCell<Option<(GlesRenderer, crate::render::SceneRenderer)>> = const { RefCell::new(None) }; }
    for protect_origin in [false, true] {
        let desktop = TestDesktop::new();
        desktop.inspect(|state| {
            state.seat.add_pointer();
        });
        let mut source = Client::new(&desktop, "org.rix.DragSourceFixture");
        map_fixture(&desktop);
        let _drag_source = start_drag(&desktop, &mut source);
        motion(&desktop, 420., 230.);
        let _frame = source.icon.frame(&source.queue.handle(), ());
        source.icon.commit();
        source.roundtrip();
        desktop.inspect(|state| {
            let output = state.space.outputs().next().unwrap().clone();
            state.send_frame_callbacks(&output, Duration::from_millis(40));
        });
        source.roundtrip();
        assert_eq!(
            source.events.frames, 1,
            "Animated drag icons must receive frame callbacks"
        );
        let render = |state: &mut crate::state::RixState| {
            GPU.with(|gpu| {
                let mut gpu = gpu.borrow_mut();
                let (renderer, scene) = gpu.get_or_insert_with(|| {
                    let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
                    let context = EGLContext::new(&display).unwrap();
                    (
                        unsafe { GlesRenderer::new(context) }.unwrap(),
                        Default::default(),
                    )
                });
                let output = state.space.outputs().next().unwrap().clone();
                let displayed = crate::drag_drop::icon_elements(renderer, state, &output, false);
                let captured = crate::drag_drop::icon_elements(renderer, state, &output, true);
                if let Some(icon) = state.drag.icon.as_ref() {
                    let id = smithay::backend::renderer::element::Id::from_wayland_resource(icon);
                    let effects = state.decoration_theme.material_effects();
                    let display_scene = scene
                        .compose(renderer, state, &output, None, &effects)
                        .unwrap();
                    let capture_scene = scene
                        .compose_capture(renderer, state, &output, None, &effects)
                        .unwrap();
                    assert_eq!(
                        display_scene
                            .iter()
                            .filter(|element| element.id() == &id)
                            .count(),
                        displayed.len()
                    );
                    assert_eq!(
                        capture_scene
                            .iter()
                            .filter(|element| element.id() == &id)
                            .count(),
                        captured.len()
                    );
                }
                (
                    displayed
                        .iter()
                        .map(|element| element.geometry(1.0.into()))
                        .collect::<Vec<_>>(),
                    captured.len(),
                )
            })
        };
        let before = desktop.inspect(render);
        assert_eq!(before.0.len(), 1);
        assert_eq!(before.0[0].loc, (420, 230).into());
        assert_eq!(before.1, 1);
        let surface = if protect_origin {
            source.surface.clone()
        } else {
            source.icon.clone()
        };
        let _policy = source.exclude(&surface);
        let after = desktop.inspect(render);
        assert_eq!(
            after.0.len(),
            1,
            "Protected icons must remain visible locally"
        );
        assert_eq!(after.1, 0, "A protected icon or source leaked into capture");
        desktop.inspect(|state| state.cancel_drag());
        assert!(desktop.inspect(render).0.is_empty());
    }
}

fn exercise_drag(preferred: Option<Action>, escape: bool) {
    exercise_drag_versions(preferred, escape, (3, 3));
}

fn exercise_drag_versions(preferred: Option<Action>, escape: bool, versions: (u32, u32)) {
    let desktop = TestDesktop::new();
    desktop.inspect(|state| {
        state.seat.add_pointer();
    });
    let mut source = Client::new_version(&desktop, "org.rix.DragSourceFixture", versions.0);
    let mut target = Client::new_version(&desktop, "org.rix.DragTargetFixture", versions.1);
    target.events.preferred = preferred;
    map_fixture(&desktop);
    let drag_source = start_drag(&desktop, &mut source);
    desktop.inspect(|state| {
        assert!(state.drag.active);
        assert!(state.drag.icon.is_some());
        assert!(state.drag.origin.is_some());
        assert!(state.selection_start.is_none());
    });
    motion(&desktop, 320., 130.);
    target.roundtrip();
    target.roundtrip();
    source.roundtrip();
    assert!(target.events.offered.iter().any(|mime| mime == MIME));
    if let Some(action) = preferred.filter(|_| versions.0 >= 3) {
        assert_eq!(source.events.negotiated.last(), Some(&action));
    }
    if escape {
        desktop.inspect(|state| {
            state.handle_keyboard_key(
                9u32.into(),
                smithay::backend::input::KeyState::Pressed,
                smithay::utils::SERIAL_COUNTER.next_serial(),
                20,
                false,
            )
        });
    } else {
        button(&desktop, false);
    }
    target.roundtrip();
    source.roundtrip();
    desktop.inspect(|state| {
        assert!(!state.drag.active);
        assert!(state.drag.icon.is_none());
    });
    if preferred.is_none() || escape {
        assert_eq!(source.events.cancelled, 1);
        assert_eq!(source.events.performed, 0);
        assert_eq!(target.events.dropped, 0);
    } else {
        assert_eq!(source.events.performed, usize::from(versions.0 >= 3));
        assert_eq!(source.events.cancelled, 0);
        assert_eq!(target.events.dropped, 1);
        let (mut reader, writer) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let offer = target.events.entered.as_ref().unwrap();
        offer.receive(MIME.into(), writer.as_fd());
        drop(writer);
        target.connection.flush().unwrap();
        source.roundtrip();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, PAYLOAD);
        if versions.1 >= 3 {
            offer.finish();
        } else {
            offer.destroy();
        }
        target.roundtrip();
        source.roundtrip();
        assert_eq!(source.events.finished, usize::from(versions.0 >= 3));
        assert_eq!(source.events.cancelled, 0);
    }
    assert!(drag_source.is_alive());
}
