use crate::{PowerPreferences, SettingsWindow};
use rix_config::{
    power::{HardwareProfile, PowerSettings, SleepAction},
    DesktopSettings,
};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::{
    rc::Rc,
    sync::{Arc, Mutex},
};
struct Draft {
    base: DesktopSettings,
    settings: PowerSettings,
    editing: usize,
    choices: Vec<HardwareProfile>,
    available: Option<Vec<String>>,
}
fn label(id: &str) -> SharedString {
    crate::i18n::tr(match id {
        "power-saver" => "Power saver",
        "balanced" => "Balanced",
        "performance" => "Performance",
        _ => id,
    })
}
fn profile_label(profile: HardwareProfile) -> SharedString {
    match profile.id() {
        Some(id) => label(id),
        None => crate::i18n::tr("Keep current system mode"),
    }
}
fn names(values: Vec<SharedString>) -> ModelRc<SharedString> {
    ModelRc::from(Rc::new(VecModel::from(values)))
}
fn profile_names(draft: &Draft) -> ModelRc<SharedString> {
    names(
        draft
            .choices
            .iter()
            .map(|profile| {
                let mut title = profile_label(*profile).to_string();
                if profile.id().is_some_and(|id| {
                    draft
                        .available
                        .as_ref()
                        .is_some_and(|available| !available.iter().any(|value| value == id))
                }) {
                    title.push_str(&format!(" · {}", crate::i18n::tr("Unavailable")));
                }
                title.into()
            })
            .collect(),
    )
}
fn store_controls(window: &SettingsWindow, draft: &mut Draft) {
    let global = window.global::<PowerPreferences>();
    let plan = &mut draft.settings.plans[draft.editing];
    plan.profile = draft
        .choices
        .get(global.get_profile().max(0) as usize)
        .copied()
        .unwrap_or_default();
    plan.idle.dim_minutes = global.get_dim_minutes().clamp(0, 240) as u32;
    plan.idle.dim_percent = global.get_dim_percent().clamp(5, 80) as u32;
    plan.idle.screen_off_minutes = global.get_off_minutes().clamp(0, 240) as u32;
    plan.idle.lock_minutes = global.get_lock_minutes().clamp(0, 240) as u32;
    plan.idle.sleep_minutes = global.get_sleep_minutes().clamp(0, 240) as u32;
    plan.idle.sleep_action = if global.get_sleep_action() == 1 {
        SleepAction::Hibernate
    } else {
        SleepAction::Suspend
    };
}
fn load_controls(window: &SettingsWindow, draft: &Draft) {
    let global = window.global::<PowerPreferences>();
    let plan = &draft.settings.plans[draft.editing];
    global.set_editing(draft.editing as i32);
    global.set_profile(
        draft
            .choices
            .iter()
            .position(|p| p == &plan.profile)
            .unwrap_or(0) as i32,
    );
    global.set_dim_minutes(plan.idle.dim_minutes as i32);
    global.set_dim_percent(plan.idle.dim_percent as i32);
    global.set_off_minutes(plan.idle.screen_off_minutes as i32);
    global.set_lock_minutes(plan.idle.lock_minutes as i32);
    global.set_sleep_minutes(plan.idle.sleep_minutes as i32);
    global.set_sleep_action(i32::from(plan.idle.sleep_action == SleepAction::Hibernate));
}
fn load(window: &SettingsWindow, draft: &Draft) {
    let global = window.global::<PowerPreferences>();
    global.set_plans(names(
        draft.settings.plans.iter().map(|p| label(&p.id)).collect(),
    ));
    global.set_profiles(profile_names(draft));
    global.set_selected(
        draft
            .settings
            .plans
            .iter()
            .position(|p| p.id == draft.settings.selected_plan)
            .unwrap_or(0) as i32,
    );
    global.set_battery(
        draft
            .settings
            .plans
            .iter()
            .position(|p| p.id == draft.settings.battery_plan)
            .unwrap_or(0) as i32,
    );
    global.set_automatic(draft.settings.automatic_battery_plan);
    global.set_inhibitors(draft.base.session.respect_idle_inhibitors);
    global.set_external_lid(draft.base.session.keep_running_with_external_display);
    load_controls(window, draft);
}
fn status(window: &SettingsWindow, draft: &mut Draft, state: rix_ipc::PowerState) {
    store_controls(window, draft);
    let chosen = draft.settings.plans[draft.editing].profile;
    draft.choices = [
        HardwareProfile::Unchanged,
        HardwareProfile::PowerSaver,
        HardwareProfile::Balanced,
        HardwareProfile::Performance,
    ]
    .into_iter()
    .filter(|p| {
        p.id()
            .is_none_or(|id| state.available_profiles.iter().any(|v| v == id))
    })
    .collect();
    if !draft.choices.contains(&chosen) {
        draft.choices.push(chosen);
    }
    draft.available = Some(state.available_profiles.clone());
    let global = window.global::<PowerPreferences>();
    global.set_profiles(profile_names(draft));
    global.set_profile(draft.choices.iter().position(|p| p == &chosen).unwrap_or(0) as i32);
    global.set_native(state.native);
    global.set_can_suspend(state.can_suspend);
    global.set_can_hibernate(state.can_hibernate);
    global.set_source(crate::i18n::tr(match state.on_battery {
        Some(true) => "On battery",
        Some(false) => "Plugged in",
        None => "Power source unavailable",
    }));
    global.set_actual_profile(if state.active_profile.is_empty() {
        crate::i18n::tr("Unavailable")
    } else {
        label(&state.active_profile)
    });
    let error = if !state.error.is_empty() {
        crate::i18n::tr(&state.error).to_string()
    } else if !state.performance_degraded.is_empty() {
        format!(
            "{}: {}",
            crate::i18n::tr("Performance is limited"),
            state.performance_degraded
        )
    } else {
        String::new()
    };
    global.set_error(error.into());
}
pub fn install(window: &SettingsWindow, config: Arc<rix_config::ConfigManager>) {
    let base = config.get_desktop();
    let settings = base
        .power
        .clone()
        .unwrap_or_else(|| PowerSettings::from_legacy(&base.session));
    let editing = settings
        .plans
        .iter()
        .position(|plan| plan.id == settings.selected_plan)
        .unwrap_or(0);
    let draft = Arc::new(Mutex::new(Draft {
        base,
        settings,
        editing,
        available: None,
        choices: vec![
            HardwareProfile::Unchanged,
            HardwareProfile::PowerSaver,
            HardwareProfile::Balanced,
            HardwareProfile::Performance,
        ],
    }));
    load(window, &draft.lock().unwrap());
    let global = window.global::<PowerPreferences>();
    let weak = window.as_weak();
    let edit = draft.clone();
    global.on_choose(move |index| {
        let Some(window) = weak.upgrade() else { return };
        let mut draft = edit.lock().unwrap();
        store_controls(&window, &mut draft);
        draft.editing = (index.max(0) as usize).min(draft.settings.plans.len() - 1);

        let chosen = draft.settings.plans[draft.editing].profile;
        if !draft.choices.contains(&chosen) {
            draft.choices.push(chosen);
            window
                .global::<PowerPreferences>()
                .set_profiles(profile_names(&draft));
        }
        load_controls(&window, &draft);
    });
    let weak = window.as_weak();
    let apply = draft.clone();
    global.on_apply(move || {
        let Some(window) = weak.upgrade() else { return };
        let global = window.global::<PowerPreferences>();
        let mut draft = apply.lock().unwrap();
        store_controls(&window, &mut draft);
        let ids: Vec<_> = draft.settings.plans.iter().map(|p| p.id.clone()).collect();
        draft.settings.selected_plan =
            ids[global.get_selected().clamp(0, ids.len() as i32 - 1) as usize].clone();
        draft.settings.battery_plan =
            ids[global.get_battery().clamp(0, ids.len() as i32 - 1) as usize].clone();
        draft.settings.automatic_battery_plan = global.get_automatic();
        let base = draft.base.clone();
        let mut updated = base.clone();
        updated.power = Some(draft.settings.clone());
        updated.session.respect_idle_inhibitors = global.get_inhibitors();
        updated.session.keep_running_with_external_display = global.get_external_lid();
        let apply = apply.clone();
        drop(draft);
        crate::jobs::run(
            &window,
            move || {
                updated.validate()?;
                crate::jobs::commit_desktop(base, updated)
            },
            move |window, saved| {
                let mut draft = apply.lock().unwrap();
                draft.settings = saved.power.clone().unwrap();
                draft.base = saved;
                load(window, &draft);
                window.set_status_message(crate::i18n::tr("Power settings saved"));
            },
        );
    });
    let weak = window.as_weak();
    tokio::spawn(async move {
        loop {
            let result = async {

                let mut events = rix_ipc::IpcClient::subscribe().await?;
                let mut client = rix_ipc::IpcClient::connect().await?;
                if let rix_ipc::IpcResponse::Power(state) =
                    client.send(rix_ipc::IpcRequest::GetPowerState).await?
                {
                    let draft = draft.clone();
                    let _ = weak.upgrade_in_event_loop(move |w| {
                        status(&w, &mut draft.lock().unwrap(), state)
                    });
                }
                loop {
                    if let rix_ipc::IpcEvent::PowerChanged(state) = events.next_event().await? {
                        let draft = draft.clone();
                        let _ = weak.upgrade_in_event_loop(move |w| {
                            status(&w, &mut draft.lock().unwrap(), state)
                        });
                    }
                }
                #[allow(unreachable_code)]
                Ok::<(), rix_ipc::ClientError>(())
            }
            .await;
            if let Err(error) = result {
                tracing::debug!(%error, "Power service subscription disconnected");
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    });
}
