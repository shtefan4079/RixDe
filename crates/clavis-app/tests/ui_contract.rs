use clavis_app::{
    apply_manager_theme, apply_prompt_theme, CredentialRow, PasswordWindow, Theme, VaultRow,
    VaultWindow,
};
use i_slint_backend_testing::{ElementHandle, TestingBackend, TestingBackendOptions};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::{cell::Cell, rc::Rc};

fn model<T: Clone + 'static>(values: Vec<T>) -> ModelRc<T> {
    ModelRc::from(Rc::new(VecModel::from(values)))
}
fn save(window: &slint::Window, name: &str) {
    if let Some(dir) = std::env::var_os("CLAVIS_UI_ARTIFACT_DIR") {
        std::fs::create_dir_all(&dir).unwrap();
        let pixels = window.take_snapshot().unwrap();
        image::save_buffer(
            std::path::PathBuf::from(dir).join(name),
            pixels.as_bytes(),
            pixels.width(),
            pixels.height(),
            image::ColorType::Rgba8,
        )
        .unwrap();
    }
}
#[test]
fn vault_states_theme_and_password_confirmation() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    let mut theme = rix_config::ThemeSettings::default();
    let ui = VaultWindow::new().unwrap();
    apply_manager_theme(&ui, &theme);
    ui.window().set_size(slint::LogicalSize::new(1020., 700.));
    ui.set_vaults(model(vec![VaultRow {
        path: "/vault/personal".into(),
        label: "Personal".into(),
        state: "Unlocked · default".into(),
        locked: false,
        default_vault: true,
    }]));
    ui.set_selected_vault(0);
    ui.set_locked(false);
    ui.set_credentials(model(vec![
        CredentialRow {
            path: "/item/1".into(),
            label: "RixBrowser · profile".into(),
            application: "org.rixos.RixBrowser".into(),
            attributes: "application: org.rixos.RixBrowser\nprofile: default".into(),
            modified: "".into(),
        },
        CredentialRow {
            path: "/item/2".into(),
            label: "Steam · account".into(),
            application: "steam-rs".into(),
            attributes: "application: steam-rs".into(),
            modified: "".into(),
        },
    ]));
    ui.set_selected_item(0);
    ui.set_status("Clavis · secrets saved in encrypted vault".into());
    ui.show().unwrap();
    assert_eq!(
        ElementHandle::find_by_accessible_label(&ui, "Add")
            .next()
            .unwrap()
            .accessible_enabled(),
        Some(true)
    );
    save(ui.window(), "manager.png");
    ui.set_revealed("dummy-visible-value".into());
    ui.invoke_clear_secret();
    assert!(ui.get_revealed().is_empty());
    ui.set_locked(true);
    assert_eq!(
        ElementHandle::find_by_accessible_label(&ui, "Add")
            .next()
            .unwrap()
            .accessible_enabled(),
        Some(false)
    );
    save(ui.window(), "locked.png");
    theme.accent_color = "#ee8855".into();
    apply_manager_theme(&ui, &theme);
    assert_eq!(ui.global::<Theme>().get_accent().red(), 238);
    ui.hide().unwrap();
    let prompt = PasswordWindow::new().unwrap();
    apply_prompt_theme(&prompt, &theme);
    prompt
        .window()
        .set_size(slint::LogicalSize::new(500., 420.));
    prompt.set_heading("Password for Personal".into());
    prompt.set_description("Protect application secrets with a vault password.".into());
    prompt.set_confirm_required(true);
    prompt.show().unwrap();
    let button = ElementHandle::find_by_accessible_label(&prompt, "Save Password")
        .next()
        .unwrap();
    assert_eq!(button.accessible_enabled(), Some(false));
    prompt.set_password("dummy-master".into());
    prompt.set_confirmation("other-password".into());
    assert_eq!(button.accessible_enabled(), Some(false));
    let accepted = Rc::new(Cell::new(false));
    let action = accepted.clone();
    prompt.on_submit(move || action.set(true));
    button.invoke_accessible_default_action();
    assert!(!accepted.get());
    prompt.set_confirmation("dummy-master".into());
    assert_eq!(button.accessible_enabled(), Some(true));
    button.invoke_accessible_default_action();
    assert!(accepted.get());
    save(prompt.window(), "password.png");
    prompt.set_password("".into());
    prompt.set_confirmation("".into());
    prompt.hide().unwrap();
}
