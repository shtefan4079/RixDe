use crate::{i18n, LanguageChoice, LanguageSettings, SettingsWindow};
use rix_config::ConfigManager;
use slint::{ComponentHandle, ModelRc, VecModel};
use std::rc::Rc;
use std::sync::Arc;

fn refresh_choices(window: &SettingsWindow, selected: &str) {
    let global = window.global::<LanguageSettings>();
    global.set_selected(selected.into());
    global.set_choices(ModelRc::from(Rc::new(VecModel::from(vec![
        LanguageChoice {
            id: "system".into(),
            name: i18n::tr("System language"),
            detail: i18n::tr("Follow the language and locale from your login session."),
        },
        LanguageChoice {
            id: "en".into(),
            name: i18n::tr("English"),
            detail: i18n::tr("Use English in RixDE and newly launched applications."),
        },
    ]))));
}

pub fn install(window: &SettingsWindow, config: Arc<ConfigManager>) {
    refresh_choices(window, &config.get_desktop().language);
    let weak = window.as_weak();
    window
        .global::<LanguageSettings>()
        .on_choose(move |selected| {
            let selected = selected.to_string();
            if !matches!(selected.as_str(), "system" | "en") {
                return;
            }
            let Some(window) = weak.upgrade() else { return };
            let base = config.get_desktop();
            let mut desktop = base.clone();
            desktop.language = selected.clone();
            let config = config.clone();
            crate::jobs::run(
                &window,
                move || crate::jobs::commit_desktop(base, desktop),
                move |window, desktop| {
                    if let Err(error) = i18n::select_language(&desktop.language) {
                        tracing::warn!(%error, "Could not apply the selected interface language");
                    }
                    refresh_choices(window, &desktop.language);
                    crate::refresh_localized_models(window, config);
                    window.global::<LanguageSettings>().set_status(i18n::tr(
                        "Language saved. Restart open applications to update their language.",
                    ));
                    window.set_status_message("".into());
                },
            );
        });
}
