use crate::{AppearanceRuleRow, SettingsWindow, WindowAppearancePreferences};
use rix_config::{DesktopSettings, ThemeSettings, WindowAppearanceRule, WindowAppearanceSettings};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::{
    collections::BTreeMap,
    io::Read,
    path::Path,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
struct ThemeChoice {
    label: String,
    theme: Option<ThemeSettings>,
}
struct Draft {
    base: DesktopSettings,
    settings: WindowAppearanceSettings,
    editing: Option<usize>,
    editor_generation: u64,
    theme: Option<ThemeSettings>,
    themes: Vec<ThemeChoice>,
    running: Vec<(String, String)>,
}
impl Draft {
    fn new(base: DesktopSettings) -> Self {
        Self {
            settings: base.window_appearance.clone(),
            base,
            editing: None,
            editor_generation: 0,
            theme: None,
            themes: vec![
                ThemeChoice {
                    label: crate::i18n::tr("Follow desktop theme").to_string(),
                    theme: None,
                },
                ThemeChoice {
                    label: "Plasma Glass".into(),
                    theme: Some(ThemeSettings::plasma_glass()),
                },
                ThemeChoice {
                    label: "Aero Glass".into(),
                    theme: Some(ThemeSettings::aero_glass()),
                },
                ThemeChoice {
                    label: "Cyber Dark".into(),
                    theme: Some(ThemeSettings::cyber_dark()),
                },
                ThemeChoice {
                    label: "Crisp Light".into(),
                    theme: Some(ThemeSettings::crisp_light()),
                },
            ],
            running: Vec::new(),
        }
    }
    fn store(&mut self, app_id: String, material: bool) -> anyhow::Result<()> {
        anyhow::ensure!(
            !app_id.is_empty()
                && app_id.len() <= 255
                && app_id.trim() == app_id
                && !app_id.chars().any(char::is_control),
            crate::i18n::tr("Invalid Wayland application ID")
        );
        anyhow::ensure!(
            !self
                .settings
                .rules
                .iter()
                .enumerate()
                .any(|(index, rule)| Some(index) != self.editing && rule.app_id == app_id),
            crate::i18n::tr("This application already has a rule")
        );
        anyhow::ensure!(
            self.editing.is_some() || self.settings.rules.len() < 64,
            crate::i18n::tr("At most 64 application rules are allowed")
        );
        let mut next = self.settings.clone();
        let rule = WindowAppearanceRule {
            app_id,
            use_desktop_material: material,
            theme: self.theme.clone(),
        };
        if let Some(index) = self.editing {
            let Some(existing) = next.rules.get_mut(index) else {
                anyhow::bail!(crate::i18n::tr("This application rule no longer exists"));
            };
            *existing = rule;
        } else {
            next.rules.push(rule);
        }
        next.validate()?;
        self.settings = next;
        Ok(())
    }
}
fn model<T: Clone + 'static>(rows: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(rows))
}
fn sync(window: &SettingsWindow, draft: &Draft) {
    let ui = window.global::<WindowAppearancePreferences>();
    ui.set_rules(model(
        draft
            .settings
            .rules
            .iter()
            .map(|rule| AppearanceRuleRow {
                app_id: rule.app_id.as_str().into(),
                material: rule.use_desktop_material,
                theme_name: rule
                    .theme
                    .as_ref()
                    .map(|theme| SharedString::from(&theme.name))
                    .unwrap_or_else(|| crate::i18n::tr("Follow desktop theme")),
            })
            .collect(),
    ));
    ui.set_themes(model(
        draft
            .themes
            .iter()
            .map(|choice| SharedString::from(&choice.label))
            .collect(),
    ));
    ui.set_theme_index(
        draft
            .themes
            .iter()
            .position(|choice| choice.theme == draft.theme)
            .unwrap_or(0) as i32,
    );
    let mut running = vec![crate::i18n::tr("Choose a running application…")];
    running.extend(draft.running.iter().map(|(id, title)| {
        if title.is_empty() {
            id.clone().into()
        } else {
            format!("{title} · {id}").into()
        }
    }));
    ui.set_running_apps(model(running));
}
fn begin_editor(window: &SettingsWindow, draft: &mut Draft, index: Option<usize>) {
    let ui = window.global::<WindowAppearancePreferences>();
    let rule = index.and_then(|index| draft.settings.rules.get(index));
    draft.editing = index;
    draft.editor_generation = draft.editor_generation.wrapping_add(1);
    draft.theme = rule.and_then(|rule| rule.theme.clone());
    ui.set_app_id(rule.map(|rule| rule.app_id.as_str()).unwrap_or("").into());
    ui.set_rule_material(
        rule.map(|rule| rule.use_desktop_material)
            .unwrap_or(ui.get_use_desktop_material()),
    );
    ui.set_editing_existing(index.is_some());
    ui.set_running_index(0);
    if let Some(theme) = &draft.theme {
        if !draft
            .themes
            .iter()
            .any(|choice| choice.theme.as_ref() == Some(theme))
        {
            draft.themes.push(ThemeChoice {
                label: format!("{} · {}", crate::i18n::tr("Saved theme"), theme.name),
                theme: Some(theme.clone()),
            });
        }
    }
    sync(window, draft);
    ui.set_editor_open(true);
    ui.set_status("".into());
}
fn running_apps(windows: Vec<rix_ipc::WindowInfo>) -> Vec<(String, String)> {
    let mut apps = BTreeMap::new();
    for window in windows {
        if !window.app_id.is_empty()
            && window.app_id.len() <= 255
            && !window.app_id.chars().any(char::is_control)
        {
            apps.entry(window.app_id).or_insert(window.title);
        }
    }
    apps.into_iter().take(256).collect()
}
fn refresh_apps(window: &SettingsWindow, draft: Arc<Mutex<Draft>>) {
    if window
        .global::<WindowAppearancePreferences>()
        .get_refreshing()
    {
        return;
    }
    window
        .global::<WindowAppearancePreferences>()
        .set_refreshing(true);
    let weak = window.as_weak();
    tokio::spawn(async move {
        let result = async {
            let mut client = rix_ipc::IpcClient::connect().await?;
            match client.send(rix_ipc::IpcRequest::ListWindows).await? {
                rix_ipc::IpcResponse::Windows(windows) => Ok(running_apps(windows)),
                response => anyhow::bail!("Unexpected window list: {response:?}"),
            }
        }
        .await;
        let _ = weak.upgrade_in_event_loop(move |window| {
            let ui = window.global::<WindowAppearancePreferences>();
            ui.set_refreshing(false);
            match result {
                Ok(apps) => {
                    let mut draft = draft.lock().unwrap();
                    draft.running = apps;
                    sync(&window, &draft);
                    ui.set_running_index(0);
                }
                Err(error) => ui.set_status(
                    format!(
                        "{}: {error}",
                        crate::i18n::tr("Could not list running applications")
                    )
                    .into(),
                ),
            }
        });
    });
}
fn read_theme(path: &Path) -> anyhow::Result<ThemeSettings> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 1024 * 1024,
        crate::i18n::tr("Theme document exceeds 1 MiB")
    );
    let theme: ThemeSettings = serde_json::from_slice(&bytes)?;
    theme.validate()?;
    Ok(theme)
}

