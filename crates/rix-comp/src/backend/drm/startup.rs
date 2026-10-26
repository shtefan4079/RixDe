use anyhow::Context;
use smithay::{backend::session::Session, reexports::calloop::EventLoop};
use std::time::{Duration, Instant};

pub(super) fn initialize_when_active<D, S: Session, T>(
    event_loop: &mut EventLoop<'_, D>,
    state: &mut D,
    session: &mut S,
    timeout: Duration,
    initialize: impl FnOnce(&mut S) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let started = Instant::now();
    while !session.is_active() {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            anyhow::bail!(
                "libseat session on '{}' did not become active within {:.1}s. \
                 The login session must own the active VT before DRM can start; \
                 check the display manager's PAM/logind session and VT configuration.",
                session.seat(),
                timeout.as_secs_f32(),
            );
        }
        event_loop
            .dispatch(Some(remaining.min(Duration::from_millis(100))), state)
            .context("Failed to dispatch the initial libseat activation")?;
    }
    initialize(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::reexports::{
        calloop::timer::{TimeoutAction, Timer},
        rustix::fs::OFlags,
    };
    use std::{cell::Cell, fs::File, os::fd::OwnedFd, path::Path, rc::Rc};

    struct FakeSession {
        active: Rc<Cell<bool>>,
        opened: usize,
        switched: usize,
    }

    impl Session for FakeSession {
        type Error = ();

        fn open(&mut self, _: &Path, _: OFlags) -> Result<OwnedFd, Self::Error> {
            assert!(self.is_active(), "device opened before initial Enable");
            self.opened += 1;
            Ok(File::open("/dev/null").unwrap().into())
        }

        fn close(&mut self, _: OwnedFd) -> Result<(), Self::Error> {
            Ok(())
        }

        fn change_vt(&mut self, _: i32) -> Result<(), Self::Error> {
            self.switched += 1;
            Ok(())
        }

        fn is_active(&self) -> bool {
            self.active.get()
        }

        fn seat(&self) -> String {
            "seat-test".into()
        }
    }

    fn session(active: bool) -> FakeSession {
        FakeSession {
            active: Rc::new(Cell::new(active)),
            opened: 0,
            switched: 0,
        }
    }

    fn open_device(session: &mut FakeSession) -> anyhow::Result<OwnedFd> {
        Ok(session
            .open(Path::new("/dev/null"), OFlags::RDONLY)
            .unwrap())
    }

    #[test]
    fn delayed_enable_is_dispatched_before_device_initialization() {
        let mut event_loop = EventLoop::try_new().unwrap();
        let mut session = session(false);
        let active = session.active.clone();
        event_loop
            .handle()
            .insert_source(
                Timer::from_duration(Duration::from_millis(10)),
                move |_, _, dispatches: &mut usize| {
                    active.set(true);
                    *dispatches += 1;
                    TimeoutAction::Drop
                },
            )
            .unwrap();
        let mut dispatches = 0;
        let _device = initialize_when_active(
            &mut event_loop,
            &mut dispatches,
            &mut session,
            Duration::from_secs(1),
            open_device,
        )
        .unwrap();
        assert_eq!(dispatches, 1);
        assert_eq!(session.opened, 1);
        assert_eq!(session.switched, 0);
    }

    #[test]
    fn already_active_session_initializes_without_dispatching_or_switching_vt() {
        let mut event_loop = EventLoop::try_new().unwrap();
        let mut session = session(true);
        let _device = initialize_when_active(
            &mut event_loop,
            &mut (),
            &mut session,
            Duration::ZERO,
            open_device,
        )
        .unwrap();
        assert_eq!(session.opened, 1);
        assert_eq!(session.switched, 0);
    }

    #[test]
    fn inactive_session_times_out_without_device_or_vt_access() {
        let mut event_loop = EventLoop::try_new().unwrap();
        let mut session = session(false);
        let error = initialize_when_active(
            &mut event_loop,
            &mut (),
            &mut session,
            Duration::from_millis(10),
            open_device,
        )
        .unwrap_err();
        assert!(error.to_string().contains("seat-test"));
        assert!(error.to_string().contains("active VT"));
        assert_eq!(session.opened, 0);
        assert_eq!(session.switched, 0);
    }
}
