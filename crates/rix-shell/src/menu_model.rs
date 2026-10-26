use crate::{
    apps::{get_active_icon_theme, resolve_icon_at_size, AppInfo},
    AppEntryData, RixPanelWindow,
};
use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use std::{
    collections::HashMap,
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    time::Duration,
};
use tokio::sync::mpsc;
static GENERATION: AtomicU64 = AtomicU64::new(0);
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);
type Cache = HashMap<(String, String, u32), Option<rix_assets::Tile>>;
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
static REQUESTS: OnceLock<mpsc::Sender<MenuRequest>> = OnceLock::new();

pub(crate) const DEBOUNCE: Duration = Duration::from_millis(120);

pub(crate) struct MenuRequest {
    sequence: u64,
    pub(crate) apps: Vec<AppInfo>,
    pub(crate) category: String,
    pub(crate) query: String,
}

impl MenuRequest {
    pub(crate) fn new(apps: Vec<AppInfo>, category: String, query: String) -> Self {
        Self {
            sequence: REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1,
            apps,
            category,
            query,
        }
    }

    fn superseded(&self) -> bool {
        REQUEST_SEQUENCE.load(Ordering::Relaxed) > self.sequence
    }
}

pub fn invalidate_icons() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
    if let Some(cache) = CACHE.get() {
        cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }
}

pub(crate) fn install(window: &RixPanelWindow) {
    let (sender, receiver) = mpsc::channel(16);
    let weak = window.as_weak();
    let _ = REQUESTS.set(sender);
    tokio::spawn(async move {
        run_menu_requests(
            receiver,
            DEBOUNCE,
            |request| request.superseded(),
            move |request| {
                let weak = weak.clone();
                let _ = weak.upgrade_in_event_loop(move |window| {
                    crate::apply_menu_results(
                        &window,
                        &request.apps,
                        &request.category,
                        &request.query,
                    );
                });
            },
        )
        .await;
    });
}

pub(crate) fn enqueue(request: MenuRequest) -> bool {
    REQUESTS
        .get()
        .is_some_and(|requests| requests.try_send(request).is_ok())
}

pub(crate) async fn run_menu_requests(
    mut requests: mpsc::Receiver<MenuRequest>,
    delay: Duration,
    superseded: impl Fn(&MenuRequest) -> bool,
    mut emit: impl FnMut(MenuRequest),
) {
    while let Some(mut pending) = requests.recv().await {
        loop {
            match tokio::time::timeout(delay, requests.recv()).await {
                Ok(Some(newer)) => pending = newer,
                Ok(None) => {
                    if !superseded(&pending) {
                        emit(pending);
                    }
                    return;
                }
                Err(_) => break,
            }
        }
        if !superseded(&pending) {
            emit(pending);
        }
    }
}

pub fn set_results(window: &RixPanelWindow, apps: Vec<AppInfo>) {
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    let previous: Vec<AppEntryData> = window.get_apps().iter().collect();
    let mut rows = text_results(&apps);

    let mut pending_icons = Vec::new();
    for (index, app) in apps.iter().enumerate() {
        match previous.iter().find(|row| row.exec == app.desktop_id) {
            Some(old) if old.has_icon => {
                rows[index].icon = old.icon.clone();
                rows[index].has_icon = true;
            }
            _ => pending_icons.push((index, app.clone())),
        }
    }
    apply_rows(window, rows);
    if pending_icons.is_empty() {
        return;
    }

    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let weak = window.as_weak();
    let theme = get_active_icon_theme();
    let size = (window.global::<crate::Theme>().get_app_icon_size()
        * window.window().scale_factor())
    .round()
    .clamp(1., 512.) as u32;
    tokio::task::spawn_blocking(move || {
        for (index, app) in pending_icons {
            if GENERATION.load(Ordering::Relaxed) != generation {
                break;
            }
            let key = (theme.clone(), app.icon_name.clone(), size);
            let cache = CACHE.get_or_init(Default::default);
            let cached = cache
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(&key)
                .cloned();
            let icon = cached.unwrap_or_else(|| {
                let tile = resolve_icon_at_size(&app.icon_name, size)
                    .and_then(|path| rix_assets::rasterize_icon(&path, size).ok());
                let mut cache = cache.lock().unwrap_or_else(|error| error.into_inner());
                if cache.len() > 1024 {
                    cache.clear();
                }
                cache.insert(key, tile.clone());
                tile
            });
            let Some(tile) = icon else { continue };
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if GENERATION.load(Ordering::Relaxed) != generation {
                    return;
                }
                let Some(window) = weak.upgrade() else { return };
                let model = window.get_apps();
                let Some(model) = model.as_any().downcast_ref::<VecModel<AppEntryData>>() else {
                    return;
                };
                if let Some(mut row) = model.row_data(index) {
                    row.icon = Image::from_rgba8_premultiplied(
                        SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                            &tile.pixels,
                            tile.width,
                            tile.height,
                        ),
                    );
                    row.has_icon = true;
                    model.set_row_data(index, row);
                }
            });
        }
    });
}

