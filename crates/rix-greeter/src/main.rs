use clap::Parser;
use rix_dm::{Catalog, Client, LoginError, PowerAction, Reply, Request};
use rix_login_ui::LoginWindow;
use slint::ComponentHandle;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

fn catalog_guard(catalog: &Mutex<Option<Catalog>>) -> MutexGuard<'_, Option<Catalog>> {
    catalog.lock().unwrap_or_else(|poisoned| {
        tracing::error!("login catalog mutex was poisoned; continuing with its last value");
        poisoned.into_inner()
    })
}

fn harden_process() {
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
        let limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        libc::setrlimit(libc::RLIMIT_CORE, &limit);
    }
}

#[derive(Parser)]
#[command(about = "The graphical login screen for the Rix display manager")]
struct Args {

    #[arg(long)]
    preview: bool,

    #[arg(long, requires = "preview")]
    preview_lock: bool,

    #[arg(long)]
    socket: Option<PathBuf>,
}

fn failure_message(error: &LoginError) -> String {
    match error {
        LoginError::AuthenticationFailed => {
            "Could not sign in. Check your password and try again."
        }
        LoginError::PasswordExpired => {
            "Your password has expired. Change it from a console login."
        }
        LoginError::InvalidSession => {
            "This session is unavailable. Select another."
        }
        LoginError::RateLimited => {
            "Wait a moment before trying again."
        }
        LoginError::SessionAlreadyRunning => {
            "Rix is already running. Return to your desktop and sign out before signing in."
        }
        LoginError::SessionFailed => {
            "The desktop session could not be started."
        }
        LoginError::InvalidRequest => {
            "The login request could not be processed."
        }
    }
    .into()
}

fn catalog_to_ui(window: &LoginWindow, catalog: &Catalog) {
    rix_login_ui::set_choices(
        window,
        catalog
            .users
            .iter()
            .map(|u| u.display_name.clone())
            .chain(std::iter::once(
                "Other user".into(),
            ))
            .collect(),
        catalog.sessions.iter().map(|s| s.name.clone()).collect(),
    );
    if let Some(user) = catalog.users.first() {
        window.set_username(user.username.clone().into());
        window.set_display_name(user.display_name.clone().into());
    }
    window.set_selected_user(0);
    window.set_selected_session(0);
    window.set_manual_user(catalog.users.is_empty());

    window.set_busy(false);
    window.invoke_focus_password();
}

