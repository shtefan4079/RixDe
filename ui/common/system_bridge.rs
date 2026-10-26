#[path = "network_bridge.rs"]
mod network_bridge;
pub fn refresh_network_icons(window: &SystemHost, theme: &str) {
    ICON_CACHE.with(|cache| cache.borrow_mut().clear());
    network_bridge::refresh_icons(window, theme);
}
use crate::{AudioRow, CalendarDay, KeyboardRow, SystemHost, SystemState};
use chrono::{Datelike, Duration, Local, NaiveDate};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::{cell::RefCell, rc::Rc};
fn model<T: Clone + 'static>(rows: Vec<T>) -> ModelRc<T> {
    ModelRc::from(Rc::new(VecModel::from(rows)))
}
fn reconcile<T: Clone + PartialEq + 'static>(
    current: ModelRc<T>,
    rows: Vec<T>,
    key: impl Fn(&T) -> slint::SharedString,
) -> ModelRc<T> {
    if current.row_count() == rows.len()
        && current
            .iter()
            .zip(&rows)
            .all(|(old, new)| key(&old) == key(new))
    {
        if let Some(model) = current.as_any().downcast_ref::<VecModel<T>>() {
            for (index, row) in rows.into_iter().enumerate() {
                if model.row_data(index).as_ref() != Some(&row) {
                    model.set_row_data(index, row);
                }
            }
            return current;
        }
    }
    model(rows)
}
thread_local! { static ICON_CACHE: RefCell<std::collections::HashMap<(String,String),slint::Image>> = RefCell::new(Default::default()); }
fn icon(name: &str, theme: &str) -> slint::Image {
    ICON_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let key = (theme.to_owned(), name.to_owned());
        cache
            .entry(key)
            .or_insert_with(|| {
                rix_assets::IconResolver::default()
                    .resolve(name, theme, 32, None)
                    .and_then(|p| slint::Image::load_from_path(&p).ok())
                    .unwrap_or_default()
            })
            .clone()
    })
}
pub fn apply_keyboard(window: &SystemHost, state: rix_ipc::KeyboardState) {
    let global = window.global::<SystemState>();
    global.set_layout_label(
        state
            .layouts
            .get(state.active as usize)
            .map(|l| {
                if l.label.is_empty() {
                    l.layout.to_uppercase()
                } else {
                    l.label.clone()
                }
            })
            .unwrap_or_default()
            .into(),
    );
    global.set_layouts(model(
        state
            .layouts
            .into_iter()
            .enumerate()
            .map(|(index, l)| KeyboardRow {
                label: if l.label.is_empty() {
                    l.layout.to_uppercase()
                } else {
                    l.label
                }
                .into(),
                name: if l.variant.is_empty() {
                    l.layout
                } else {
                    format!("{} · {}", l.layout, l.variant)
                }
                .into(),
                active: index == state.active as usize,
            })
            .collect(),
    ));
}
pub fn install(window: &SystemHost, config: std::sync::Arc<rix_config::ConfigManager>) {
    window
        .global::<SystemState>()
        .set_maximum_volume(config.get_layout().services.maximum_volume as i32);
    window
        .global::<SystemState>()
        .set_week_start(config.get_layout().services.week_start as i32);
    let audio = rix_services::audio::start(config.clone());
    let global = window.global::<SystemState>();
    let commands = audio.commands.clone();
    global.on_audio_volume(move |key, value| {
        let _ = commands.try_send(rix_services::audio::Action::Volume(
            key.into(),
            value.max(0) as u32,
        ));
    });
    let commands = audio.commands.clone();
    global.on_audio_mute(move |key, value| {
        let _ = commands.try_send(rix_services::audio::Action::Mute(key.into(), value));
    });
    global.on_audio_default(move |key| {
        let _ = audio
            .commands
            .try_send(rix_services::audio::Action::Default(key.into()));
    });
    let weak = window.as_weak();
    let cfg = config.clone();
    tokio::spawn(async move {
        let mut updates = audio.updates;
        while updates.changed().await.is_ok() {
            let snapshot = updates.borrow_and_update().clone();
            let weak = weak.clone();
            let theme = cfg.get_theme().icon_theme;
            let _ = slint::invoke_from_event_loop(move || {
                let Some(window) = weak.upgrade() else { return };
                let global = window.global::<SystemState>();
                let default = snapshot
                    .nodes
                    .iter()
                    .find(|n| n.kind == "sink" && n.default);
                global.set_audio_icon(icon(
                    if default.is_some_and(|n| n.muted || n.volume == 0) {
                        "audio-volume-muted"
                    } else {
                        "audio-volume-high"
                    },
                    &theme,
                ));
                global.set_audio_error(crate::i18n::tr(&snapshot.error));
                global.set_audio(reconcile(
                    global.get_audio(),
                    snapshot
                        .nodes
                        .into_iter()
                        .map(|n| AudioRow {
                            app_id: n.app_id.into(),
                            binary: n.binary.into(),
                            key: n.key.into(),
                            name: n.description.into(),
                            icon: icon(
                                if n.muted {
                                    if n.kind == "source" || n.kind == "source-output" {
                                        "microphone-sensitivity-muted"
                                    } else {
                                        "audio-volume-muted"
                                    }
                                } else {
                                    &n.icon
                                },
                                &theme,
                            ),
                            kind: n.kind.into(),
                            volume: n.volume as i32,
                            muted: n.muted,
                            preferred: n.default,
                        })
                        .collect(),
                    |row| row.key.clone(),
                ));
                global.invoke_audio_changed();
            });
        }
    });
    network_bridge::install(window, config.clone());
    let weak = window.as_weak();
    global.on_keyboard_select(move |index| {
        let weak = weak.clone();
        tokio::spawn(async move {
            if let Ok(mut client) = rix_ipc::IpcClient::connect().await {
                if let Ok(rix_ipc::IpcResponse::Keyboard(state)) = client
                    .send(rix_ipc::IpcRequest::SetKeyboardGroup(index as u32))
                    .await
                {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(window) = weak.upgrade() {
                            apply_keyboard(&window, state);
                        }
                    });
                }
            }
        });
    });
    let weak = window.as_weak();
    tokio::spawn(async move {
        if let Ok(mut client) = rix_ipc::IpcClient::connect().await {
            if let Ok(rix_ipc::IpcResponse::Keyboard(state)) =
                client.send(rix_ipc::IpcRequest::GetKeyboard).await
            {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = weak.upgrade() {
                        apply_keyboard(&window, state);
                    }
                });
            }
        }
    });
    install_calendar(window);
}
fn install_calendar(window: &SystemHost) {
    let date = Local::now().date_naive();
    let state = Rc::new(RefCell::new((date.with_day(1).unwrap(), date)));
    calendar(window, date, date);
    let weak = window.as_weak();
    let current = state.clone();
    window
        .global::<SystemState>()
        .on_calendar_month(move |delta| {
            let mut dates = current.borrow_mut();
            let month_index = dates.0.year() * 12 + dates.0.month0() as i32 + delta;
            if let Some(date) = NaiveDate::from_ymd_opt(
                month_index.div_euclid(12),
                month_index.rem_euclid(12) as u32 + 1,
                1,
            ) {
                dates.0 = date;
            }
            if let Some(window) = weak.upgrade() {
                calendar(&window, dates.0, dates.1);
            }
        });
    let weak = window.as_weak();
    let current = state.clone();
    window
        .global::<SystemState>()
        .on_calendar_select(move |date| {
            if let Ok(date) = NaiveDate::parse_from_str(date.as_str(), "%Y-%m-%d") {
                let mut dates = current.borrow_mut();
                dates.1 = date;
                if let Some(window) = weak.upgrade() {
                    calendar(&window, dates.0, dates.1);
                }
            }
        });
    let weak = window.as_weak();
    window.global::<SystemState>().on_calendar_today(move || {
        let today = Local::now().date_naive();
        *state.borrow_mut() = (today.with_day(1).unwrap(), today);
        if let Some(window) = weak.upgrade() {
            calendar(&window, today, today);
        }
    });
}
fn calendar(window: &SystemHost, month: NaiveDate, selected: NaiveDate) {
    let today = Local::now().date_naive();
    let first = month.with_day(1).unwrap();
    let week_start = window
        .global::<SystemState>()
        .get_week_start()
        .rem_euclid(7);
    let start = first
        - Duration::days(
            (first.weekday().num_days_from_monday() as i32 - week_start).rem_euclid(7) as i64,
        );
    let months = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let global = window.global::<SystemState>();
    global.set_month(
        format!(
            "{} {}",
            crate::i18n::tr(months[month.month0() as usize]),
            month.year()
        )
        .into(),
    );
    global.set_today_date(
        format!(
            "{} {}",
            crate::i18n::tr("Today ·"),
            today.format("%d.%m.%Y")
        )
        .into(),
    );
    global.set_selected_date(selected.format("%d.%m.%Y").to_string().into());
    global.set_weekdays(model(
        (0..7)
            .map(|i| {
                let day = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
                    [((i + week_start) % 7) as usize];
                crate::i18n::tr(day)
            })
            .collect(),
    ));
    global.set_days(model(
        (0..42)
            .map(|i| {
                let date = start + Duration::days(i);
                CalendarDay {
                    text: date.day().to_string().into(),
                    date: date.to_string().into(),
                    today: date == today,
                    current: date.month() == month.month(),
                    selected: date == selected,
                }
            })
            .collect(),
    ));
}
