use crate::{
    i18n, ApplicationChooser, ChooserApplication, DefaultAppGroup, DefaultAppsSettings,
    SettingsWindow,
};
use rix_config::ConfigManager;
use rix_services::applications::file_opening::{self as opening, ApplicationChoices};
use slint::{ComponentHandle, Image, Model, ModelRc, VecModel};
use std::{
    rc::Rc,
    sync::{Arc, Mutex},
};

struct AppKind {
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    detail: &'static str,
    mime_types: &'static [&'static str],
}
const APP_KINDS: &[AppKind] = &[
    AppKind {
        id: "browser",
        icon: "internet-web-browser",
        label: "Web browser",
        detail: "Web links and HTML files",
        mime_types: &[
            "x-scheme-handler/http",
            "x-scheme-handler/https",
            "text/html",
            "application/xhtml+xml",
        ],
    },
    AppKind {
        id: "mail",
        icon: "mail-client",
        label: "Email",
        detail: "Email links",
        mime_types: &["x-scheme-handler/mailto"],
    },
    AppKind {
        id: "files",
        icon: "system-file-manager",
        label: "File manager",
        detail: "Folders and file locations",
        mime_types: &["inode/directory"],
    },
    AppKind {
        id: "editor",
        icon: "accessories-text-editor",
        label: "Text editor",
        detail: "Plain text files",
        mime_types: &["text/plain"],
    },
    AppKind {
        id: "pdf",
        icon: "application-pdf",
        label: "PDF viewer",
        detail: "PDF documents",
        mime_types: &["application/pdf"],
    },
    AppKind {
        id: "images",
        icon: "image-x-generic",
        label: "Image viewer",
        detail: "Common image formats",
        mime_types: &["image/png", "image/jpeg", "image/webp", "image/gif"],
    },
    AppKind {
        id: "audio",
        icon: "audio-x-generic",
        label: "Audio player",
        detail: "Music and audio files",
        mime_types: &[
            "audio/mpeg",
            "audio/ogg",
            "audio/flac",
            "audio/x-wav",
            "audio/mp4",
        ],
    },
    AppKind {
        id: "video",
        icon: "video-x-generic",
        label: "Video player",
        detail: "Common video formats",
        mime_types: &["video/mp4", "video/webm", "video/x-matroska"],
    },
    AppKind {
        id: "archives",
        icon: "package-x-generic",
        label: "Archive manager",
        detail: "ZIP, TAR and compressed archives",
        mime_types: &[
            "application/zip",
            "application/x-tar",
            "application/gzip",
            "application/x-xz",
            "application/x-7z-compressed",
            "application/vnd.rar",
        ],
    },
];
struct AppGroupData {
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    detail: &'static str,
    application: String,
}
#[derive(Clone)]
struct Choice {
    choices: ApplicationChoices,
    mime_types: Vec<String>,
}
type State = Arc<Mutex<Option<Choice>>>;
fn app_icon(name: &str, icon_theme: &str, is_dark: bool) -> Image {
    if name.starts_with('/') {
        if let Ok(img) = Image::load_from_path(std::path::Path::new(name)) {
            return img;
        }
    }
    let mut resolver = rix_assets::IconResolver::default();
    let themes: &[&str] = if is_dark {
        &[icon_theme, "breeze-dark", "breeze", "Adwaita", "hicolor"]
    } else {
        &[icon_theme, "breeze", "Adwaita", "hicolor", "breeze-dark"]
    };
    for &t in themes {
        if let Some(path) = resolver.resolve(name, t, 32, None) {
            if let Ok(img) = Image::load_from_path(&path) {
                return img;
            }
        }
    }
    Image::default()
}
fn available_groups() -> Vec<AppGroupData> {
    APP_KINDS
        .iter()
        .map(|kind| AppGroupData {
            id: kind.id,
            icon: kind.icon,
            label: kind.label,
            detail: kind.detail,
            application: opening::applications_for_mime(kind.mime_types[0])
                .ok()
                .and_then(|apps| apps.first().map(|app| app.name.clone()))
                .unwrap_or_default(),
        })
        .collect()
}
fn to_ui_groups(groups: Vec<AppGroupData>, icon_theme: &str, is_dark: bool) -> Vec<DefaultAppGroup> {
    groups
        .into_iter()
        .map(|group| DefaultAppGroup {
            id: group.id.into(),
            icon: app_icon(group.icon, icon_theme, is_dark),
            label: i18n::tr(group.label),
            detail: i18n::tr(group.detail),
            application: if group.application.is_empty() {
                i18n::tr("Choose an application")
            } else {
                group.application.into()
            },
        })
        .collect()
}
pub fn refresh(window: &SettingsWindow, config: Arc<ConfigManager>) {
    let weak = window.as_weak();
    let theme = config.get_theme();
    let icon_theme = theme.icon_theme.clone();
    let is_dark = theme.appearance.prefers_dark();
    tokio::spawn(async move {
        let Ok(groups) = tokio::task::spawn_blocking(available_groups).await else {
            return;
        };
        let _ = weak.upgrade_in_event_loop(move |window| {
            window
                .global::<DefaultAppsSettings>()
                .set_groups(ModelRc::from(Rc::new(VecModel::from(to_ui_groups(
                    groups,
                    &icon_theme,
                    is_dark,
                )))));
        });
    });
}
fn refresh_choices(window: &SettingsWindow, state: &State, config: &ConfigManager) {
    let Some(choice) = state.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
        return;
    };
    let ui = window.global::<ApplicationChooser>();
    let query = ui.get_query().trim().to_lowercase();
    let selected = (ui.get_selected_index() >= 0)
        .then(|| {
            ui.get_applications()
                .row_data(ui.get_selected_index() as usize)
        })
        .flatten()
        .map(|app| app.id);
    let theme = config.get_theme();
    let is_dark = theme.appearance.prefers_dark();
    let rows: Vec<_> = choice
        .choices
        .applications
        .iter()
        .filter(|app| {
            (ui.get_show_all() || choice.choices.recommended.contains(&app.id))
                && (query.is_empty()
                    || app.name.to_lowercase().contains(&query)
                    || app.id.to_lowercase().contains(&query))
        })
        .map(|app| ChooserApplication {
            id: app.id.clone().into(),
            label: app.name.clone().into(),
            detail: if choice.choices.recommended.contains(&app.id) {
                i18n::tr("Supports this file type")
            } else {
                i18n::tr("Other application")
            },
            icon: app_icon(&app.icon, &theme.icon_theme, is_dark),
            recommended: choice.choices.recommended.contains(&app.id),
            is_default: choice.choices.default_id.as_ref() == Some(&app.id),
        })
        .collect();
    let index = selected
        .and_then(|id| rows.iter().position(|app| app.id == id))
        .or_else(|| rows.iter().position(|app| app.is_default))
        .or_else(|| (!rows.is_empty()).then_some(0));
    ui.set_applications(ModelRc::from(Rc::new(VecModel::from(rows))));
    ui.set_selected_index(index.map_or(-1, |index| index as i32));
}
fn choices_for(mime_types: Vec<String>) -> Result<Choice, String> {
    let mut choices = opening::choices_for_mime(mime_types.first().ok_or("Invalid file type")?)?;
    for mime in &mime_types[1..] {
        for app in opening::applications_for_mime(mime)? {
            choices.recommended.insert(app.id.clone());
            if !choices
                .applications
                .iter()
                .any(|existing| existing.id == app.id)
            {
                choices.applications.push(app);
            }
        }
    }
    Ok(Choice {
        choices,
        mime_types,
    })
}
pub fn install(window: &SettingsWindow, config: Arc<ConfigManager>) {
    refresh(window, config.clone());
    let state: State = Arc::new(Mutex::new(None));
    let weak = window.as_weak();
    let current = state.clone();
    let config_for_choose = config.clone();
    window
        .global::<DefaultAppsSettings>()
        .on_choose(move |kind_id| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let settings = window.global::<DefaultAppsSettings>();
            if settings.get_busy() {
                return;
            }
            let (mime_types, label) = if kind_id == "custom" {
                (
                    vec![settings.get_custom_mime().trim().to_owned()],
                    i18n::tr("Other file type"),
                )
            } else {
                let Some(kind) = APP_KINDS.iter().find(|kind| kind.id == kind_id.as_str()) else {
                    return;
                };
                (
                    kind.mime_types
                        .iter()
                        .map(|mime| (*mime).to_owned())
                        .collect(),
                    i18n::tr(kind.label),
                )
            };
            settings.set_busy(true);
            settings.set_status(i18n::tr("Loading applications…"));
            let weak = weak.clone();
            let state = current.clone();
            let config = config_for_choose.clone();
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || choices_for(mime_types))
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()));
                let _ = weak.upgrade_in_event_loop(move |window| {
                    let settings = window.global::<DefaultAppsSettings>();
                    settings.set_busy(false);
                    match result {
                        Ok(choice) => {
                            let description =
                                format!("{} · {}", label, choice.mime_types.join(", "));
                            let show_all = choice.choices.recommended.is_empty();
                            *state.lock().unwrap_or_else(|e| e.into_inner()) = Some(choice);
                            let chooser = window.global::<ApplicationChooser>();
                            chooser.set_title(i18n::tr("Choose default application"));
                            chooser.set_description(description.into());
                            chooser.set_query("".into());
                            chooser.set_selected_index(-1);
                            chooser.set_allow_always(false);
                            chooser.set_always_use(false);
                            chooser.set_accept_label(i18n::tr("Set as default"));
                            chooser.set_busy(false);
                            chooser.set_show_all(show_all);
                            refresh_choices(&window, &state, &config);
                            settings.set_status("".into());
                            settings.set_chooser_visible(true);
                        }
                        Err(error) => {
                            tracing::warn!(%error, "Could not list default handlers");
                            settings.set_status(i18n::tr(
                                "Enter a valid MIME type, for example application/pdf",
                            ));
                        }
                    }
                });
            });
        });
    let weak = window.as_weak();
    let current = state.clone();
    let current_config = config.clone();
    window.global::<ApplicationChooser>().on_search(move |_| {
        if let Some(window) = weak.upgrade() {
            refresh_choices(&window, &current, &current_config);
        }
    });
    let weak = window.as_weak();
    let current = state.clone();
    let current_config = config.clone();
    window
        .global::<ApplicationChooser>()
        .on_show_all_changed(move |_| {
            if let Some(window) = weak.upgrade() {
                refresh_choices(&window, &current, &current_config);
            }
        });
    let weak = window.as_weak();
    let current = state.clone();
    window.global::<ApplicationChooser>().on_cancelled(move || {
        if let Some(window) = weak.upgrade() {
            if window.global::<ApplicationChooser>().get_busy() {
                return;
            }
            window
                .global::<DefaultAppsSettings>()
                .set_chooser_visible(false);
            *current.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    });
    let weak = window.as_weak();
    window
        .global::<ApplicationChooser>()
        .on_confirm(move |id, _| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            if window.global::<ApplicationChooser>().get_busy() {
                return;
            }
            let Some(choice) = state.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
                return;
            };
            if !window.global::<DefaultAppsSettings>().get_chooser_visible()
                || !choice
                    .choices
                    .applications
                    .iter()
                    .any(|app| app.id == id.as_str())
            {
                return;
            }
            let id = id.to_string();
            window.global::<ApplicationChooser>().set_busy(true);
            window.global::<DefaultAppsSettings>().set_busy(true);
            window
                .global::<DefaultAppsSettings>()
                .set_status(i18n::tr("Setting the default application…"));
            let weak = weak.clone();
            let state = state.clone();
            let config = config.clone();
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    opening::set_defaults(
                        &choice
                            .mime_types
                            .iter()
                            .map(String::as_str)
                            .collect::<Vec<_>>(),
                        &id,
                    )
                })
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
                let _ = weak.upgrade_in_event_loop(move |window| {
                    window.global::<ApplicationChooser>().set_busy(false);
                    let settings = window.global::<DefaultAppsSettings>();
                    settings.set_busy(false);
                    settings.set_status(match result {
                        Ok(()) => {
                            settings.set_chooser_visible(false);
                            *state.lock().unwrap_or_else(|e| e.into_inner()) = None;
                            i18n::tr("Default application saved")
                        }
                        Err(error) => {
                            tracing::warn!(%error, "Could not update a default application");
                            i18n::tr("Could not set the default application")
                        }
                    });
                    refresh(&window, config);
                });
            });
        });
}
