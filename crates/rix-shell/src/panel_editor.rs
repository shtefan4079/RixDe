use crate::RixPanelWindow;
use rix_config::{ConfigManager, PanelConfig, WidgetConfig, WidgetKind, WidgetSection};
use rix_core::{Alignment, PanelLength, Position};
use slint::{ComponentHandle, Model};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

const COMMIT_DEBOUNCE: Duration = Duration::from_millis(150);

pub fn apply(window: &RixPanelWindow, panel: &PanelConfig) {
    window.set_panel_at_top(panel.position == Position::Top);
    window.set_panel_alignment(match panel.alignment {
        Alignment::Start => 0,
        Alignment::End => 2,
        _ => 1,
    });
    window.set_panel_length_mode(match panel.length {
        PanelLength::Fill => 0,
        PanelLength::FitContent => 1,
        PanelLength::Fixed(_) => 2,
    });
    if let PanelLength::Fixed(length) = panel.length {
        window.set_panel_fixed_length(length as i32);
    }
    window.set_panel_reserve_space(panel.exclusive_zone);
    window.set_panel_opacity(panel.opacity);
    let selected = panel
        .widgets
        .iter()
        .find(|w| window.get_selected_widget() == w.id);
    window.set_selected_widget_title(selected.map(|w| label(&w.kind)).unwrap_or_default());
    if let Some(widget) = selected {
        window.set_selected_widget_section(match widget.section {
            WidgetSection::Start => 0,
            WidgetSection::Center => 1,
            WidgetSection::End => 2,
        });
    } else {
        window.set_selected_widget("".into());
    }
}

