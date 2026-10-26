use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::rc::Rc;
use std::time::Duration;

slint::include_modules!();

pub fn localized<'a>(english: &'a str, _fallback: &'a str) -> &'a str {
    english
}

pub fn set_choices(window: &LoginWindow, users: Vec<String>, sessions: Vec<String>) {
    fn model(values: Vec<String>) -> ModelRc<SharedString> {
        Rc::new(VecModel::from(
            values
                .into_iter()
                .map(SharedString::from)
                .collect::<Vec<_>>(),
        ))
        .into()
    }
    window.set_users(model(users));
    window.set_sessions(model(sessions));
}

pub fn initialize(window: &LoginWindow, lock_mode: bool) -> slint::Timer {
    window.set_lock_mode(lock_mode);
    window.set_title_text(
        if lock_mode {
            "Welcome back"
        } else {
            "Welcome to Rix"
        }
        .into(),
    );
    window.set_subtitle_text(
        if lock_mode {
            "Your desktop is waiting for you"
        } else {
            "Your space. Your Rix."
        }
        .into(),
    );
    window.set_action_text(if lock_mode { "Unlock" } else { "Sign in" }.into());
    window.set_password_placeholder("Password".into());
    window.set_user_label("USER".into());
    window.set_session_label("SESSION".into());
    window.set_power_label("Power Off".into());
    window.set_restart_label("Restart".into());
    window.set_hint_text(
        if lock_mode {
            "Session locked"
        } else {
            "A fresh start awaits"
        }
        .into(),
    );
    window.set_working_text("Verifying…".into());

    fn tick(window: &LoginWindow) {
        let now = chrono::Local::now();
        window.set_time_text(now.format("%H:%M").to_string().into());
        window.set_date_text(now.format("%A, %B %-d").to_string().into());
    }
    tick(window);
    let weak = window.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(1),
        move || {
            if let Some(window) = weak.upgrade() {
                tick(&window);
            }
        },
    );
    timer
}
