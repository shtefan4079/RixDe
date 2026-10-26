use crate::{AppInfo, AudioRow, RixPanelWindow, SystemState, TaskbarItemData};
use slint::{ComponentHandle, Model};
fn matches(item: &TaskbarItemData, row: &AudioRow, apps: &[AppInfo]) -> bool {
    if !item.is_running || row.kind != "sink-input" {
        return false;
    }
    [&row.app_id, &row.binary]
        .into_iter()
        .filter(|id| !id.is_empty())
        .any(|id| {
            match (
                crate::find_app(apps, item.app_id.as_str()),
                crate::find_app(apps, id.as_str()),
            ) {
                (Some(task), Some(stream)) => task.desktop_id == stream.desktop_id,
                _ => item
                    .app_id
                    .eq_ignore_ascii_case(id.trim_end_matches(".desktop")),
            }
        })
}
fn streams(window: &RixPanelWindow, id: i32, apps: &[AppInfo]) -> Vec<AudioRow> {
    let Some(item) = window
        .get_taskbar_items()
        .iter()
        .find(|i| i.id == id || i.windows.iter().any(|w| w.id == id))
    else {
        return Vec::new();
    };
    window
        .global::<SystemState>()
        .get_audio()
        .iter()
        .filter(|row| matches(&item, row, apps))
        .collect()
}
pub fn refresh(window: &RixPanelWindow, apps: &[AppInfo]) {
    let rows: Vec<_> = window.global::<SystemState>().get_audio().iter().collect();
    let model = window.get_taskbar_items();
    for (index, mut item) in model.iter().enumerate() {
        let matching: Vec<_> = rows
            .iter()
            .filter(|row| matches(&item, row, apps))
            .collect();
        let has_audio = !matching.is_empty();
        let audio_muted = has_audio && matching.iter().all(|r| r.muted);
        let audio_volume = matching.iter().map(|r| r.volume).max().unwrap_or(0);
        if window.get_preview_id() == item.id {
            window.set_preview_has_audio(has_audio);
            window.set_preview_audio_muted(audio_muted);
            window.set_preview_audio_volume(audio_volume);
        }

        if item.has_audio == has_audio
            && item.audio_muted == audio_muted
            && item.audio_volume == audio_volume
        {
            continue;
        }
        item.has_audio = has_audio;
        item.audio_muted = audio_muted;
        item.audio_volume = audio_volume;
        model.set_row_data(index, item);
    }
}
pub fn install(window: &RixPanelWindow, apps: impl Into<crate::apps::ApplicationCatalog>) {
    let apps = apps.into();
    let weak = window.as_weak();
    let all = apps.clone();
    window.global::<SystemState>().on_audio_changed(move || {
        if let Some(w) = weak.upgrade() {
            refresh(&w, &all.snapshot());
        }
    });
    let weak = window.as_weak();
    let all = apps.clone();
    window.on_task_audio_toggle(move |id| {
        if let Some(w) = weak.upgrade() {
            let rows = streams(&w, id, &all.snapshot());
            let mute = !rows.iter().all(|r| r.muted);
            for row in rows {
                w.global::<SystemState>().invoke_audio_mute(row.key, mute);
            }
        }
    });
    let weak = window.as_weak();
    window.on_task_audio_volume(move |id, value| {
        if let Some(w) = weak.upgrade() {
            for row in streams(&w, id, &apps.snapshot()) {
                w.global::<SystemState>()
                    .invoke_audio_volume(row.key, value);
            }
        }
    });
}
