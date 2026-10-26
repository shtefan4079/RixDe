use crate::{SettingsWindow, StyleSetting};
use rix_config::ThemeSettings;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::rc::Rc;
pub fn install(window: &SettingsWindow, editor: crate::theme_editor::State) {
    window.on_index_of_option(|options, current| index_of_option(&options, &current));
    window.on_style_options(style_options);
    let model = Rc::new(VecModel::from(rows(&editor.baseline())));
    bind(window, editor, model);
}

fn index_of_option(options: &ModelRc<SharedString>, current: &str) -> i32 {
    options
        .iter()
        .position(|option| option.as_str() == current)
        .and_then(|index| i32::try_from(index).ok())
        .unwrap_or(-1)
}

fn style_options(options: ModelRc<SharedString>, current: SharedString) -> ModelRc<SharedString> {

    if current.is_empty() || index_of_option(&options, current.as_str()) >= 0 {
        return options;
    }
    let mut values = Vec::with_capacity(options.row_count().saturating_add(1));
    values.push(current);
    values.extend(options.iter());
    Rc::new(VecModel::from(values)).into()
}

fn rows(theme: &ThemeSettings) -> Vec<StyleSetting> {
    let mut appearance = rix_config::appearance::Appearance::default();
    appearance.palette.extend(theme.appearance.palette.clone());
    appearance.metrics.extend(theme.appearance.metrics.clone());
    let appearance = rix_config::appearance::Appearance {
        palette: appearance.palette,
        metrics: appearance.metrics,
        ..*theme.appearance.clone()
    };
    let mut rows: Vec<StyleSetting> = appearance
        .palette
        .iter()
        .map(|(key, value)| StyleSetting {
            group: "palette".into(),
            key: key.into(),
            value: value.into(),
            number: 0.,
            limit: 1.,
        })
        .collect();
    rows.extend(appearance.metrics.iter().map(|(key, value)| StyleSetting {
        group: "metrics".into(),
        key: key.into(),
        value: value.to_string().into(),
        number: *value,
        limit: (value * 2.).clamp(64., 4096.),
    }));
    for (key, value) in [
        ("font_family", appearance.font_family),
        ("time_format", appearance.time_format),
        ("date_format", appearance.date_format),
        ("panel_frame", appearance.panel_frame),
        ("menu_frame", appearance.menu_frame),
        ("task_active_frame", appearance.task_active_frame),
        ("task_hover_frame", appearance.task_hover_frame),
        ("decoration_theme", appearance.decoration_theme),
    ] {
        rows.push(StyleSetting {
            group: "appearance".into(),
            key: key.into(),
            value: value.into(),
            number: 0.,
            limit: 1.,
        });
    }
    rows
}

pub fn refresh(window: &SettingsWindow, theme: &ThemeSettings) {
    if let Some(model) = window
        .get_style_settings()
        .as_any()
        .downcast_ref::<VecModel<StyleSetting>>()
    {
        model.set_vec(rows(theme));
    }
}

fn bind(
    window: &SettingsWindow,
    editor: crate::theme_editor::State,
    model: Rc<VecModel<StyleSetting>>,
) {
    window.set_style_settings(ModelRc::from(model.clone()));
    let edit = model.clone();
    window.on_edit_style_setting(move |index, value| {
        if let Some(mut row) = edit.row_data(index as usize) {
            row.number = value.parse().unwrap_or(0.);
            row.value = value;
            edit.set_row_data(index as usize, row);
        }
    });
    let weak = window.as_weak();
    window.on_apply_style_settings(move || {
        let result = apply(editor.baseline(), &model);
        if let Some(window) = weak.upgrade() {
            match result {
                Ok(theme) => {
                    crate::jobs::save_theme(&window, editor.clone(), theme);
                }
                Err(error) => window.set_status_message(error.to_string().into()),
            }
        }
    });
}
fn apply(mut theme: ThemeSettings, rows: &VecModel<StyleSetting>) -> anyhow::Result<ThemeSettings> {
    for row in rows.iter() {
        let key = row.key.to_string();
        let value = row.value.to_string();
        match row.group.as_str() {
            "palette" => {
                theme.appearance.palette.insert(key, value);
            }
            "metrics" => {
                let number = value
                    .parse::<f32>()
                    .map_err(|_| anyhow::anyhow!("{}: {key}", crate::i18n::tr("Enter a number")))?;
                theme.appearance.metrics.insert(key, number);
            }
            _ => match key.as_str() {
                "font_family" => theme.appearance.font_family = value,
                "time_format" => theme.appearance.time_format = value,
                "date_format" => theme.appearance.date_format = value,
                "panel_frame" => theme.appearance.panel_frame = value,
                "menu_frame" => theme.appearance.menu_frame = value,
                "task_active_frame" => theme.appearance.task_active_frame = value,
                "task_hover_frame" => theme.appearance.task_hover_frame = value,
                "decoration_theme" => theme.appearance.decoration_theme = value,
                _ => anyhow::bail!("Unknown theme field: {key}"),
            },
        }
    }
    theme.validate()?;
    Ok(theme)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_selectors_find_the_configured_option_and_keep_custom_values() {
        let presets: ModelRc<SharedString> = Rc::new(VecModel::from(vec![
            "%H:%M".into(),
            "%H:%M:%S".into(),
            "%I:%M %p".into(),
        ]))
        .into();
        assert_eq!(index_of_option(&presets, "%H:%M:%S"), 1);
        let existing = style_options(presets.clone(), "%I:%M %p".into());
        assert_eq!(existing.row_count(), 3);
        assert_eq!(index_of_option(&existing, "%I:%M %p"), 2);
        let custom = style_options(presets.clone(), "%H:%M %Z".into());
        assert_eq!(index_of_option(&custom, "%H:%M %Z"), 0);
        assert_eq!(custom.row_data(1).as_deref(), Some("%H:%M"));
        assert_eq!(presets.row_count(), 3, "Presets remain untouched");
        let no_selection = style_options(presets.clone(), "".into());
        assert_eq!(index_of_option(&no_selection, ""), -1);
        assert_eq!(no_selection.row_count(), 3);
    }

    #[test]
    fn discovered_font_names_preserve_a_configured_font_chain() {
        let fonts: ModelRc<SharedString> = Rc::new(VecModel::from(vec![
            "DejaVu Sans".into(),
            "Noto Sans".into(),
        ]))
        .into();
        let chain = "Inter, Noto Sans, sans-serif";
        let options = style_options(fonts.clone(), chain.into());
        assert_eq!(index_of_option(&options, chain), 0);
        assert_eq!(options.row_count(), 3);
        let selected = style_options(fonts, "Noto Sans".into());
        assert_eq!(index_of_option(&selected, "Noto Sans"), 1);
        assert_eq!(selected.row_count(), 2);
    }
}
