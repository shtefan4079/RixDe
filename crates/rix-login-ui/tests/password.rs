use rix_login_ui::LoginWindow;
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::ComponentHandle;
use std::{cell::Cell, rc::Rc};

struct Headless;
impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer))
    }
}

#[test]
fn submission_clears_password_before_callback_and_ignores_busy_or_secondary_screen() {
    slint::platform::set_platform(Box::new(Headless)).unwrap();
    let window = LoginWindow::new().unwrap();
    let calls = Rc::new(Cell::new(0));
    let counted = calls.clone();
    let weak = window.as_weak();
    window.on_submit(move |password| {
        assert_eq!(password, "example-secret");
        assert!(weak.upgrade().unwrap().get_password().is_empty());
        counted.set(counted.get() + 1);
    });
    window.set_password("example-secret".into());
    window.invoke_submit_password();
    assert_eq!(calls.get(), 1);
    window.set_busy(true);
    window.invoke_submit_password();
    window.set_busy(false);
    window.set_secondary_output(true);
    window.invoke_submit_password();
    assert_eq!(calls.get(), 1);
}