type Commit =
    Arc<dyn Fn(DesktopSettings, DesktopSettings) -> anyhow::Result<DesktopSettings> + Send + Sync>;
pub fn install(window: &SettingsWindow, config: Arc<rix_config::ConfigManager>) {
    let draft = Arc::new(Mutex::new(Draft::new(config.get_desktop())));
    install_callbacks(window, draft.clone(), Arc::new(crate::jobs::commit_desktop));
    let weak = window.as_weak();
    let layout = config.get_layout();
    let packages_draft = draft.clone();
    tokio::spawn(async move {
        let choices = tokio::task::spawn_blocking(move || {
            rix_config::themes::discover()
                .into_iter()
                .filter_map(|package| {
                    package
                        .load(&layout, false)
                        .ok()
                        .map(|snapshot| ThemeChoice {
                            label: package.info.name,
                            theme: Some(snapshot.theme),
                        })
                })
                .collect::<Vec<_>>()
        })
        .await;
        if let Ok(choices) = choices {
            let _ = weak.upgrade_in_event_loop(move |window| {
                let mut draft = packages_draft.lock().unwrap();
                for choice in choices {
                    if !draft
                        .themes
                        .iter()
                        .any(|existing| existing.theme == choice.theme)
                    {
                        draft.themes.push(choice);
                    }
                }
                sync(&window, &draft);
            });
        }
    });
    refresh_apps(window, draft);
}
fn install_callbacks(window: &SettingsWindow, draft: Arc<Mutex<Draft>>, commit: Commit) {
    let ui = window.global::<WindowAppearancePreferences>();
    ui.set_use_desktop_material(draft.lock().unwrap().settings.use_desktop_material);
    sync(window, &draft.lock().unwrap());
    let weak = window.as_weak();
    let state = draft.clone();
    ui.on_refresh_apps(move || {
        if let Some(window) = weak.upgrade() {
            refresh_apps(&window, state.clone());
        }
    });
    let weak = window.as_weak();
    let state = draft.clone();
    ui.on_choose_running(move |index| {
        let Some(window) = weak.upgrade() else { return };
        let state = state.lock().unwrap();
        if let Some((id, _)) = index
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| state.running.get(index))
        {
            window
                .global::<WindowAppearancePreferences>()
                .set_app_id(id.as_str().into());
        }
    });
    let weak = window.as_weak();
    let state = draft.clone();
    ui.on_add_rule(move || {
        if let Some(window) = weak.upgrade() {
            begin_editor(&window, &mut state.lock().unwrap(), None);
        }
    });
    let weak = window.as_weak();
    let state = draft.clone();
    ui.on_edit_rule(move |index| {
        let Some(window) = weak.upgrade() else { return };
        let mut state = state.lock().unwrap();
        if let Ok(index) = usize::try_from(index) {
            if index < state.settings.rules.len() {
                begin_editor(&window, &mut state, Some(index));
            }
        }
    });
    let weak = window.as_weak();
    let state = draft.clone();
    ui.on_remove_rule(move |index| {
        let Some(window) = weak.upgrade() else { return };
        let mut state = state.lock().unwrap();
        if let Ok(index) = usize::try_from(index) {
            if index < state.settings.rules.len() {
                state.settings.rules.remove(index);
                sync(&window, &state);
            }
        }
    });
    let weak = window.as_weak();
    let state = draft.clone();
    ui.on_choose_theme(move |index| {
        let Some(window) = weak.upgrade() else { return };
        let mut state = state.lock().unwrap();
        if let Some(choice) = usize::try_from(index)
            .ok()
            .and_then(|index| state.themes.get(index))
        {
            state.theme = choice.theme.clone();
            sync(&window, &state);
        }
    });
    let weak = window.as_weak();
    let state = draft.clone();
    ui.on_store_rule(move || {
        let Some(window) = weak.upgrade() else { return };
        let ui = window.global::<WindowAppearancePreferences>();
        let mut state = state.lock().unwrap();
        match state.store(ui.get_app_id().to_string(), ui.get_rule_material()) {
            Ok(()) => {
                sync(&window, &state);
                ui.set_editor_open(false);
                ui.set_status(crate::i18n::tr(
                    "Rule ready. Apply to save window appearance.",
                ));
            }
            Err(error) => ui.set_status(error.to_string().into()),
        }
    });
    let weak = window.as_weak();
    let state = draft.clone();
    ui.on_cancel_rule(move || {
        {
            let mut state = state.lock().unwrap();
            state.editor_generation = state.editor_generation.wrapping_add(1);
        }
        if let Some(window) = weak.upgrade() {
            window
                .global::<WindowAppearancePreferences>()
                .set_editor_open(false);
            window
                .global::<WindowAppearancePreferences>()
                .set_status("".into());
        }
    });
    let weak = window.as_weak();
    let state = draft.clone();
    ui.on_import_theme(move || {
        let generation = state.lock().unwrap().editor_generation;
        let weak = weak.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let result = async {
                let path = crate::dialogs::choose_file(
                    crate::i18n::tr("Import window theme").as_str(),
                    "JSON | *.json *.JSON",
                )
                .await?;
                let Some(path) = path else {
                    return Ok(None);
                };
                tokio::task::spawn_blocking(move || read_theme(&path))
                    .await?
                    .map(Some)
            }
            .await;
            let _ = weak.upgrade_in_event_loop(move |window| {
                let ui = window.global::<WindowAppearancePreferences>();

                if !ui.get_editor_open() || state.lock().unwrap().editor_generation != generation {
                    return;
                }
                match result {
                    Ok(Some(theme)) => {
                        let mut state = state.lock().unwrap();
                        if !state
                            .themes
                            .iter()
                            .any(|choice| choice.theme.as_ref() == Some(&theme))
                        {
                            state.themes.push(ThemeChoice {
                                label: format!(
                                    "{} · {}",
                                    crate::i18n::tr("Imported theme"),
                                    theme.name
                                ),
                                theme: Some(theme.clone()),
                            });
                        }
                        state.theme = Some(theme);
                        sync(&window, &state);
                        ui.set_status("".into());
                    }
                    Ok(None) => {}
                    Err(error) => ui.set_status(error.to_string().into()),
                }
            });
        });
    });
    let weak = window.as_weak();
    ui.on_apply(move || {
        let Some(window) = weak.upgrade() else { return };
        if window
            .global::<WindowAppearancePreferences>()
            .get_editor_open()
        {
            return;
        }
        let (base, updated) = {
            let mut state = draft.lock().unwrap();
            state.settings.use_desktop_material = window
                .global::<WindowAppearancePreferences>()
                .get_use_desktop_material();
            let mut updated = state.base.clone();
            updated.window_appearance = state.settings.clone();
            (state.base.clone(), updated)
        };
        let draft = draft.clone();
        let commit = commit.clone();
        crate::jobs::run(
            &window,
            move || {
                updated.validate()?;
                commit(base, updated)
            },
            move |window, saved| {
                let mut state = draft.lock().unwrap();
                state.settings = saved.window_appearance.clone();
                state.base = saved;
                sync(window, &state);
                window
                    .global::<WindowAppearancePreferences>()
                    .set_use_desktop_material(state.settings.use_desktop_material);
                window
                    .global::<WindowAppearancePreferences>()
                    .set_status("".into());
                window.set_status_message(crate::i18n::tr("Window appearance saved"));
            },
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::Model;

    #[test]
    fn failed_duplicate_edit_preserves_both_original_rules() {
        let mut state = Draft::new(DesktopSettings::default());
        state.store("org.example.One".into(), false).unwrap();
        state.store("org.example.Two".into(), true).unwrap();
        state.editing = Some(1);
        let before = state.settings.clone();
        assert!(state.store("org.example.One".into(), true).is_err());
        assert_eq!(state.settings, before);
    }

    #[test]
    fn json_import_rejects_invalid_theme_and_oversized_document() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("theme.json");
        let expected = ThemeSettings::crisp_light();
        std::fs::write(&path, serde_json::to_vec(&expected).unwrap()).unwrap();
        assert_eq!(read_theme(&path).unwrap(), expected);
        let mut invalid = expected;
        invalid.accent_color = "bad-color".into();
        std::fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(read_theme(&path).is_err());
        std::fs::write(&path, vec![b' '; 1024 * 1024 + 1]).unwrap();
        assert!(read_theme(&path).is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn settings_callbacks_persist_and_edit_the_selected_application_rule() {
        i_slint_backend_testing::init_integration_test_with_system_time();
        let fixture = tempfile::tempdir().unwrap();
        let config =
            Arc::new(rix_config::ConfigManager::init_in(fixture.path().join("rix")).unwrap());
        let window = SettingsWindow::new().unwrap();
        let mut state = Draft::new(config.get_desktop());
        state.running = running_apps(vec![
            rix_ipc::WindowInfo {
                id: 1,
                title: "Editor document".into(),
                app_id: "org.example.Editor".into(),
                outputs: Vec::new(),
                foreground_outputs: None,
                is_active: true,
                is_minimized: false,
                is_maximized: false,
                is_fullscreen: false,
            },
            rix_ipc::WindowInfo {
                id: 2,
                title: "Other document".into(),
                app_id: "org.example.Editor".into(),
                outputs: Vec::new(),
                foreground_outputs: None,
                is_active: false,
                is_minimized: false,
                is_maximized: false,
                is_fullscreen: false,
            },
        ]);
        assert_eq!(
            state.running.len(),
            1,
            "Multiple windows of one app must share one ID choice"
        );
        let config_for_commit = config.clone();

        install_callbacks(
            &window,
            Arc::new(Mutex::new(state)),
            Arc::new(move |base, updated| {
                let merged = rix_config::edit::merge_document(
                    &base,
                    &updated,
                    &config_for_commit.get_desktop(),
                )?;
                config_for_commit.save_desktop(merged.clone())?;
                Ok(merged)
            }),
        );
        window.set_active_tab(17);
        let ui = window.global::<WindowAppearancePreferences>();
        ui.invoke_add_rule();
        ui.invoke_choose_running(1);
        ui.invoke_choose_theme(4);
        ui.set_rule_material(false);
        ui.invoke_store_rule();
        assert_eq!(
            ui.get_rules().row_data(0).unwrap().app_id,
            "org.example.Editor"
        );
        assert_eq!(
            ui.get_rules().row_data(0).unwrap().theme_name,
            "Crisp Light"
        );
        assert!(!ui.get_rules().row_data(0).unwrap().material);
        let mut concurrent = config.get_desktop();
        concurrent.language = "en".into();
        concurrent.wallpaper.dim = 0.35;
        config.save_desktop(concurrent).unwrap();
        ui.set_use_desktop_material(false);
        ui.invoke_apply();
        let weak = window.as_weak();
        let timer = slint::Timer::default();
        let mut phase = 0;
        let mut ticks = 0;
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(20),
            move || {
                ticks += 1;
                assert!(ticks < 250, "Window appearance callback save timed out");
                let window = weak.upgrade().unwrap();
                if window.get_busy() {
                    return;
                }
                let saved: DesktopSettings = serde_json::from_slice(
                    &std::fs::read(config.config_dir().join("desktop.json")).unwrap(),
                )
                .unwrap();
                assert_eq!(saved.language, "en");
                assert_eq!(saved.wallpaper.dim, 0.35);
                let ui = window.global::<WindowAppearancePreferences>();
                match phase {
                    0 => {
                        assert!(!saved.window_appearance.use_desktop_material);
                        let rule = &saved.window_appearance.rules[0];
                        assert_eq!(rule.app_id, "org.example.Editor");
                        assert!(!rule.use_desktop_material);
                        assert_eq!(rule.theme, Some(ThemeSettings::crisp_light()));
                        ui.invoke_edit_rule(0);
                        assert_eq!(ui.get_app_id(), "org.example.Editor");
                        assert_eq!(ui.get_theme_index(), 4);
                        ui.invoke_choose_theme(0);
                        ui.set_rule_material(true);
                        ui.invoke_store_rule();
                        ui.set_use_desktop_material(true);
                        ui.invoke_apply();
                        phase = 1;
                    }
                    1 => {
                        let rule = &saved.window_appearance.rules[0];
                        assert_eq!(rule.app_id, "org.example.Editor");
                        assert!(rule.use_desktop_material);
                        assert!(rule.theme.is_none());
                        assert!(saved.window_appearance.use_desktop_material);
                        ui.invoke_remove_rule(0);
                        ui.invoke_apply();
                        phase = 2;
                    }
                    _ => {
                        assert!(saved.window_appearance.rules.is_empty());
                        slint::quit_event_loop().unwrap();
                    }
                }
            },
        );
        window.run().unwrap();
    }

    #[test]
    fn render_window_appearance_with_dark_and_light_themes() {
        use slint::platform::{
            software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
            Platform, PlatformError, WindowAdapter,
        };
        use std::rc::Rc;
        struct Surface(Rc<MinimalSoftwareWindow>);
        impl Platform for Surface {
            fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
                Ok(self.0.clone())
            }
        }
        let surface = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Surface(surface.clone()))).unwrap();
        let window = SettingsWindow::new().unwrap();
        crate::skins::install(window.global::<crate::SkinRenderer>(), window.as_weak());
        window.set_active_tab(17);
        let output = std::env::var_os("RIX_SETTINGS_RENDER_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("rix-settings-window-appearance"));
        std::fs::create_dir_all(&output).unwrap();
        for (name, theme) in [
            ("dark", ThemeSettings::default()),
            ("light", ThemeSettings::crisp_light()),
        ] {
            crate::theme_bindings::apply(
                window.global::<crate::Theme>(),
                window.global::<crate::Palette>(),
                &theme,
            );
            for (width, height) in [(900u32, 600u32), (1040, 760)] {
                window
                    .window()
                    .set_size(slint::PhysicalSize::new(width, height));
                let mut draft = Draft::new(DesktopSettings::default());
                draft.theme = Some(ThemeSettings::crisp_light());
                draft.store("org.example.Editor".into(), false).unwrap();
                sync(&window, &draft);
                let ui = window.global::<WindowAppearancePreferences>();
                ui.set_editor_open(false);
                ui.set_scroll_position(0.);
                window.show().unwrap();
                for state in ["rules", "editor", "editor-bottom"] {
                    if state == "editor" {
                        begin_editor(&window, &mut draft, Some(0));
                    }
                    if state == "editor-bottom" {
                        ui.set_scroll_position(-400.);
                    }
                    slint::platform::update_timers_and_animations();
                    let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
                    surface.request_redraw();
                    surface.draw_if_needed(|renderer| {
                        renderer.render(&mut pixels, width as usize);
                    });
                    let bytes: Vec<_> = pixels
                        .iter()
                        .flat_map(|pixel| [pixel.r, pixel.g, pixel.b])
                        .collect();
                    assert!(
                        bytes.iter().any(|byte| *byte > 100),
                        "Settings must render readable content"
                    );
                    image::save_buffer(
                        output.join(format!("{name}-{width}-{state}.png")),
                        &bytes,
                        width,
                        height,
                        image::ColorType::Rgb8,
                    )
                    .unwrap();
                }
            }
        }
    }
}