fn main() -> anyhow::Result<()> {
    harden_process();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();
    let _surface = if args.preview {
        None
    } else {
        let controller = rix_shell_adapter::install(rix_shell_adapter::LayerSurfaceSpec {
            namespace: "rix-greeter".into(),
            layer: rix_shell_adapter::ShellLayer::Overlay,
            anchors: rix_shell_adapter::ShellAnchors {
                top: true,
                bottom: true,
                left: true,
                right: true,
            },
            keyboard_interactive: true,
            exclusive_zone: -1,
            ..Default::default()
        })?;
        controller.set_output_fullscreen(true);
        Some(controller)
    };
    let window = LoginWindow::new()?;
    let _clock = rix_login_ui::initialize(&window, args.preview_lock);
    if let Some(surface) = &_surface {
        let weak = window.as_weak();
        surface.on_layout_changed(move |layout, caps_lock| {
            if let Some(window) = weak.upgrade() {
                window.set_layout_text(layout.into());
                window.set_caps_lock(caps_lock);
            }
        });
    }
    if args.preview {
        window.set_username("rix".into());
        window.set_display_name("Rix".into());
        rix_login_ui::set_choices(&window, vec!["Rix".into()], vec!["RixDE · Wayland".into()]);
        window.set_layout_text("EN".into());
        let weak = window.as_weak();
        window.on_submit(move |_| {
            if let Some(window) = weak.upgrade() {
                window.set_status("Preview".into());
            }
        });
        window.show()?;
        window.invoke_focus_password();
        slint::run_event_loop()?;
        return Ok(());
    }
    let socket = Arc::new(args.socket.unwrap_or_else(|| {
        std::env::var_os("RIX_DM_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/run/rix-dm/control.sock".into())
    }));
    let catalog = Arc::new(Mutex::new(None::<Catalog>));

    window.set_busy(true);
    let weak = window.as_weak();
    let catalog_socket = socket.clone();
    let catalog_state = catalog.clone();
    std::thread::spawn(move || {
        let reply = Client::connect(catalog_socket.as_ref())
            .and_then(|mut client| client.request(Request::GetCatalog));
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = weak.upgrade() {

                match reply {
                    Ok(Reply::Catalog(value)) => {
                        catalog_to_ui(&window, &value);
                        *catalog_guard(&catalog_state) = Some(value);
                    }
                    _ => {
                        window.set_status(
                            "The login service is unavailable".into(),
                        );
                        window.set_busy(true);
                    }
                }
            }
        });
    });
    let choices = catalog.clone();
    let weak = window.as_weak();
    window.on_user_selected(move |index| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let catalog = catalog_guard(&choices);
        if let Some(user) = catalog
            .as_ref()
            .and_then(|catalog| catalog.users.get(index.max(0) as usize))
        {
            window.set_username(user.username.clone().into());
            window.set_display_name(user.display_name.clone().into());
            window.set_manual_user(false);
            window.set_status("".into());
        } else {
            window.set_manual_user(true);
            window.set_username("".into());
            window.set_display_name("".into());
            window.set_status("".into());
        }
    });
    let choices = catalog.clone();
    let login_socket = socket.clone();
    let weak = window.as_weak();
    window.on_submit(move |password| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if window.get_busy() {
            return;
        }
        tracing::info!(trigger = %window.get_submission_trigger(), "Login submission requested");
        let session = {
            let catalog = catalog_guard(&choices);
            catalog
                .as_ref()
                .and_then(|catalog| {
                    catalog
                        .sessions
                        .get(window.get_selected_session().max(0) as usize)
                })
                .map(|session| session.id.clone())
        };
        let Some(session) = session else {
            window.set_status(
                "No desktop session is configured".into(),
            );
            return;
        };
        let request = Request::Login {
            username: window.get_username().to_string(),
            password: rix_auth::SecretString::new(password.to_string()),
            session,
        };
        window.set_busy(true);
        window.set_status("".into());
        let socket = login_socket.clone();
        let weak = window.as_weak();
        std::thread::spawn(move || {
            let reply =
                Client::connect(socket.as_ref()).and_then(|mut client| client.request(request));
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    match reply {
                        Ok(Reply::Authenticated) => {
                            let _ = slint::quit_event_loop();
                        }
                        Ok(Reply::Rejected {
                            reason,
                            retry_after_secs,
                        }) => {
                            window.set_status(failure_message(&reason).into());
                            if retry_after_secs > 0 {
                                let weak = window.as_weak();
                                slint::Timer::single_shot(
                                    std::time::Duration::from_secs(retry_after_secs.min(60)),
                                    move || {
                                        if let Some(window) = weak.upgrade() {
                                            window.set_busy(false);
                                            window.invoke_focus_password();
                                        }
                                    },
                                );
                            } else {
                                window.set_busy(false);
                                window.invoke_focus_password();
                            }
                        }
                        _ => {
                            window.set_busy(false);
                            window.set_status(
                                "The login service is unavailable".into(),
                            );
                            window.invoke_focus_password();
                        }
                    }
                }
            });
        });
    });
    let weak = window.as_weak();
    window.on_power_action(move |action| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if window.get_busy() {
            return;
        }
        let action = match action.as_str() {
            "poweroff" => PowerAction::PowerOff,
            "reboot" => PowerAction::Reboot,
            _ => return,
        };
        window.set_busy(true);
        let socket = socket.clone();
        let weak = window.as_weak();
        std::thread::spawn(move || {
            let reply = Client::connect(socket.as_ref())
                .and_then(|mut client| client.request(Request::Power { action }));
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    window.set_busy(false);
                    if !matches!(reply, Ok(Reply::Ok)) {
                        window.set_status(
                            "The power action could not be completed".into(),
                        );
                    }
                }
            });
        });
    });
    window.show()?;
    slint::run_event_loop()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::platform::{
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
        Platform, PlatformError, WindowAdapter, WindowEvent,
    };
    use std::{cell::Cell, rc::Rc};

    struct Headless;
    impl Platform for Headless {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
            Ok(MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer))
        }
    }

    #[test]
    fn loaded_catalog_enables_and_focuses_password_for_the_first_key() {
        slint::platform::set_platform(Box::new(Headless)).unwrap();

        for activate_before_catalog in [false, true] {
            let window = LoginWindow::new().unwrap();
            window.set_busy(true);
            window.show().unwrap();
            if activate_before_catalog {
                window
                    .window()
                    .dispatch_event(WindowEvent::WindowActiveChanged(true));
            }
            catalog_to_ui(
                &window,
                &Catalog {
                    users: vec![rix_dm::User {
                        username: "fixture".into(),
                        display_name: "Fixture user".into(),
                    }],
                    sessions: vec![rix_dm::Session {
                        id: "rix".into(),
                        name: "RixDE".into(),
                    }],
                },
            );

            window.set_busy(false);
            if !activate_before_catalog {
                window
                    .window()
                    .dispatch_event(WindowEvent::WindowActiveChanged(true));
            }
            assert!(!window.get_busy());
            assert_eq!(window.get_username(), "fixture");
            let submitted = Rc::new(Cell::new(false));
            let observed = submitted.clone();
            let weak = window.as_weak();
            window.on_submit(move |password| {
                assert_eq!(password, "a9! ");
                assert_eq!(
                    weak.upgrade().unwrap().get_submission_trigger(),
                    "password-enter"
                );
                observed.set(true);
            });

            for text in ["a", "9", "!", " "] {
                window
                    .window()
                    .dispatch_event(WindowEvent::KeyPressed { text: text.into() });
                window
                    .window()
                    .dispatch_event(WindowEvent::KeyReleased { text: text.into() });
                assert!(!submitted.get(), "Typing must never submit the form");
            }
            assert_eq!(window.get_password(), "a9! ");
            window.window().dispatch_event(WindowEvent::KeyPressed {
                text: slint::platform::Key::Return.into(),
            });
            assert!(submitted.get(), "Enter must submit the focused password");
            assert!(window.get_password().is_empty());
            assert_eq!(window.get_submission_trigger(), "programmatic");
            window.hide().unwrap();
        }
    }
}
