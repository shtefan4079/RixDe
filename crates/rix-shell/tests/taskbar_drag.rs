use slint::{
    platform::{Key, PointerEventButton, WindowEvent},
    ComponentHandle, LogicalPosition, Model, ModelRc, VecModel,
};
use std::{cell::RefCell, rc::Rc, time::Duration};

slint::slint! {
    import { TaskbarWidget, TaskbarItemData } from "../ui/widgets/taskbar.slint";
    import { Theme } from "../ui/theme.slint";
    export { TaskbarItemData, Theme }
    export component TaskbarProbe inherits Window {
        width: 480px; height: 120px;
        in property <[TaskbarItemData]> items;
        in property <int> content-width;
        in property <length> available-width: 400px;
        in property <bool> show-labels;
        in property <bool> edit-mode;
        in property <int> cancel-generation;
        out property <bool> dragging: bar.drag-active;
        callback activated(int);
        callback started(int);
        callback reordered(int, int);
        callback audio(int);
        callback menu(int);
        public function cancel() { bar.cancel-drag(); }
        bar := TaskbarWidget {
            x: 16px; y: 16px; width: root.available-width; height: 48px;
            items: root.items; content-width: root.content-width;
            show-labels: root.show-labels; is-edit-mode: root.edit-mode;
            cancel-generation: root.cancel-generation;
            item-clicked(id) => { root.activated(id); }
            drag-started(id) => { root.started(id); }
            item-moved(id, index) => { root.reordered(id, index); }
            audio-toggle(id) => { root.audio(id); }
            item-context-menu(id, x, y) => { root.menu(id); }
        }
    }
}

#[derive(Default, Debug)]
struct Events {
    activated: Vec<i32>,
    started: Vec<i32>,
    reordered: Vec<(i32, i32)>,
    audio: Vec<i32>,
    menu: Vec<i32>,
}

fn pump() {
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    slint::platform::update_timers_and_animations();
}

fn fixture(
    ids: &[i32],
    scale: f32,
) -> (
    TaskbarProbe,
    Rc<VecModel<TaskbarItemData>>,
    Rc<RefCell<Events>>,
) {
    let probe = TaskbarProbe::new().unwrap();
    let theme = probe.global::<Theme>();
    theme.set_task_width(48.);
    theme.set_task_label_width(144.);
    theme.set_task_spacing(6.);
    theme.set_task_padding(8.);
    theme.set_panel_icon_size(24.);
    theme.set_icon_size_sm(16.);
    theme.set_spacing_xs(4.);
    theme.set_task_highlight_scale(1.);
    theme.set_task_highlight_radius_scale(1.);
    let items = Rc::new(VecModel::from(
        ids.iter()
            .map(|id| TaskbarItemData {
                id: *id,
                title: format!("App {id}").into(),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    ));
    probe.set_items(ModelRc::from(items.clone()));
    probe.set_content_width((ids.len() as i32 * 54 - 6).max(0));
    let events = Rc::new(RefCell::new(Events::default()));
    let observed = events.clone();
    probe.on_activated(move |id| observed.borrow_mut().activated.push(id));
    let observed = events.clone();
    probe.on_started(move |id| observed.borrow_mut().started.push(id));
    let observed = events.clone();
    probe.on_reordered(move |id, index| observed.borrow_mut().reordered.push((id, index)));
    let observed = events.clone();
    probe.on_audio(move |id| observed.borrow_mut().audio.push(id));
    let observed = events.clone();
    probe.on_menu(move |id| observed.borrow_mut().menu.push(id));
    probe.show().unwrap();
    probe
        .window()
        .dispatch_event(WindowEvent::ScaleFactorChanged {
            scale_factor: scale,
        });
    probe.window().dispatch_event(WindowEvent::Resized {
        size: slint::LogicalSize::new(480., 120.),
    });
    pump();
    (probe, items, events)
}

fn pointer(probe: &TaskbarProbe, kind: &str, x: f32, y: f32) {
    let position = LogicalPosition::new(x, y);
    let event = match kind {
        "down" => WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        },
        "up" => WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        },
        "right-down" => WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Right,
        },
        "right-up" => WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Right,
        },
        _ => WindowEvent::PointerMoved { position },
    };
    probe.window().dispatch_event(event);
    pump();
}

