use crate::{PanelWidgetSetting, SettingsWindow};
use rix_config::{PanelConfig, WidgetConfig, WidgetKind, WidgetSection};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::rc::Rc;

#[derive(Clone)]
pub struct EditorState(std::sync::Arc<std::sync::Mutex<rix_config::LayoutConfig>>);
impl EditorState {
    pub fn baseline(&self) -> rix_config::LayoutConfig {
        self.0.lock().unwrap().clone()
    }
    pub fn load(&self, window: &SettingsWindow, layout: &rix_config::LayoutConfig) {
        crate::sync_layout_to_ui(window, layout);
        *self.0.lock().unwrap() = layout.clone();
    }
}

pub fn load(window: &SettingsWindow, panel: &PanelConfig) {
    let rows = panel
        .widgets
        .iter()
        .map(|w| PanelWidgetSetting {
            id: w.id.as_str().into(),
            kind: w.kind.as_str().into(),
            label: label(&w.kind),
            section: match w.section {
                WidgetSection::Start => 0,
                WidgetSection::Center => 1,
                WidgetSection::End => 2,
            },
            width: w
                .properties
                .get("width")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as i32,
        })
        .collect::<Vec<_>>();
    window.set_panel_widgets(ModelRc::from(Rc::new(VecModel::from(rows))));
}
fn label(kind: &WidgetKind) -> slint::SharedString {
    match kind {
        WidgetKind::Launcher => crate::i18n::tr("Application launcher"),
        WidgetKind::Taskbar => crate::i18n::tr("Applications"),
        WidgetKind::Tray => crate::i18n::tr("System tray"),
        WidgetKind::Clock => crate::i18n::tr("Clock"),
        WidgetKind::ShowDesktop => crate::i18n::tr("Show desktop"),
        WidgetKind::Spacer => crate::i18n::tr("Spacer"),
        _ => kind.as_str().into(),
    }
}

pub fn refresh_labels(window: &SettingsWindow) {
    let mut rows = window.get_panel_widgets().iter().collect::<Vec<_>>();
    for row in &mut rows {
        row.label = label(&WidgetKind::from_name(row.kind.as_str()));
    }
    window.set_panel_widgets(ModelRc::from(Rc::new(VecModel::from(rows))));
}

