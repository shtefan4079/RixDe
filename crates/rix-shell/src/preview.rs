use crate::{PreviewItem, RixPanelWindow, Theme};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};
#[derive(Default)]
struct Hover {
    task: Option<(i32, f32)>,
    over_preview: bool,
    changed: Option<Instant>,
    last_capture: Option<Instant>,
    in_flight: bool,
    next_capture: usize,
}
pub fn install(window: &RixPanelWindow) -> Rc<slint::Timer> {
    let state = Rc::new(RefCell::new(Hover::default()));
    let timer = Rc::new(slint::Timer::default());
    let weak = window.as_weak();

    let tick: Rc<dyn Fn()> = {
        let state = state.clone();
        let weak = weak.clone();
        let timer = timer.clone();
        Rc::new(move || {
            let Some(window) = weak.upgrade() else {
                timer.stop();
                return;
            };
            let mut state = state.borrow_mut();
            'tick: {
                if !window.get_previews_enabled()
                    || window.get_taskbar_drag_active()
                    || window.get_is_app_menu_open()
                    || window.get_is_context_menu_open()
                    || window.get_is_edit_mode()
                    || !window.get_system_popup().is_empty()
                {
                    window.set_preview_visible(false);
                    state.task = None;
                    state.over_preview = false;
                    break 'tick;
                }
                let elapsed = state.changed.map(|time| time.elapsed()).unwrap_or_default();
                let theme = window.global::<Theme>();
                if state.task.is_none() && !state.over_preview && !window.get_preview_pinned() {
                    if elapsed.as_millis() >= theme.get_preview_hide_delay() as u128 {
                        window.set_preview_visible(false);
                    }
                    break 'tick;
                }
                let (id, x) = state
                    .task
                    .unwrap_or((window.get_preview_id(), window.get_preview_anchor()));
                let tasks = window.get_taskbar_items();
                let Some(item) = tasks
                    .iter()
                    .find(|item| {
                        item.is_running
                            && (item.id == id || item.windows.iter().any(|w| w.id == id))
                    })
                    .or_else(|| {

                        (id == window.get_preview_id() && window.get_preview_visible())
                            .then(|| {
                                tasks.iter().find(|item| {
                                    item.windows.iter().any(|member| {
                                        window
                                            .get_preview_items()
                                            .iter()
                                            .any(|old| old.id == member.id)
                                    })
                                })
                            })
                            .flatten()
                    })
                else {
                    window.set_preview_visible(false);
                    break 'tick;
                };
                if !window.get_preview_visible()
                    && elapsed.as_millis() < theme.get_preview_delay() as u128
                {
                    break 'tick;
                }
                let previous = window.get_preview_items();
                let rows: Vec<PreviewItem> = item
                    .windows
                    .iter()
                    .map(|member| {
                        let mut row = previous
                            .iter()
                            .find(|old| old.id == member.id)
                            .unwrap_or_default();
                        row.id = member.id;
                        row.title = member.title;
                        row.active = member.active;
                        row
                    })
                    .collect();
                if rows.is_empty() {
                    window.set_preview_visible(false);
                    break 'tick;
                }
                let membership_changed = rows.len() != previous.row_count()
                    || rows.iter().zip(previous.iter()).any(|(a, b)| a.id != b.id);
                if membership_changed {
                    state.last_capture = None;
                    state.next_capture = 0;
                    window.set_preview_page(0);
                    window.set_preview_items(ModelRc::from(Rc::new(VecModel::from(rows.clone()))));
                } else if let Some(model) =
                    previous.as_any().downcast_ref::<VecModel<PreviewItem>>()
                {
                    for (index, row) in rows.iter().enumerate() {
                        let old = model.row_data(index).unwrap();
                        if old.title != row.title || old.active != row.active {
                            model.set_row_data(index, row.clone());
                        }
                    }
                }
                window.set_preview_id(item.id);
                if state.task.is_some() {
                    state.task = Some((item.id, x));
                }
                window.set_preview_anchor(x);
                window.set_preview_has_audio(item.has_audio);
                window.set_preview_audio_muted(item.audio_muted);
                window.set_preview_audio_volume(item.audio_volume);
                window.set_preview_visible(true);
                if state.in_flight
                    || state.last_capture.is_some_and(|time| {
                        time.elapsed().as_millis() < theme.get_preview_refresh() as u128
                    })
                {
                    break 'tick;
                }
                let capacity = window.get_preview_capacity().max(1) as usize;
                let start = window.get_preview_page().max(0) as usize * capacity;
                let end = (start + capacity).min(rows.len());
                if start >= end {
                    window.set_preview_page(0);
                    break 'tick;
                }
                let id = rows[start + state.next_capture % (end - start)].id;
                state.next_capture += 1;
                state.in_flight = true;

                if state.next_capture.is_multiple_of(end - start) {
                    state.last_capture = Some(Instant::now());
                }

                let width =
                    (theme.get_preview_width() * window.window().scale_factor()).round() as u32;
                let height =
                    (theme.get_preview_height() * window.window().scale_factor()).round() as u32;
                let weak = weak.clone();

                tokio::spawn(async move {
                    let result = tokio::time::timeout(Duration::from_secs(3), async {
                        let mut client = rix_ipc::IpcClient::connect().await?;
                        client
                            .send(rix_ipc::IpcRequest::GetWindowPreview {
                                id: id as u64,
                                width,
                                height,
                            })
                            .await
                    })
                    .await;
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(window) = weak.upgrade() {
                            window.invoke_preview_completed();
                            if !window.get_preview_visible() {
                                return;
                            }
                            let model = window.get_preview_items();
                            let Some(model) =
                                model.as_any().downcast_ref::<VecModel<PreviewItem>>()
                            else {
                                return;
                            };
                            let Some(index) = model.iter().position(|row| row.id == id) else {
                                return;
                            };
                            let mut row = model.row_data(index).unwrap();
                            match result {
                                Ok(Ok(rix_ipc::IpcResponse::WindowPreview {
                                    width,
                                    height,
                                    rgba,
                                    ..
                                })) if rgba.len() == width as usize * height as usize * 4 => {
                                    row.image = slint::Image::from_rgba8_premultiplied(
                                        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                            &rgba, width, height,
                                        ),
                                    );
                                    row.failed = false;
                                }
                                _ => row.failed = true,
                            }
                            model.set_row_data(index, row);
                        }
                    });
                });
            }
            let idle = !window.get_preview_visible()
                && state.task.is_none()
                && !state.over_preview
                && !window.get_preview_pinned()
                && !state.in_flight;
            if idle {
                timer.stop();
            }
        })
    };
    let wake: Rc<dyn Fn()> = {
        let timer = timer.clone();
        let tick = tick.clone();
        Rc::new(move || {
            timer.start(slint::TimerMode::Repeated, Duration::from_millis(50), {
                let tick = tick.clone();
                move || tick()
            });
        })
    };
    let hover = state.clone();
    let wake_hover = wake.clone();
    let weak = window.as_weak();
    window.on_preview_open_group(move |id, x| {
        let mut state = hover.borrow_mut();
        state.task = Some((id, x));
        state.changed = Some(Instant::now() - Duration::from_secs(10));
        if let Some(window) = weak.upgrade() {
            window.set_preview_pinned(true);
        }
        wake_hover();
    });
    let hover = state.clone();
    let wake_hover = wake.clone();
    let weak = window.as_weak();
    window.on_preview_dismiss(move || {
        let mut state = hover.borrow_mut();
        state.task = None;
        state.over_preview = false;
        state.changed = Some(Instant::now());
        if let Some(window) = weak.upgrade() {
            window.set_preview_pinned(false);
            window.set_preview_visible(false);
        }
        wake_hover();
    });
    let hover = state.clone();
    let wake_hover = wake.clone();
    window.on_task_hover(move |id, entered, x| {
        let mut state = hover.borrow_mut();
        if entered {
            state.task = Some((id, x));
        } else if state.task.is_some_and(|task| task.0 == id) {
            state.task = None;
        }
        state.changed = Some(Instant::now());
        wake_hover();
    });
    let hover = state.clone();
    let wake_hover = wake.clone();
    window.on_preview_hover(move |entered| {
        let mut state = hover.borrow_mut();
        state.over_preview = entered;
        state.changed = Some(Instant::now());
        wake_hover();
    });
    let hover = state.clone();
    let wake_hover = wake.clone();
    window.on_preview_completed(move || {
        hover.borrow_mut().in_flight = false;
        wake_hover();
    });
    timer
}