#[test]
fn click_threshold_reorder_both_directions_and_signed_ids_at_all_scales() {
    i_slint_backend_testing::init_no_event_loop();
    for scale in [1., 1.25, 2.] {
        let (probe, _, events) = fixture(&[-1, 22, 33, 44], scale);
        pointer(&probe, "down", 40., 40.);
        pointer(&probe, "move", 45., 43.);
        pointer(&probe, "up", 45., 43.);
        assert_eq!(events.borrow().activated, [-1]);
        assert!(events.borrow().started.is_empty());
        pointer(&probe, "down", 40., 40.);
        pointer(&probe, "move", 170., 40.);
        assert!(probe.get_dragging());
        pointer(&probe, "up", 170., 40.);
        assert!(!probe.get_dragging());
        assert_eq!(events.borrow().reordered, [(-1, 2)]);
        assert_eq!(
            events.borrow().activated,
            [-1],
            "A drag must never launch or minimize at scale {scale}"
        );
        pointer(&probe, "down", 202., 40.);
        pointer(&probe, "move", 18., 40.);
        pointer(&probe, "up", 18., 40.);
        assert_eq!(events.borrow().reordered, [(-1, 2), (44, 0)]);
        probe.hide().unwrap();
    }
}

#[test]
fn escape_vertical_drop_external_cancel_and_deleted_source_never_activate() {
    i_slint_backend_testing::init_no_event_loop();
    let (probe, items, events) = fixture(&[11, 22, 33], 1.25);
    pointer(&probe, "down", 40., 40.);
    pointer(&probe, "move", 150., 40.);
    probe.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Escape.into(),
    });
    pump();
    assert!(!probe.get_dragging());
    pointer(&probe, "up", 150., 40.);
    pointer(&probe, "down", 40., 40.);
    pointer(&probe, "move", 150., 85.);
    pointer(&probe, "up", 150., 85.);
    assert!(!probe.get_dragging());
    pointer(&probe, "down", 40., 40.);
    pointer(&probe, "move", 150., 40.);
    probe.invoke_cancel();
    pointer(&probe, "up", 150., 40.);
    pointer(&probe, "down", 40., 40.);
    pointer(&probe, "move", 150., 40.);
    items.remove(0);
    pump();
    assert!(!probe.get_dragging());
    pointer(&probe, "up", 150., 40.);
    assert!(
        events.borrow().activated.is_empty(),
        "Cancelled captures must swallow release"
    );
    assert!(events.borrow().reordered.is_empty());

    pointer(&probe, "down", 40., 40.);
    probe.set_cancel_generation(1);
    probe.window().dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(40., 40.),
        button: PointerEventButton::Left,
    });
    pump();
    assert!(events.borrow().activated.is_empty());
}

#[test]
fn model_replacement_cancels_representative_changes_but_audio_and_context_still_work() {
    i_slint_backend_testing::init_no_event_loop();
    let (probe, items, events) = fixture(&[11, 22, 33], 1.);
    pointer(&probe, "right-down", 40., 40.);
    pointer(&probe, "right-up", 40., 40.);
    assert_eq!(events.borrow().menu, [11]);
    let mut first = items.row_data(0).unwrap();
    first.has_audio = true;
    items.set_row_data(0, first);
    pump();
    pointer(&probe, "down", 56., 24.);
    pointer(&probe, "up", 56., 24.);
    assert_eq!(events.borrow().audio, [11]);
    assert!(events.borrow().activated.is_empty());
    pointer(&probe, "down", 30., 40.);
    pointer(&probe, "move", 150., 40.);
    let mut first = items.row_data(0).unwrap();
    first.id = 99;
    items.set_row_data(0, first);
    pump();
    assert!(!probe.get_dragging());
    pointer(&probe, "up", 150., 40.);
    assert!(events.borrow().reordered.is_empty());
    probe.set_edit_mode(true);
    pump();
    pointer(&probe, "down", 30., 40.);
    pointer(&probe, "move", 150., 40.);
    pointer(&probe, "up", 150., 40.);
    assert_eq!(
        events.borrow().started,
        [11],
        "Edit mode must reserve drags for the whole panel widget"
    );
    assert!(events.borrow().activated.is_empty());
}