pub fn install(
    window: &SettingsWindow,
    config: std::sync::Arc<rix_config::ConfigManager>,
) -> EditorState {
    let state = EditorState(std::sync::Arc::new(std::sync::Mutex::new(
        config.get_layout(),
    )));
    state.load(window, &state.baseline());
    let weak = window.as_weak();
    let cfg = config.clone();
    let editor = state.clone();
    window.on_select_panel(move || {
        if let Some(window) = weak.upgrade() {
            editor.load(&window, &cfg.get_layout());
        }
    });
    let weak = window.as_weak();
    let cfg = config.clone();
    let editor = state.clone();
    window.on_add_panel(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let base = cfg.get_layout();
        let mut layout = base.clone();
        if layout.panels.len() >= 16 {
            return;
        }
        let panel = PanelConfig {
            id: (1..)
                .map(|n| format!("panel-{n}"))
                .find(|id| layout.panels.iter().all(|p| &p.id != id))
                .unwrap(),
            position: rix_core::Position::Top,
            ..Default::default()
        };
        let selected = layout.panels.len() as i32;
        layout.panels.push(panel);
        save_structure(&window, base, layout, selected, editor.clone());
    });
    let weak = window.as_weak();
    let cfg = config;
    let editor = state.clone();
    window.on_remove_panel(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let base = cfg.get_layout();
        let mut layout = base.clone();
        let loaded = editor.baseline();
        let Some(panel) = loaded
            .panels
            .get(window.get_selected_panel().max(0) as usize)
        else {
            return;
        };
        let Some(selected) = layout
            .panels
            .iter()
            .position(|candidate| candidate.id == panel.id)
        else {
            return;
        };
        if layout.panels.len() <= 1 || selected >= layout.panels.len() {
            return;
        }
        layout.panels.remove(selected);
        save_structure(&window, base, layout, selected as i32, editor.clone());
    });
    let weak = window.as_weak();
    window.on_edit_panel_widget(move |index, row| {
        let Some(window) = weak.upgrade() else { return };
        let rows = window.get_panel_widgets();
        if let Some(model) = rows.as_any().downcast_ref::<VecModel<PanelWidgetSetting>>() {
            if index >= 0 && (index as usize) < model.row_count() {
                model.set_row_data(index as usize, row);
            }
        }
    });
    let weak = window.as_weak();
    window.on_move_panel_widget(move |index, delta| {
        let Some(window) = weak.upgrade() else { return };
        let model = window.get_panel_widgets();
        let mut rows = model.iter().collect::<Vec<_>>();
        let destination = index + delta;
        if index >= 0
            && destination >= 0
            && (index as usize) < rows.len()
            && (destination as usize) < rows.len()
        {
            rows.swap(index as usize, destination as usize);
            window.set_panel_widgets(ModelRc::from(Rc::new(VecModel::from(rows))));
        }
    });
    let weak = window.as_weak();
    window.on_remove_panel_widget(move |index| {
        let Some(window) = weak.upgrade() else { return };
        let rows = window.get_panel_widgets();
        if let Some(model) = rows.as_any().downcast_ref::<VecModel<PanelWidgetSetting>>() {
            if index >= 0 && (index as usize) < model.row_count() {
                model.remove(index as usize);
            }
        }
    });
    let weak = window.as_weak();
    window.on_add_panel_widget(move |kind| {
        let Some(window) = weak.upgrade() else { return };
        let kind = match kind {
            0 => WidgetKind::Launcher,
            1 => WidgetKind::Taskbar,
            2 => WidgetKind::Tray,
            3 => WidgetKind::Clock,
            4 => WidgetKind::ShowDesktop,
            _ => WidgetKind::Spacer,
        };
        let model = window.get_panel_widgets();
        let mut rows = model.iter().collect::<Vec<_>>();
        let id = (1..)
            .map(|n| format!("ui-{}-{n}", kind.as_str()))
            .find(|id| rows.iter().all(|r| r.id != id))
            .unwrap();
        rows.push(PanelWidgetSetting {
            id: id.into(),
            kind: kind.as_str().into(),
            label: label(&kind),
            section: 2,
            width: 0,
        });
        window.set_panel_widgets(ModelRc::from(Rc::new(VecModel::from(rows))));
    });
    let editor = state.clone();
    let h = window.as_weak();
    window.on_apply_panel_launcher(move || {
        let Some(w) = h.upgrade() else { return };
        let base = editor.baseline();
        let mut layout = base.clone();
        let selected =
            (w.get_selected_panel().max(0) as usize).min(layout.panels.len().saturating_sub(1));
        let old_widgets = layout
            .panels
            .get(selected)
            .map(|panel| panel.widgets.clone())
            .unwrap_or_default();
        let widgets = widgets(&w, &old_widgets);
        if layout.panels.is_empty() {
            layout.panels.push(Default::default());
        }
        let panel = &mut layout.panels[selected];
        panel.screen = w.get_panel_screen().to_string();
        panel.position = match w.get_panel_position() {
            0 => rix_core::Position::Bottom,
            1 => rix_core::Position::Top,
            _ => panel.position,
        };
        panel.alignment = match w.get_panel_alignment() {
            0 => rix_core::Alignment::Start,
            2 => rix_core::Alignment::End,
            1 => rix_core::Alignment::Center,
            _ => panel.alignment,
        };
        panel.length = match w.get_panel_length_mode() {
            1 => rix_core::PanelLength::FitContent,
            2 => rix_core::PanelLength::Fixed(w.get_panel_length().clamp(120, 7680) as u32),
            _ => rix_core::PanelLength::Fill,
        };
        panel.exclusive_zone = w.get_panel_exclusive();
        panel.thickness = w.get_panel_thickness().clamp(24, 120) as u32;
        panel.floating_margin = w.get_panel_margin().clamp(0, 64) as u32;
        panel.corner_radius = w.get_panel_radius().clamp(0, 32) as u32;
        panel.floating = w.get_panel_floating();
        panel.use_theme_frame = w.get_panel_theme_frame();
        panel.widgets = widgets;

        layout.launcher.width = w.get_launcher_width().clamp(400, 1200) as u32;
        layout.launcher.alignment = match w.get_launcher_alignment() {
            0 => rix_core::Alignment::Start,
            2 => rix_core::Alignment::End,
            _ => rix_core::Alignment::Center,
        };
        layout.launcher.height = w.get_launcher_height().clamp(400, 1000) as u32;
        let icon = w.get_launcher_icon().trim().to_owned();
        layout.launcher.icon = if icon.is_empty() {
            "start-here".into()
        } else {
            icon
        };
        layout.launcher.footer = crate::launcher_editor::read(&w);

        let editor = editor.clone();
        crate::jobs::run(
            &w,
            move || crate::jobs::commit_layout(base, layout),
            move |w, layout| {
                editor.load(w, &layout);
                w.set_status_message(crate::i18n::tr("Panel and launcher applied"));
            },
        );
    });
    state
}
fn save_structure(
    window: &SettingsWindow,
    base: rix_config::LayoutConfig,
    layout: rix_config::LayoutConfig,
    selected: i32,
    editor: EditorState,
) {
    crate::jobs::run(
        window,
        move || crate::jobs::commit_layout(base, layout),
        move |window, layout| {
            window.set_selected_panel(selected);
            editor.load(window, &layout);
            window.set_status_message("".into());
        },
    );
}

