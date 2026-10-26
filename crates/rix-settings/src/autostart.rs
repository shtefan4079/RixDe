use crate::{AutostartChoice, AutostartSettings, SettingsWindow};
use rix_services::{
    applications::{scan_applications, AppInfo},
    autostart::AutostartStore,
};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::{
    rc::Rc,
    sync::{Arc, Mutex},
};
fn refresh(window: &SettingsWindow, entries: &[rix_services::autostart::Entry], apps: &[AppInfo]) {
    let global = window.global::<AutostartSettings>();
    let query = global.get_query().to_lowercase();
    global.set_entries(ModelRc::from(Rc::new(VecModel::from(
        entries
            .iter()
            .map(|e| AutostartChoice {
                id: e.id.as_str().into(),
                name: e.name.as_str().into(),
                enabled: e.enabled,
                available: e.available,
                custom: e.custom,
            })
            .collect::<Vec<_>>(),
    ))));
    global.set_apps(ModelRc::from(Rc::new(VecModel::from(
        apps.iter()
            .filter(|app| {
                (query.is_empty()
                    || app.name.to_lowercase().contains(&query)
                    || app.desktop_id.to_lowercase().contains(&query))
                    && !entries
                        .iter()
                        .any(|e| e.id.trim_end_matches(".desktop") == app.desktop_id)
            })
            .map(|app| AutostartChoice {
                id: app.desktop_id.as_str().into(),
                name: app.name.as_str().into(),
                available: true,
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    ))));
}
type Operation = Box<dyn FnOnce(&AutostartStore, &[AppInfo]) -> anyhow::Result<()> + Send>;
pub fn install(window: &SettingsWindow, config: Arc<rix_config::ConfigManager>) {
    let store = Arc::new(AutostartStore::new(config.config_dir()));
    let entries = Arc::new(Mutex::new(store.list().unwrap_or_default()));
    let apps = Arc::new(scan_applications());
    refresh(window, &entries.lock().unwrap(), &apps);
    let weak = window.as_weak();
    let cached = entries.clone();
    let a = apps.clone();
    window.global::<AutostartSettings>().on_search(move |_| {
        if let Some(w) = weak.upgrade() {
            refresh(&w, &cached.lock().unwrap(), &a);
        }
    });
    let dispatch = Rc::new(move |window: &SettingsWindow, operation: Operation| {
        let store = store.clone();
        let apps = apps.clone();
        let cached = entries.clone();
        crate::jobs::run(
            window,
            move || {
                operation(&store, &apps)?;
                Ok((store.list()?, apps))
            },
            move |window, (entries, apps)| {
                refresh(window, &entries, &apps);
                *cached.lock().unwrap() = entries;
                window.global::<AutostartSettings>().set_picker_open(false);
                window.global::<AutostartSettings>().set_error("".into());
                window.set_status_message(crate::i18n::tr("Autostart saved"));
            },
        );
    });
    let weak = window.as_weak();
    let action = dispatch.clone();
    window
        .global::<AutostartSettings>()
        .on_toggle(move |id, enabled| {
            if let Some(w) = weak.upgrade() {
                let id = id.to_string();
                action(
                    &w,
                    Box::new(move |store, _| store.set_enabled(&id, enabled)),
                );
            }
        });
    let weak = window.as_weak();
    let action = dispatch.clone();
    window.global::<AutostartSettings>().on_remove(move |id| {
        if let Some(w) = weak.upgrade() {
            let id = id.to_string();
            action(&w, Box::new(move |store, _| store.remove(&id)));
        }
    });
    let weak = window.as_weak();
    window.global::<AutostartSettings>().on_add(move |id| {
        if let Some(w) = weak.upgrade() {
            let id = id.to_string();
            dispatch(
                &w,
                Box::new(move |store, apps| {
                    let app = apps
                        .iter()
                        .find(|app| app.desktop_id == id)
                        .ok_or_else(|| {
                            anyhow::anyhow!(crate::i18n::tr("Application is unavailable"))
                        })?;
                    store.add(app)
                }),
            );
        }
    });
}
