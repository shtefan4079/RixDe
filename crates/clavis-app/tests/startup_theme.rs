use clavis_app::{apply_manager_theme, Theme, VaultWindow};
use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
use slint::ComponentHandle;

#[test]
fn first_frame_keeps_app_artwork_without_preparing_unused_taskbar_states() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let frame = directory.path().join("frame.svg");
    let parts = [
        "topleft",
        "top",
        "topright",
        "left",
        "center",
        "right",
        "bottomleft",
        "bottom",
        "bottomright",
    ];
    let artwork = parts
        .iter()
        .enumerate()
        .map(|(i, name)| {
            format!(
                r##"<rect id="{name}" x="{}" y="{}" width="4" height="4" fill="#397759"/>"##,
                i % 3 * 4,
                i / 3 * 4
            )
        })
        .collect::<String>();
    std::fs::write(
        &frame,
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="12" height="12">{artwork}</svg>"#
        ),
    )
    .unwrap();
    let reference = frame.to_string_lossy().into_owned();
    let mut settings = rix_config::ThemeSettings::default();
    settings.appearance.panel_frame = reference.clone();
    settings.appearance.menu_frame = reference.clone();

    settings.appearance.task_active_frame = reference.clone();
    settings.appearance.task_hover_frame = reference;
    settings.appearance.font_family = "DejaVu Sans".into();
    settings.accent_color = "#13a5ce".into();
    let ui = VaultWindow::new().unwrap();
    apply_manager_theme(&ui, &settings);
    let theme = ui.global::<Theme>();
    assert!(theme.get_panel_frame().available);
    assert!(theme.get_menu_frame().available);
    assert!(!theme.get_task_active_frame().available);
    assert!(!theme.get_task_hover_frame().available);
    assert_eq!(theme.get_font_family(), "DejaVu Sans");
    assert_eq!(theme.get_accent().red(), 0x13);
    let prepared = theme.get_menu_frame().cache_key;
    ui.window().set_size(slint::LogicalSize::new(1020., 700.));
    ui.show().unwrap();
    assert_eq!(theme.get_menu_frame().cache_key, prepared);
    assert_eq!(theme.get_material_opacity(), settings.opacity);

    apply_manager_theme(&ui, &settings);
    assert_eq!(theme.get_menu_frame().cache_key, prepared);
    ui.hide().unwrap();
}