pub fn set_outputs(window: &SettingsWindow, outputs: &[String]) {
    let mut ids = vec!["primary".to_string(), "all".to_string()];
    ids.extend(
        outputs
            .iter()
            .filter(|id| !["focused", "primary", "all", ""].contains(&id.as_str()))
            .cloned(),
    );
    let current = window.get_panel_screen();
    let current = if current.is_empty() || current == "focused" {
        "primary"
    } else {
        current.as_str()
    };
    if !ids.iter().any(|id| id == current) {
        ids.push(current.to_string());
    }
    ids.dedup();
    window.set_panel_screen_index(ids.iter().position(|id| id == current).unwrap_or(0) as i32);
    window.set_panel_screens(ModelRc::new(VecModel::from(
        ids.iter()
            .map(|id| match id.as_str() {
                "primary" => crate::i18n::tr("Primary display"),
                "all" => crate::i18n::tr("All displays"),
                _ => id.as_str().into(),
            })
            .collect::<Vec<_>>(),
    )));
    window.set_panel_screen_ids(ModelRc::new(VecModel::from(
        ids.into_iter()
            .map(Into::into)
            .collect::<Vec<slint::SharedString>>(),
    )));
}

pub fn widgets(window: &SettingsWindow, previous: &[WidgetConfig]) -> Vec<WidgetConfig> {
    collect(
        window.get_panel_widgets().iter(),
        previous,
        window.get_taskbar_labels(),
    )
}
fn collect(
    rows: impl Iterator<Item = PanelWidgetSetting>,
    previous: &[WidgetConfig],
    labels: bool,
) -> Vec<WidgetConfig> {
    rows.map(|row| {
        let section = match row.section {
            0 => WidgetSection::Start,
            1 => WidgetSection::Center,
            _ => WidgetSection::End,
        };
        let mut widget = previous
            .iter()
            .find(|w| row.id == w.id)
            .cloned()
            .unwrap_or_else(|| {
                WidgetConfig::new(
                    row.id.to_string(),
                    WidgetKind::from_name(&row.kind),
                    section,
                )
            });
        widget.section = section;
        if row.width > 0 {
            widget.properties.insert("width".into(), row.width.into());
        } else {
            widget.properties.remove("width");
        }
        if widget.kind == WidgetKind::Taskbar {
            widget
                .properties
                .insert("show-labels".into(), labels.into());
        }
        widget
    })
    .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn moving_widget_keeps_instance_properties() {
        let original = WidgetConfig::new("clock-a", WidgetKind::Clock, WidgetSection::End)
            .with_property("timezone", "Europe/Amsterdam")
            .with_property("width", 180);
        let row = PanelWidgetSetting {
            id: "clock-a".into(),
            section: 0,
            width: 120,
            ..Default::default()
        };
        let result = collect([row].into_iter(), &[original], false);
        assert_eq!(result[0].section, WidgetSection::Start);
        assert_eq!(result[0].properties["timezone"], "Europe/Amsterdam");
        assert_eq!(result[0].properties["width"], 120);
    }
}
