use crate::{KeyboardSettings, LayoutChoice, SettingsWindow};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::{rc::Rc, sync::Arc};
const OPTIONS: [&str; 6] = [
    "grp:alt_shift_toggle",
    "grp:ctrl_shift_toggle",
    "grp:win_space_toggle",
    "grp:caps_toggle",
    "grp:ralt_toggle",
    "",
];
fn rows(rows: Vec<LayoutChoice>) -> ModelRc<LayoutChoice> {
    ModelRc::from(Rc::new(VecModel::from(rows)))
}
fn choices(catalog: &[rix_services::keyboard::Layout], query: &str) -> Vec<LayoutChoice> {
    let query = query.to_lowercase();
    catalog
        .iter()
        .filter(|l| {
            query.is_empty()
                || l.name.to_lowercase().contains(&query)
                || l.code.contains(&query)
                || l.variant.contains(&query)
        })
        .map(|l| LayoutChoice {
            code: l.code.as_str().into(),
            variant: l.variant.as_str().into(),
            name: l.name.as_str().into(),
            label: l.code.to_uppercase().into(),
        })
        .collect()
}
pub fn install(window: &SettingsWindow, config: Arc<rix_config::ConfigManager>) {
    let catalog = Arc::new(match rix_services::keyboard::catalog() {
        Ok(catalog) => catalog,
        Err(error) => {
            window.global::<KeyboardSettings>().set_error(
                format!(
                    "{}: {error}",
                    crate::i18n::tr("Could not read the XKB layout catalog")
                )
                .into(),
            );
            Vec::new()
        }
    });
    sync(window, &config.get_layout().keyboard, &catalog);
    let global = window.global::<KeyboardSettings>();
    let weak = window.as_weak();
    let layouts = catalog.clone();
    global.on_search(move |query| {
        if let Some(window) = weak.upgrade() {
            window
                .global::<KeyboardSettings>()
                .set_catalog(rows(choices(&layouts, query.as_str())));
        }
    });
    let weak = window.as_weak();
    let layouts = catalog.clone();
    global.on_add(move |code, variant| {
        let Some(window) = weak.upgrade() else { return };
        let global = window.global::<KeyboardSettings>();
        let mut active: Vec<_> = global.get_active().iter().collect();
        if active.len() >= 4 {
            global.set_error(crate::i18n::tr(
                "XKB supports up to four layouts in one group",
            ));
            return;
        }
        if active
            .iter()
            .any(|l| l.code == code.as_str() && l.variant == variant.as_str())
        {
            global.set_error(crate::i18n::tr("This layout has already been added"));
            return;
        }
        if let Some(layout) = layouts
            .iter()
            .find(|l| l.code == code.as_str() && l.variant == variant.as_str())
        {
            active.push(LayoutChoice {
                code,
                variant,
                name: layout.name.as_str().into(),
                label: layout.code.to_uppercase().into(),
            });
            global.set_active(rows(active));
            global.set_error("".into());
        }
    });
    let weak = window.as_weak();
    global.on_remove(move |index| {
        if let Some(window) = weak.upgrade() {
            let global = window.global::<KeyboardSettings>();
            let mut active: Vec<_> = global.get_active().iter().collect();
            if active.len() > 1 && (index as usize) < active.len() {
                active.remove(index as usize);
                global.set_active(rows(active));
            }
        }
    });
    let weak = window.as_weak();
    global.on_move(move |index, delta| {
        if let Some(window) = weak.upgrade() {
            let global = window.global::<KeyboardSettings>();
            let mut active: Vec<_> = global.get_active().iter().collect();
            let to = index + delta;
            if index >= 0
                && to >= 0
                && (index as usize) < active.len()
                && (to as usize) < active.len()
            {
                active.swap(index as usize, to as usize);
                global.set_active(rows(active));
            }
        }
    });
    let weak = window.as_weak();
    let baseline = Arc::new(std::sync::Mutex::new(config.get_layout()));
    global.on_apply(move || {
        let Some(window) = weak.upgrade() else { return };
        let global = window.global::<KeyboardSettings>();
        let base = baseline.lock().unwrap().clone();
        let mut layout = base.clone();
        layout.keyboard = rix_config::KeyboardConfig {
            layouts: global
                .get_active()
                .iter()
                .map(|l| rix_config::KeyboardLayout {
                    layout: l.code.into(),
                    variant: l.variant.into(),
                    label: l.label.into(),
                })
                .collect(),
            switch_option: OPTIONS
                .get(global.get_switch_index() as usize)
                .unwrap_or(&"")
                .to_string(),
            repeat_rate: global.get_repeat_rate() as u32,
            repeat_delay: global.get_repeat_delay() as u32,
        };
        let baseline = baseline.clone();
        let catalog = catalog.clone();
        crate::jobs::run(
            &window,
            move || crate::jobs::commit_layout(base, layout),
            move |window, layout| {
                sync(window, &layout.keyboard, &catalog);
                *baseline.lock().unwrap() = layout;
                window.global::<KeyboardSettings>().set_error("".into());
                window.set_status_message(crate::i18n::tr("Keyboard layouts saved"));
            },
        );
    });
}

fn sync(
    window: &SettingsWindow,
    keyboard: &rix_config::KeyboardConfig,
    catalog: &[rix_services::keyboard::Layout],
) {
    let global = window.global::<KeyboardSettings>();
    global.set_active(rows(
        keyboard
            .layouts
            .iter()
            .map(|l| LayoutChoice {
                code: l.layout.as_str().into(),
                variant: l.variant.as_str().into(),
                label: if l.label.is_empty() {
                    l.layout.to_uppercase()
                } else {
                    l.label.clone()
                }
                .into(),
                name: catalog
                    .iter()
                    .find(|c| c.code == l.layout && c.variant == l.variant)
                    .map(|c| c.name.as_str())
                    .unwrap_or(&l.layout)
                    .into(),
            })
            .collect(),
    ));
    global.set_catalog(rows(choices(catalog, "")));
    global.set_repeat_rate(keyboard.repeat_rate as i32);
    global.set_repeat_delay(keyboard.repeat_delay as i32);
    global.set_switch_index(
        OPTIONS
            .iter()
            .position(|s| *s == keyboard.switch_option)
            .unwrap_or(5) as i32,
    );
}
