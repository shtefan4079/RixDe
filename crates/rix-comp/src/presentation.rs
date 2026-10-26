use std::time::{Duration, Instant};

pub(crate) const STALL_TIMEOUT: Duration = Duration::from_millis(1500);

pub(crate) struct SubmissionTime {
    started: Instant,
}

impl SubmissionTime {
    pub(crate) fn now() -> Self {
        Self {
            started: Instant::now(),
        }
    }

    pub(crate) fn stalled(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) > STALL_TIMEOUT
    }
}

#[derive(Default)]
pub(crate) struct SubmissionBarrier {
    generation: u64,
    awaiting_drain: bool,
}

impl SubmissionBarrier {
    pub(crate) fn block_until_events_drained(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.awaiting_drain = true;
    }

    pub(crate) fn drain_token(&self) -> Option<u64> {
        self.awaiting_drain.then_some(self.generation)
    }

    pub(crate) fn events_dispatched(&mut self, token: Option<u64>) {
        if token == self.drain_token() {
            self.awaiting_drain = false;
        }
    }

    pub(crate) fn can_submit(&self) -> bool {
        !self.awaiting_drain
    }
}

#[derive(Default)]
pub(crate) struct Recovery {
    attempts: u8,
    last_attempt: Option<Instant>,
}

impl Recovery {
    pub(crate) fn attempt(&mut self, now: Instant) -> bool {
        if self.attempts >= 2
            || self
                .last_attempt
                .is_some_and(|last| now.saturating_duration_since(last) < STALL_TIMEOUT)
        {
            return false;
        }
        self.attempts += 1;
        self.last_attempt = Some(now);
        true
    }

    pub(crate) fn presented(&mut self, now: Instant) {
        if self
            .last_attempt
            .is_some_and(|last| now.saturating_duration_since(last) >= Duration::from_secs(60))
        {
            self.attempts = 0;
            self.last_attempt = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persistent_stalls_exhaust_the_budget_until_a_healthy_minute() {
        let now = Instant::now();
        let mut recovery = Recovery::default();
        assert!(recovery.attempt(now));
        recovery.presented(now + Duration::from_secs(1));
        assert!(recovery.attempt(now + Duration::from_secs(2)));
        assert!(!recovery.attempt(now + Duration::from_secs(3)));
        recovery.presented(now + Duration::from_secs(62));
        assert!(recovery.attempt(now + Duration::from_secs(63)));
    }

    #[test]
    fn slow_composition_does_not_exhaust_a_new_kms_submission_deadline() {
        let render_started = Instant::now();
        let queued = render_started + Duration::from_secs(3);
        let submission = SubmissionTime { started: queued };
        assert!(!submission.stalled(queued + Duration::from_millis(16)));
        assert!(!submission.stalled(queued + STALL_TIMEOUT));
        assert!(submission.stalled(queued + STALL_TIMEOUT + Duration::from_millis(1)));
    }

    #[test]
    fn recovery_drains_old_flip_events_before_allowing_a_new_submission() {
        let mut barrier = SubmissionBarrier::default();
        assert!(barrier.can_submit());
        barrier.block_until_events_drained();
        assert!(!barrier.can_submit());
        let token = barrier.drain_token();

        barrier.events_dispatched(token);
        assert!(barrier.can_submit());
    }

    #[test]
    fn reset_inside_dispatch_requires_a_subsequent_event_drain() {
        let mut barrier = SubmissionBarrier::default();
        let token = barrier.drain_token();
        barrier.block_until_events_drained();
        barrier.events_dispatched(token);
        assert!(!barrier.can_submit());
        barrier.events_dispatched(barrier.drain_token());
        assert!(barrier.can_submit());
    }

    #[test]
    fn another_reset_during_recovery_drain_keeps_the_barrier_closed() {
        let mut barrier = SubmissionBarrier::default();
        barrier.block_until_events_drained();
        let token = barrier.drain_token();
        barrier.block_until_events_drained();
        barrier.events_dispatched(token);
        assert!(!barrier.can_submit());
        barrier.events_dispatched(barrier.drain_token());
        assert!(barrier.can_submit());
    }

    #[test]
    fn recovery_retries_wait_for_the_driver_without_spending_the_budget() {
        let now = Instant::now();
        let mut recovery = Recovery::default();
        assert!(recovery.attempt(now));
        for delay in [100, 500, 1000, 1499] {
            assert!(!recovery.attempt(now + Duration::from_millis(delay)));
        }
        assert!(recovery.attempt(now + STALL_TIMEOUT));
        assert!(!recovery.attempt(now + Duration::from_secs(3)));
    }
}
