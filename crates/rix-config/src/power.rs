use crate::{desktop::SessionSettings, ConfigError};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum HardwareProfile {
    #[default]
    Unchanged,
    PowerSaver,
    Balanced,
    Performance,
}
impl HardwareProfile {
    pub fn id(self) -> Option<&'static str> {
        match self {
            Self::Unchanged => None,
            Self::PowerSaver => Some("power-saver"),
            Self::Balanced => Some("balanced"),
            Self::Performance => Some("performance"),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SleepAction {
    #[default]
    Suspend,
    Hibernate,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IdlePolicy {

    pub dim_minutes: u32,
    pub dim_percent: u32,
    pub screen_off_minutes: u32,
    pub lock_minutes: u32,
    pub sleep_minutes: u32,
    pub sleep_action: SleepAction,
}
impl Default for IdlePolicy {
    fn default() -> Self {
        Self {
            dim_minutes: 0,
            dim_percent: 35,
            screen_off_minutes: 0,
            lock_minutes: 10,
            sleep_minutes: 0,
            sleep_action: SleepAction::Suspend,
        }
    }
}
impl IdlePolicy {
    fn validate(&self) -> Result<(), ConfigError> {
        if [
            self.dim_minutes,
            self.screen_off_minutes,
            self.lock_minutes,
            self.sleep_minutes,
        ]
        .iter()
        .any(|delay| *delay > 240)
            || !(5..=80).contains(&self.dim_percent)
        {
            return Err(ConfigError::InvalidValue(
                "Invalid power plan delays or dim level".into(),
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerPlan {
    pub id: String,
    pub profile: HardwareProfile,
    pub idle: IdlePolicy,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PowerSettings {
    pub selected_plan: String,
    pub battery_plan: String,
    pub automatic_battery_plan: bool,
    pub plans: Vec<PowerPlan>,
}
impl Default for PowerSettings {
    fn default() -> Self {
        Self {
            selected_plan: "balanced".into(),
            battery_plan: "power-saver".into(),
            automatic_battery_plan: false,
            plans: vec![
                PowerPlan {
                    id: "power-saver".into(),
                    profile: HardwareProfile::PowerSaver,
                    idle: IdlePolicy {
                        dim_minutes: 2,
                        screen_off_minutes: 5,
                        lock_minutes: 5,
                        sleep_minutes: 15,
                        ..Default::default()
                    },
                },
                PowerPlan {
                    id: "balanced".into(),
                    profile: HardwareProfile::Balanced,
                    idle: IdlePolicy::default(),
                },
                PowerPlan {
                    id: "performance".into(),
                    profile: HardwareProfile::Performance,
                    idle: IdlePolicy::default(),
                },
            ],
        }
    }
}
impl PowerSettings {

    pub fn from_legacy(session: &SessionSettings) -> Self {
        let mut power = Self::default();
        power
            .plans
            .iter_mut()
            .find(|plan| plan.id == "balanced")
            .unwrap()
            .profile = HardwareProfile::Unchanged;
        for plan in &mut power.plans {
            plan.idle.lock_minutes = session.idle_lock_minutes;
        }
        power
    }
    pub fn active(&self, on_battery: Option<bool>) -> &PowerPlan {
        let id = if self.automatic_battery_plan && on_battery == Some(true) {
            &self.battery_plan
        } else {
            &self.selected_plan
        };
        self.plans
            .iter()
            .find(|plan| &plan.id == id)
            .unwrap_or(&self.plans[0])
    }
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.plans.is_empty() || self.plans.len() > 16 {
            return Err(ConfigError::InvalidValue(
                "A power configuration needs 1–16 plans".into(),
            ));
        }
        let mut ids = std::collections::HashSet::new();
        for plan in &self.plans {
            if plan.id.is_empty()
                || plan.id.len() > 64
                || !plan
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                || !ids.insert(&plan.id)
            {
                return Err(ConfigError::InvalidValue(
                    "Invalid or duplicate power plan ID".into(),
                ));
            }
            plan.idle.validate()?;
        }
        if !ids.contains(&self.selected_plan) || !ids.contains(&self.battery_plan) {
            return Err(ConfigError::InvalidValue(
                "Selected power plan does not exist".into(),
            ));
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migration_preserves_lock_and_does_not_change_system_power_mode() {
        let session = SessionSettings {
            idle_lock_minutes: 23,
            ..Default::default()
        };
        let power = PowerSettings::from_legacy(&session);
        assert_eq!(power.active(Some(false)).idle.lock_minutes, 23);
        assert_eq!(power.active(None).profile, HardwareProfile::Unchanged);
        assert_eq!(
            serde_json::from_str::<PowerSettings>(&serde_json::to_string(&power).unwrap()).unwrap(),
            power
        );
    }
    #[test]
    fn battery_selection_and_invalid_plans_are_checked() {
        let mut power = PowerSettings {
            automatic_battery_plan: true,
            ..Default::default()
        };
        assert_eq!(power.active(Some(true)).id, "power-saver");
        assert_eq!(power.active(None).id, "balanced");
        assert!(power.validate().is_ok());
        power.plans[0].idle.sleep_minutes = 241;
        assert!(power.validate().is_err());
        power.plans[0].idle.sleep_minutes = 0;
        power.plans[1].id = power.plans[0].id.clone();
        assert!(power.validate().is_err());
    }
}
