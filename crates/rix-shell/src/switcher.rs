use crate::{RixPanelWindow, SwitcherItem};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

thread_local! {

    static SWITCHER_TICK: RefCell<Option<Rc<dyn Fn()>>> = const { RefCell::new(None) };
}

fn kick() {
    SWITCHER_TICK.with(|tick| {
        if let Some(tick) = tick.borrow().as_ref() {
            tick();
        }
    });
}

pub fn snapshot(
    window: &RixPanelWindow,
    state: rix_ipc::SwitcherState,
    config: &rix_config::ConfigManager,
    apps: &[crate::apps::AppInfo],
) {
    let settings = config.get_desktop().switcher;
    window.set_switcher_delay(settings.show_delay_ms as i32);
    window.set_switcher_card_width(settings.card_width as f32);
    let is_new_session = !window.get_switcher_requested()
        || window.get_switcher_generation() != state.generation as i32;
    window.set_switcher_generation(state.generation as i32);
    window.set_switcher_total(state.windows.len() as i32);
    if state.windows.is_empty() {
        window.set_switcher_requested(false);
        window.set_switcher_visible(false);
        return;
    }
    if !window.get_switcher_requested() {
        crate::popup::dismiss_all(window);
        window.set_is_edit_mode(false);
    }
    let selected = (state.selected as usize).min(state.windows.len() - 1);
    let screen_capacity = ((window.get_panel_width() as f32 - 64.)
        / (settings.card_width as f32 + 16.))
        .floor()
        .max(1.) as usize;
    let visible = (settings.max_visible as usize)
        .min(screen_capacity)
        .min(state.windows.len());
    let start = selected
        .saturating_sub(visible / 2)
        .min(state.windows.len() - visible);
    let previous: Vec<_> = window.get_switcher_items().iter().collect();
    let rows = state
        .windows
        .iter()
        .skip(start)
        .take(visible)
        .map(|w| {
            let previous = previous.iter().find(|p| p.id as u64 == w.id);
            let app = crate::find_app(apps, &w.app_id);
            let icon = app
                .and_then(|a| crate::apps::load_icon_at_size(&a.icon_name, 48))
                .unwrap_or_default();
            SwitcherItem {
                id: w.id as i32,
                title: w.title.as_str().into(),
                app: app.map(|a| a.name.as_str()).unwrap_or(&w.app_id).into(),
                icon,
                preview: previous.map(|p| p.preview.clone()).unwrap_or_default(),
                loaded: !is_new_session && previous.is_some_and(|p| p.loaded),
                minimized: w.is_minimized,
            }
        })
        .collect::<Vec<_>>();
    window.set_switcher_items(ModelRc::from(Rc::new(VecModel::from(rows))));
    window.set_switcher_selected((selected - start) as i32);
    window.set_switcher_position((selected + 1) as i32);
    window.set_switcher_requested(true);
    kick();
}
pub fn install(window: &RixPanelWindow) -> Rc<slint::Timer> {
    window.on_switcher_choose(|id| {
        crate::fire_and_forget_ipc(rix_ipc::IpcRequest::ChooseSwitcher(
            (id >= 0).then_some(id as u64),
        ))
    });
    let weak = window.as_weak();
    let pending = Rc::new(Cell::new(None::<(i32, Instant)>));
    let last_refresh = Rc::new(Cell::new(None::<Instant>));
    let busy = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let timer = Rc::new(slint::Timer::default());
    let tick: Rc<dyn Fn()> = {
        let timer = timer.clone();
        let pending = pending.clone();
        let last_refresh = last_refresh.clone();
        let busy = busy.clone();
        let weak = weak.clone();
        Rc::new(move || {
            let Some(w) = weak.upgrade() else {
                timer.stop();
                return;
            };
            if !w.get_switcher_requested() {
                pending.set(None);
                last_refresh.set(None);
                timer.stop();
                return;
            }
            let generation = w.get_switcher_generation();
            if pending.get().is_none_or(|(g, _)| g != generation) {
                pending.set(Some((generation, Instant::now())));
                last_refresh.set(Some(Instant::now()));
            }
            if !w.get_switcher_visible()
                && pending.get().unwrap().1.elapsed()
                    >= Duration::from_millis(w.get_switcher_delay().max(0) as u64)
            {
                w.set_switcher_visible(true);
            }
            if busy.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let rows = w.get_switcher_items();
            let count = rows.row_count();
            if count == 0 {
                timer.stop();
                return;
            }
            let selected = (w.get_switcher_selected().max(0) as usize).min(count - 1);
            let target = (0..count)
                .map(|offset| (selected + offset) % count)
                .find_map(|i| rows.row_data(i).filter(|r| !r.loaded).map(|r| (i, r)));

            let (index, mut row) = match target {
                Some(found) => found,
                None => {
                    if w.get_switcher_visible()
                        && last_refresh
                            .get()
                            .is_some_and(|t| t.elapsed() >= Duration::from_millis(1500))
                    {
                        last_refresh.set(Some(Instant::now()));
                        for i in 0..count {
                            if let Some(mut r) = rows.row_data(i) {
                                r.loaded = false;
                                rows.set_row_data(i, r);
                            }
                        }
                    }
                    return;
                }
            };
            row.loaded = true;
            rows.set_row_data(index, row.clone());
            busy.store(true, std::sync::atomic::Ordering::Relaxed);
            let busy = busy.clone();
            let weak = weak.clone();
            let width = w.get_switcher_card_width().round().clamp(160., 400.) as u32;
            tokio::spawn(async move {
                let response = async {
                    let mut client = rix_ipc::IpcClient::connect().await?;
                    client
                        .send(rix_ipc::IpcRequest::GetWindowPreview {
                            id: row.id as u64,
                            width,
                            height: width * 9 / 16,
                        })
                        .await
                }
                .await;
                let _ = weak.upgrade_in_event_loop(move |w| {
                    if !w.get_switcher_requested() || w.get_switcher_generation() != generation {
                        return;
                    }
                    if let Ok(rix_ipc::IpcResponse::WindowPreview {
                        width,
                        height,
                        rgba,
                        ..
                    }) = response
                    {
                        if rgba.len() != width as usize * height as usize * 4 {
                            return;
                        }
                        let rows = w.get_switcher_items();
                        if let Some((index, mut item)) =
                            rows.iter().enumerate().find(|(_, r)| r.id == row.id)
                        {
                            item.preview =
                                slint::Image::from_rgba8_premultiplied(slint::SharedPixelBuffer::<
                                    slint::Rgba8Pixel,
                                >::clone_from_slice(
                                    &rgba, width, height
                                ));
                            rows.set_row_data(index, item);
                        }
                    }
                });
                busy.store(false, std::sync::atomic::Ordering::Relaxed);
            });
        })
    };
    let wake: Rc<dyn Fn()> = {
        let timer = timer.clone();
        let tick = tick.clone();
        Rc::new(move || {
            timer.start(slint::TimerMode::Repeated, Duration::from_millis(16), {
                let tick = tick.clone();
                move || tick()
            });
        })
    };
    SWITCHER_TICK.with(|slot| *slot.borrow_mut() = Some(wake));
    timer
}