fn apply_rows(window: &RixPanelWindow, rows: Vec<AppEntryData>) {
    let current = window.get_apps();
    if let Some(model) = current.as_any().downcast_ref::<VecModel<AppEntryData>>() {
        if model.row_count() == rows.len() && model.iter().zip(&rows).all(|(old, new)| old == *new)
        {
            return;
        }
        while model.row_count() > rows.len() {
            model.remove(model.row_count() - 1);
        }
        for (index, row) in rows.into_iter().enumerate() {
            if index < model.row_count() {
                let old = model.row_data(index).unwrap();
                if old != row {
                    model.set_row_data(index, row);
                }
            } else {
                model.push(row);
            }
        }
    } else {
        window.set_apps(ModelRc::from(Rc::new(VecModel::from(rows))));
    }
}

fn text_results(apps: &[AppInfo]) -> Vec<AppEntryData> {
    apps.iter()
        .map(|app| AppEntryData {
            name: app.name.as_str().into(),
            exec: app.desktop_id.as_str().into(),
            badge: app.name.chars().next().unwrap_or(' ').to_string().into(),
            icon: Image::default(),
            has_icon: false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn dbus_entries_without_exec_keep_distinct_launch_keys_in_results_and_favorites() {
        let apps: Vec<_> = ["com.example.First", "com.example.Second"]
            .into_iter()
            .map(|id| AppInfo {
                desktop_file: format!("/unused/{id}.desktop").into(),
                name: id.into(),
                desktop_id: id.into(),
                exec: String::new(),
                icon_name: String::new(),
                icon_path: None,
                categories: Vec::new(),
                startup_wm_class: String::new(),
            })
            .collect();
        for rows in [
            text_results(&apps),
            crate::apps::convert_apps_to_menu_data(&apps, 32),
        ] {
            assert_ne!(rows[0].exec, rows[1].exec);
            for (row, expected) in rows.iter().zip(&apps) {
                let actual = crate::apps::find_app(&apps, row.exec.as_str()).unwrap();
                assert_eq!(actual.desktop_file, expected.desktop_file);
            }
        }
        assert!(crate::apps::find_app(&apps, "").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn request_bursts_collapse_into_one_rebuild_of_the_newest_query() {
        let (sender, receiver) = mpsc::channel(8);
        let emitted = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorded = emitted.clone();
        let worker = tokio::spawn(run_menu_requests(
            receiver,
            Duration::from_millis(40),
            |_| false,
            move |request| {
                recorded
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(request.query);
            },
        ));
        for query in ["a", "ab", "abc"] {
            sender
                .send(MenuRequest::new(vec![], "all".into(), query.into()))
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            *emitted.lock().unwrap_or_else(|error| error.into_inner()),
            ["abc"]
        );
        drop(sender);
        worker.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn requests_superseded_by_an_immediate_apply_are_not_replayed() {
        let (sender, receiver) = mpsc::channel(8);
        let emitted = Arc::new(Mutex::new(0usize));
        let recorded = emitted.clone();

        let newest = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let observed = newest.clone();
        let worker = tokio::spawn(run_menu_requests(
            receiver,
            Duration::from_millis(20),
            move |request| request.sequence < observed.load(Ordering::SeqCst),
            move |_| *recorded.lock().unwrap() += 1,
        ));
        let stale = MenuRequest {
            sequence: 1,
            apps: vec![],
            category: "all".into(),
            query: "a".into(),
        };

        newest.store(2, Ordering::SeqCst);
        sender.send(stale).await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(*emitted.lock().unwrap(), 0);
        drop(sender);
        worker.await.unwrap();
    }
}
