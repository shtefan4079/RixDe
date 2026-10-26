use rix_login_ui::LoginWindow;
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::{ComponentHandle, PhysicalSize, Rgb8Pixel};
use std::rc::Rc;

struct PreviewPlatform(Rc<MinimalSoftwareWindow>);
impl Platform for PreviewPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let directory = args
        .first()
        .map(String::as_str)
        .unwrap_or("/tmp/rix-login-preview");
    std::fs::create_dir_all(directory)?;
    let surface = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(PreviewPlatform(surface.clone())))?;
    let window = LoginWindow::new()?;
    window.set_username("rix".into());
    window.set_display_name("Rix".into());
    window.set_time_text("16:42".into());
    window.set_date_text("Sunday, October 4".into());
    window.set_layout_text("EN".into());
    window.set_title_text("Welcome".into());
    window.set_subtitle_text("Your space. Your Rix.".into());
    window.set_action_text("Sign In".into());
    window.set_password_placeholder("Password".into());
    window.set_user_label("USER".into());
    window.set_session_label("SESSION".into());
    window.set_power_label("Power Off".into());
    window.set_restart_label("Restart".into());
    window.set_hint_text("Ready for a fresh start".into());
    rix_login_ui::set_choices(&window, vec!["Rix".into()], vec!["RixDE · Wayland".into()]);
    window.show()?;
    for (name, width, height, lock) in [
        ("login", 1366, 768, false),
        ("lock", 1366, 768, true),
        ("login-compact", 800, 600, false),
    ] {
        window.set_lock_mode(lock);
        window.set_action_text(
            if lock {
                "Unlock"
            } else {
                "Sign In"
            }
            .into(),
        );
        window.set_title_text(
            if lock {
                "Welcome Back"
            } else {
                "Welcome"
            }
            .into(),
        );
        window.set_subtitle_text(
            if lock {
                "Your desktop is waiting for you"
            } else {
                "Your space. Your Rix."
            }
            .into(),
        );
        window.set_hint_text(
            if lock {
                "Session locked"
            } else {
                "Ready for a fresh start"
            }
            .into(),
        );
        surface.set_size(PhysicalSize::new(width, height));
        slint::platform::update_timers_and_animations();
        let mut pixels = vec![Rgb8Pixel::default(); (width * height) as usize];
        surface.request_redraw();
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width as usize);
        });
        let bytes: Vec<u8> = pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
        image::save_buffer(
            format!("{directory}/{name}.png"),
            &bytes,
            width,
            height,
            image::ColorType::Rgb8,
        )?;
    }
    Ok(())
}