#[test]
fn labelled_tasks_use_actual_midpoints_and_same_slot_drop_has_no_action() {
    i_slint_backend_testing::init_no_event_loop();
    let (probe, items, events) = fixture(&[11, 22, 33, 44], 2.);
    let mut second = items.row_data(1).unwrap();
    second.is_active = true;
    items.set_row_data(1, second);
    probe.set_show_labels(true);
    probe.set_content_width(306);
    pump();

    pointer(&probe, "down", 40., 40.);
    pointer(&probe, "move", 120., 40.);
    pointer(&probe, "up", 120., 40.);
    assert!(events.borrow().reordered.is_empty());
    assert!(events.borrow().activated.is_empty());
    pointer(&probe, "down", 40., 40.);
    pointer(&probe, "move", 160., 40.);
    pointer(&probe, "up", 160., 40.);
    assert_eq!(events.borrow().reordered, [(11, 1)]);
    pointer(&probe, "down", 240., 40.);
    pointer(&probe, "move", 120., 40.);
    pointer(&probe, "up", 120., 40.);
    assert_eq!(events.borrow().reordered, [(11, 1), (33, 1)]);
}

#[test]
fn narrow_taskbar_edge_scroll_reaches_last_app_and_clamps_after_model_shrinks() {
    i_slint_backend_testing::init_no_event_loop();
    let (probe, items, events) = fixture(&[11, 22, 33, 44, 55, 66], 1.25);
    probe.set_available_width(120.);
    pump();
    pointer(&probe, "down", 40., 40.);
    pointer(&probe, "move", 130., 40.);
    for _ in 0..20 {
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(40));
        slint::platform::update_timers_and_animations();
    }
    pointer(&probe, "up", 130., 40.);
    assert_eq!(
        events.borrow().reordered,
        [(11, 5)],
        "Held edge must expose the entire overflowing row"
    );
    for _ in 0..5 {
        items.remove(1);
    }
    probe.set_content_width(48);
    pump();
    pointer(&probe, "down", 40., 40.);
    pointer(&probe, "up", 40., 40.);
    assert_eq!(
        events.borrow().activated,
        [11],
        "A smaller catalog must reset an obsolete viewport offset"
    );
}

#[test]
fn final_motion_and_release_in_one_frame_commit_after_pointer_capture_is_cleared() {
    i_slint_backend_testing::init_no_event_loop();
    let (probe, items, events) = fixture(&[11, 22, 33, 44], 1.);
    let weak = probe.as_weak();
    let observed = events.clone();
    let reordered_items = items.clone();
    probe.on_reordered(move |id, index| {
        assert!(
            !weak.upgrade().unwrap().get_dragging(),
            "Model updates must see completed drag state"
        );
        observed.borrow_mut().reordered.push((id, index));
        let row = reordered_items.row_data(0).unwrap();
        reordered_items.remove(0);
        reordered_items.insert(index as usize, row);
    });
    pointer(&probe, "down", 40., 40.);
    for event in [
        WindowEvent::PointerMoved {
            position: LogicalPosition::new(170., 40.),
        },
        WindowEvent::PointerReleased {
            position: LogicalPosition::new(170., 40.),
            button: PointerEventButton::Left,
        },
    ] {
        probe.window().dispatch_event(event);
    }
    pump();
    assert_eq!(events.borrow().reordered, [(11, 2)]);
    assert!(events.borrow().activated.is_empty());
    assert_eq!(items.row_data(2).unwrap().id, 11);

    pointer(&probe, "down", 40., 40.);
    pointer(&probe, "up", 40., 40.);
    assert_eq!(events.borrow().activated, [22]);
}