pub fn set_open(window: &RixPanelWindow, open: bool) {
    if open {
        crate::popup::dismiss_all(window);
    }
    window.set_is_edit_mode(open);
    if !open {
        window.set_selected_widget("".into());
        window.set_selected_widget_title("".into());
    }
    crate::events::fire_and_forget_ipc(rix_ipc::IpcRequest::SetEditMode(open));
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

fn section(index: i32) -> WidgetSection {
    match index {
        0 => WidgetSection::Start,
        1 => WidgetSection::Center,
        _ => WidgetSection::End,
    }
}

type PanelEdit = Box<dyn Fn(&mut PanelConfig) + Send + Sync>;
type Editor = tokio::sync::mpsc::Sender<PanelEdit>;

fn update(window: &RixPanelWindow, editor: &Editor, edit: PanelEdit) {

    if let Err(error) = editor.try_send(edit) {
        window.set_panel_edit_error(
            format!("{}: {error}", crate::i18n::tr("Could not save the panel")).into(),
        );
    }
}

fn commit(window: &RixPanelWindow, editor: &Editor, edits: Vec<PanelEdit>) {
    if edits.is_empty() {
        return;
    }
    let batch: PanelEdit = if edits.len() == 1 {
        edits.into_iter().next().unwrap()
    } else {
        Box::new(move |panel: &mut PanelConfig| {
            for edit in &edits {
                edit(panel);
            }
        })
    };
    update(window, editor, batch);
}

fn flush(window: &RixPanelWindow, editor: &Editor, pending: &RefCell<Vec<PanelEdit>>) {
    let edits: Vec<PanelEdit> = pending.borrow_mut().drain(..).collect();
    commit(window, editor, edits);
}

fn preview(window: &RixPanelWindow, config: &ConfigManager, edit: &PanelEdit) {
    let layout = config.get_layout();
    let Some(mut panel) = crate::binding::panel(&layout).cloned() else {
        return;
    };
    edit(&mut panel);
    window.set_thickness(panel.thickness as i32);
    window.set_floating(panel.floating);
    window.set_floating_margin(panel.floating_margin as i32);
    window.set_corner_radius(panel.corner_radius as i32);
    window.set_use_theme_frame(panel.use_theme_frame);
    apply(window, &panel);
    crate::layout::apply(window, &panel);
}

fn editor(
    window: &RixPanelWindow,
    config: Arc<ConfigManager>,
    apps: crate::apps::ApplicationCatalog,
) -> Editor {
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<PanelEdit>(32);
    let weak = window.as_weak();
    tokio::spawn(async move {
        while let Some(edit) = receiver.recv().await {
            let result = async {
                let mut client = rix_ipc::IpcClient::connect().await?;
                let rix_ipc::IpcResponse::Layout(base) =
                    client.send(rix_ipc::IpcRequest::GetLayout).await?
                else {
                    anyhow::bail!("Unexpected layout response");
                };
                let mut updated = base.clone();
                let panel = crate::binding::panel_mut(&mut updated)
                    .ok_or_else(|| anyhow::anyhow!("Panel no longer exists"))?;
                let previous: Vec<_> = panel
                    .widgets
                    .iter()
                    .map(|widget| widget.id.clone())
                    .collect();
                edit(panel);
                let selected = panel
                    .widgets
                    .iter()
                    .find(|widget| !previous.contains(&widget.id))
                    .map(|widget| widget.id.clone());
                let rix_ipc::IpcResponse::Layout(layout) = client
                    .send(rix_ipc::IpcRequest::EditLayout { base, updated })
                    .await?
                else {
                    anyhow::bail!("Unexpected config write response");
                };
                Ok::<_, anyhow::Error>((layout, selected))
            }
            .await;
            let apps = apps.snapshot();
            let clipboard_enabled = config.get_desktop().clipboard.enabled;
            let _ = weak.upgrade_in_event_loop(move |window| match result {
                Ok((layout, selected)) => {
                    if let Some(id) = selected {
                        window.set_selected_widget(id.into());
                    }
                    window.set_panel_edit_error("".into());
                    crate::apply_layout_to_ui(&window, &layout, &apps, clipboard_enabled);
                }
                Err(error) => window.set_panel_edit_error(
                    format!("{}: {error}", crate::i18n::tr("Could not save the panel")).into(),
                ),
            });
        }
    });
    sender
}

pub fn install(
    window: &RixPanelWindow,
    config: Arc<ConfigManager>,
    apps: impl Into<crate::apps::ApplicationCatalog>,
) {
    let editor = editor(window, config.clone(), apps.into());
    let weak = window.as_weak();
    window.on_toggle_edit_mode(move || {
        if let Some(w) = weak.upgrade() {
            set_open(&w, !w.get_is_edit_mode());
        }
    });
    let weak = window.as_weak();
    window.on_exit_edit_mode(move || {
        if let Some(w) = weak.upgrade() {
            set_open(&w, false);
        }
    });

    let pending: Rc<RefCell<Vec<PanelEdit>>> = Rc::new(RefCell::new(Vec::new()));
    let commit_timer = Rc::new(slint::Timer::default());
    {
        let weak = window.as_weak();
        let edits = editor.clone();
        let pending = pending.clone();
        commit_timer.start(slint::TimerMode::SingleShot, COMMIT_DEBOUNCE, move || {
            if let Some(w) = weak.upgrade() {
                flush(&w, &edits, &pending);
            }
        });
    }
    let weak = window.as_weak();
    let cfg = config.clone();
    let pending_option = pending.clone();
    let option_timer = commit_timer.clone();
    window.on_set_panel_option(move |key, value| {
        let Some(w) = weak.upgrade() else { return };
        if !value.is_finite() {
            return;
        }
        let fixed_length = w.get_panel_fixed_length().max(100) as u32;
        let edit: PanelEdit = Box::new(move |panel| match key.as_str() {
            "height" => panel.thickness = value.round().clamp(24., 120.) as u32,
            "margin" => panel.floating_margin = value.round().clamp(0., 64.) as u32,
            "floating" => panel.floating = value != 0.,
            "radius" => panel.corner_radius = value.round().clamp(0., 64.) as u32,
            "position" => {
                panel.position = if value == 0. {
                    Position::Bottom
                } else {
                    Position::Top
                }
            }
            "alignment" => {
                panel.alignment = match value as i32 {
                    0 => Alignment::Start,
                    2 => Alignment::End,
                    _ => Alignment::Center,
                }
            }
            "length-mode" => {
                panel.length = match value as i32 {
                    1 => PanelLength::FitContent,
                    2 => PanelLength::Fixed(fixed_length),
                    _ => PanelLength::Fill,
                }
            }
            "length" => panel.length = PanelLength::Fixed(value.round().clamp(100., 16384.) as u32),
            "reserve-space" => panel.exclusive_zone = value != 0.,
            "opacity" => panel.opacity = value.clamp(0.1, 1.),
            _ => {}
        });
        preview(&w, &cfg, &edit);
        pending_option.borrow_mut().push(edit);
        option_timer.restart();
    });

    let weak = window.as_weak();
    window.on_change_thickness(move |delta| {
        if let Some(w) = weak.upgrade() {
            w.invoke_set_panel_option("height".into(), (w.get_thickness() + delta) as f32);
        }
    });
    let weak = window.as_weak();
    window.on_toggle_floating(move || {
        if let Some(w) = weak.upgrade() {
            w.invoke_set_panel_option("floating".into(), f32::from(!w.get_floating()));
        }
    });
    let weak = window.as_weak();
    window.on_change_radius(move |delta| {
        if let Some(w) = weak.upgrade() {
            w.invoke_set_panel_option("radius".into(), (w.get_corner_radius() + delta) as f32);
        }
    });

    let weak = window.as_weak();
    let cfg = config.clone();
    window.on_select_widget(move |id| {
        if let Some(w) = weak.upgrade() {
            w.set_selected_widget(id);
            if let Some(panel) = crate::binding::panel(&cfg.get_layout()) {
                apply(&w, panel);
            }
        }
    });
    let weak = window.as_weak();
    let edits = editor.clone();
    let pending_move = pending.clone();
    window.on_move_widget(move |id, x| {
        let Some(w) = weak.upgrade() else { return };
        let rows = w.get_widgets();
        let target = rows.iter().min_by(|a, b| {
            (a.x + a.width / 2. - x)
                .abs()
                .total_cmp(&(b.x + b.width / 2. - x).abs())
        });
        let Some(target) = target else { return };
        if target.id == id {
            return;
        }
        let after = x > target.x + target.width / 2.;
        let target = target.id;
        flush(&w, &edits, &pending_move);
        update(
            &w,
            &edits,
            Box::new(move |panel| move_widget(panel, &id, &target, after)),
        );
    });
    let weak = window.as_weak();
    let edits = editor.clone();
    let pending_add = pending.clone();
    window.on_add_widget(move |index| {
        let Some(w) = weak.upgrade() else { return };
        let kind = match index {
            0 => WidgetKind::Launcher,
            1 => WidgetKind::Taskbar,
            2 => WidgetKind::Tray,
            3 => WidgetKind::Clock,
            4 => WidgetKind::ShowDesktop,
            _ => WidgetKind::Spacer,
        };
        flush(&w, &edits, &pending_add);
        update(
            &w,
            &edits,
            Box::new(move |panel| {
                let id = (1..)
                    .map(|i| format!("panel-{}-{i}", kind.as_str()))
                    .find(|id| panel.widgets.iter().all(|w| &w.id != id))
                    .unwrap();
                let mut widget = WidgetConfig::new(&id, kind.clone(), WidgetSection::End);
                if widget.kind == WidgetKind::Spacer {
                    widget.properties.insert("width".into(), 32.into());
                }
                panel.widgets.push(widget);
            }),
        );
    });
    let weak = window.as_weak();
    let edits = editor.clone();
    let pending_remove = pending.clone();
    window.on_remove_widget(move || {
        if let Some(w) = weak.upgrade() {
            let selected = w.get_selected_widget();
            flush(&w, &edits, &pending_remove);
            update(
                &w,
                &edits,
                Box::new(move |panel| panel.widgets.retain(|item| selected != item.id)),
            );
        }
    });
    let weak = window.as_weak();
    let pending_section = pending.clone();
    window.on_set_widget_section(move |index| {
        if let Some(w) = weak.upgrade() {
            let selected = w.get_selected_widget();
            flush(&w, &editor, &pending_section);
            update(
                &w,
                &editor,
                Box::new(move |panel| {
                    if let Some(widget) = panel.widgets.iter_mut().find(|item| selected == item.id)
                    {
                        widget.section = section(index);
                    }
                }),
            );
        }
    });
}

fn move_widget(panel: &mut PanelConfig, id: &str, target: &str, after: bool) {
    let Some(source) = panel.widgets.iter().position(|w| w.id == id) else {
        return;
    };
    if id == target || !panel.widgets.iter().any(|w| w.id == target) {
        return;
    }
    let mut widget = panel.widgets.remove(source);
    let destination = panel.widgets.iter().position(|w| w.id == target).unwrap();
    widget.section = panel.widgets[destination].section;
    panel
        .widgets
        .insert(destination + usize::from(after), widget);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drag_across_sections_preserves_identity_and_properties() {
        let mut panel = PanelConfig {
            widgets: vec![
                WidgetConfig::new("clock", WidgetKind::Clock, WidgetSection::End)
                    .with_property("timezone", "UTC"),
                WidgetConfig::new("launcher", WidgetKind::Launcher, WidgetSection::Center),
                WidgetConfig::new("tasks", WidgetKind::Taskbar, WidgetSection::Center),
            ],
            ..Default::default()
        };
        move_widget(&mut panel, "clock", "launcher", true);
        assert_eq!(
            panel
                .widgets
                .iter()
                .map(|w| w.id.as_str())
                .collect::<Vec<_>>(),
            ["launcher", "clock", "tasks"]
        );
        assert_eq!(panel.widgets[1].section, WidgetSection::Center);
        assert_eq!(panel.widgets[1].properties["timezone"], "UTC");
        let serialized = rix_config::format_layout_kdl(&rix_config::LayoutConfig {
            panels: vec![panel.clone()],
            ..Default::default()
        });
        assert_eq!(
            rix_config::parse_layout_kdl(&serialized).unwrap().panels[0],
            panel
        );
    }
}
