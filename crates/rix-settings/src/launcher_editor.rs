use crate::{LauncherActionSetting, SettingsWindow};
use rix_config::LauncherAction;
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::rc::Rc;

pub fn load(window: &SettingsWindow, actions: &[LauncherAction]) {
    window.set_launcher_actions(ModelRc::from(Rc::new(VecModel::from(
        actions
            .iter()
            .map(|a| LauncherActionSetting {
                action: a.action.as_str().into(),
                label: a.label.as_str().into(),
                icon: a.icon.as_str().into(),
                end: a.end,
                icon_only: a.icon_only,
                visible: a.visible,
            })
            .collect::<Vec<_>>(),
    ))));
}
pub fn read(window: &SettingsWindow) -> Vec<LauncherAction> {
    window
        .get_launcher_actions()
        .iter()
        .map(|a| LauncherAction {
            action: a.action.to_string(),
            label: a.label.to_string(),
            icon: a.icon.to_string(),
            end: a.end,
            icon_only: a.icon_only,
            visible: a.visible,
        })
        .collect()
}
pub fn install(window: &SettingsWindow) {
    let weak = window.as_weak();
    window.on_edit_launcher_action(move |index, value| {
        let Some(window) = weak.upgrade() else { return };
        let model = window.get_launcher_actions();
        let Some(model) = model
            .as_any()
            .downcast_ref::<VecModel<LauncherActionSetting>>()
        else {
            return;
        };
        if index >= 0 && (index as usize) < model.row_count() {
            model.set_row_data(index as usize, value);
        }
    });
    let weak = window.as_weak();
    window.on_move_launcher_action(move |index, delta| {
        let Some(window) = weak.upgrade() else { return };
        let mut actions = read(&window);
        let destination = index + delta;
        if index >= 0
            && destination >= 0
            && (index as usize) < actions.len()
            && (destination as usize) < actions.len()
        {
            actions.swap(index as usize, destination as usize);
            load(&window, &actions);
        }
    });
}
