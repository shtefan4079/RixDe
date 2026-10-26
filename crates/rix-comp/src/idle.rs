use crate::state::RixState;
use rix_config::power::IdlePolicy;
use smithay::{
    backend::renderer::utils::with_renderer_surface_state, desktop::WindowSurfaceType,
    wayland::idle_inhibit::IdleInhibitHandler,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use wayland_server::{protocol::wl_surface::WlSurface, Resource};

#[derive(Default, Debug)]
pub(crate) struct IdleDecision {
    pub dim: bool,
    pub screen_off: bool,
    pub lock: bool,
    pub sleep: bool,
}
pub(crate) struct Idle {
    last_activity: Instant,
    policy: Option<IdlePolicy>,
    was_locked: bool,
    pub(crate) sleep_sent: bool,
    pub(crate) sleep_valid: Arc<AtomicBool>,
    inhibitors: Vec<WlSurface>,
}
impl Default for Idle {
    fn default() -> Self {
        Self {
            last_activity: Instant::now(),
            policy: None,
            was_locked: false,
            sleep_sent: false,
            sleep_valid: Arc::new(AtomicBool::new(true)),
            inhibitors: Vec::new(),
        }
    }
}
impl Idle {
    fn reset(&mut self, now: Instant) {
        self.last_activity = now;
        self.sleep_sent = false;

        if Arc::strong_count(&self.sleep_valid) > 1 {
            self.sleep_valid.store(false, Ordering::Release);
            self.sleep_valid = Arc::new(AtomicBool::new(true));
        }
    }
    pub(crate) fn activity(&mut self) {
        self.reset(Instant::now());
    }
    fn evaluate(
        &mut self,
        now: Instant,
        policy: &IdlePolicy,
        locked: bool,
        inhibited: bool,
        sleep_inhibited: bool,
    ) -> IdleDecision {
        if self.policy.as_ref() != Some(policy) || inhibited || (self.was_locked && !locked) {
            self.reset(now);
            self.policy = Some(policy.clone());
        }
        self.was_locked = locked;
        let elapsed = now.saturating_duration_since(self.last_activity);
        let due =
            |minutes: u32| minutes > 0 && elapsed >= Duration::from_secs(u64::from(minutes) * 60);
        let sleep = !sleep_inhibited && !self.sleep_sent && due(policy.sleep_minutes);
        IdleDecision {
            dim: due(policy.dim_minutes),
            screen_off: due(policy.screen_off_minutes),
            lock: !locked && (due(policy.lock_minutes) || sleep),
            sleep,
        }
    }
}

impl RixState {
    pub(crate) fn tick_power(
        &mut self,
        policy: &rix_config::power::IdlePolicy,
        respect_inhibitors: bool,
        sleep_inhibited: bool,
        system_idle_inhibited: bool,
    ) -> IdleDecision {
        self.idle.inhibitors.retain(Resource::is_alive);
        let inhibited = respect_inhibitors && (system_idle_inhibited || self.idle_inhibited());
        let decision = self.idle.evaluate(
            Instant::now(),
            policy,
            self.lock.active,
            inhibited,
            sleep_inhibited,
        );
        if decision.lock {
            self.lock.requested = true;
        }
        decision
    }

    pub(crate) fn idle_inhibited(&self) -> bool {
        self.idle.inhibitors.iter().any(|surface| {
            if !with_renderer_surface_state(surface, |state| state.buffer().is_some())
                .unwrap_or(false)
            {
                return false;
            }
            self.space.elements().any(|window| {
                if self.space.outputs_for_element(window).is_empty() {
                    return false;
                }
                let mut found = false;
                window.with_surfaces(|candidate, _| found |= candidate == surface);
                found
            }) || self.space.outputs().any(|output| {
                smithay::desktop::layer_map_for_output(output)
                    .layer_for_surface(surface, WindowSurfaceType::ALL)
                    .is_some()
            })
        })
    }
}

impl IdleInhibitHandler for RixState {
    fn inhibit(&mut self, surface: WlSurface) {

        self.idle.inhibitors.push(surface);
    }
    fn uninhibit(&mut self, surface: WlSurface) {
        if let Some(index) = self.idle.inhibitors.iter().position(|s| s == &surface) {
            self.idle.inhibitors.swap_remove(index);
        }
    }
}
smithay::delegate_idle_inhibit!(RixState);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn staged_actions_keep_counting_through_lock_and_reset_on_input_and_inhibition() {
        let start = Instant::now();
        let mut idle = Idle::default();
        let policy = IdlePolicy {
            dim_minutes: 1,
            lock_minutes: 2,
            screen_off_minutes: 3,
            sleep_minutes: 4,
            ..Default::default()
        };
        assert!(!idle.evaluate(start, &policy, false, false, false).dim);
        assert!(
            idle.evaluate(
                start + Duration::from_secs(60),
                &policy,
                false,
                false,
                false
            )
            .dim
        );
        assert!(
            idle.evaluate(
                start + Duration::from_secs(120),
                &policy,
                false,
                false,
                false
            )
            .lock
        );
        assert!(
            idle.evaluate(
                start + Duration::from_secs(180),
                &policy,
                true,
                false,
                false
            )
            .screen_off
        );
        assert!(
            !idle
                .evaluate(start + Duration::from_secs(240), &policy, true, false, true)
                .sleep
        );
        assert!(
            idle.evaluate(
                start + Duration::from_secs(241),
                &policy,
                true,
                false,
                false
            )
            .sleep
        );
        idle.sleep_sent = true;
        assert!(
            !idle
                .evaluate(
                    start + Duration::from_secs(242),
                    &policy,
                    true,
                    false,
                    false
                )
                .sleep
        );
        let token = idle.sleep_valid.clone();
        idle.activity();
        assert!(!token.load(Ordering::Acquire));
        assert!(
            !idle
                .evaluate(Instant::now(), &policy, true, false, false)
                .screen_off
        );
        assert!(
            !idle
                .evaluate(
                    start + Duration::from_secs(300),
                    &policy,
                    false,
                    true,
                    false
                )
                .dim
        );
    }
    #[test]
    fn changed_plan_unlock_and_disabled_actions_do_not_immediately_fire() {
        let start = Instant::now();
        let mut idle = Idle::default();
        let mut policy = IdlePolicy::default();
        idle.evaluate(start, &policy, false, false, false);
        assert!(
            idle.evaluate(
                start + Duration::from_secs(600),
                &policy,
                false,
                false,
                false
            )
            .lock
        );
        idle.evaluate(
            start + Duration::from_secs(601),
            &policy,
            true,
            false,
            false,
        );
        assert!(
            !idle
                .evaluate(
                    start + Duration::from_secs(700),
                    &policy,
                    false,
                    false,
                    false
                )
                .lock
        );
        policy.lock_minutes = 0;
        let decision = idle.evaluate(
            start + Duration::from_secs(10000),
            &policy,
            false,
            false,
            false,
        );
        assert!(!decision.lock && !decision.sleep && !decision.screen_off);
    }
}
