use super::*;
use slint::Model;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires disposable compositor/profile and RIX_TEST_POLISH_DIR"]
async fn native_panel_config_transactions() {
    let output = std::env::var("RIX_TEST_POLISH_DIR").unwrap();
    let config = Arc::new(ConfigManager::init().unwrap());
    let original = config.get_layout();
    let original_theme = config.get_theme();
    let original_desktop = config.get_desktop();
    assert_eq!(original.panels.len(), 1, "Use a fresh disposable profile");
    let window = create_settings_window().unwrap();
    sync_theme_to_ui(&window, &config.get_theme());
    let _editor = panel_editor::install(&window, config.clone());
    let theme_state = crate::theme_editor::State::new(original_theme.clone());
    style_editor::install(&window, theme_state);
    language::install(&window, config.clone());
    wallpaper::install(&window, config.clone());
    power::install(&window, config.clone());
    window.set_active_tab(4);
    let weak = window.as_weak();
    let mut phase = 0;
    let mut ticks = 0;
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            ticks += 1;
            assert!(ticks < 150, "Panel transaction timed out");
            let window = weak.upgrade().unwrap();
            if window.get_busy() {
                return;
            }
            let saved = config.get_layout();
            match phase {
                0 => {
                    window.invoke_add_panel();
                    phase = 1;
                }
                1 => {
                    if saved.panels.len() != 2 {
                        return;
                    }
                    assert_eq!(window.get_selected_panel(), 1);
                    window.set_panel_thickness(64);
                    window.invoke_apply_panel_launcher();
                    phase = 2;
                }
                2 => {
                    if saved.panels[1].thickness != 64 {
                        return;
                    }
                    assert_eq!(
                        saved.panels[0], original.panels[0],
                        "Editing the second panel changed the first"
                    );
                    window.invoke_remove_panel();
                    phase = 3;
                }
                3 => {
                    if saved.panels.len() != 1 {
                        return;
                    }
                    assert_eq!(saved.panels[0], original.panels[0]);
                    window.global::<LanguageSettings>().invoke_choose("en".into());
                    phase = 4;
                }
                4 => {
                    if config.get_desktop().language != "en" { return; }
                    window.global::<WallpaperPreferences>().set_blur(2.);
                    window.global::<WallpaperPreferences>().invoke_apply();
                    phase = 5;
                }
                5 => {
                    if config.get_desktop().wallpaper.blur != 2. { return; }
                    assert_eq!(config.get_desktop().language, "en", "Wallpaper edit lost language");
                    let row = window.get_style_settings().iter().position(|row| row.key == "font_size_md").unwrap();
                    window.invoke_edit_style_setting(row as i32, "16".into());
                    window.invoke_apply_style_settings();
                    phase = 6;
                }
                6 => {
                    if config.get_theme().appearance.metric("font_size_md") != 16. { return; }
                    window.set_active_tab(16);
                    let power = window.global::<PowerPreferences>();
                    power.set_dim_minutes(3);
                    power.set_off_minutes(8);
                    power.set_lock_minutes(5);
                    power.set_sleep_minutes(20);
                    power.set_automatic(true);

                    power.invoke_choose(2);
                    power.set_dim_minutes(4);
                    power.invoke_choose(1);
                    assert_eq!(power.get_dim_minutes(), 3);
                    assert_eq!(power.get_sleep_minutes(), 20);
                    power.invoke_apply();
                    phase = 7;
                }
                7 => {
                    let saved = config.get_desktop();
                    let Some(power) = saved.power else { return; };
                    assert!(power.automatic_battery_plan);
                    let edited = power.plans.iter().find(|plan| plan.id == "balanced").unwrap();
                    assert_eq!(edited.idle.dim_minutes, 3);
                    assert_eq!(edited.idle.screen_off_minutes, 8);
                    assert_eq!(edited.idle.lock_minutes, 5);
                    assert_eq!(edited.idle.sleep_minutes, 20);
                    assert_eq!(power.plans.iter().find(|plan| plan.id == "performance").unwrap().idle.dim_minutes, 4);
                    assert_eq!(config.get_desktop().language, "en");
                    phase = 8;
                    let theme = original_theme.clone();
                    let desktop = original_desktop.clone();
                    crate::jobs::run(&window, move || {
                        tokio::runtime::Handle::current().block_on(async {
                            let mut client = rix_ipc::IpcClient::connect().await?;
                            let rix_ipc::IpcResponse::Theme(base) = client.send(rix_ipc::IpcRequest::GetTheme).await? else { anyhow::bail!("Expected theme") };
                            client.send(rix_ipc::IpcRequest::EditTheme { base, updated: theme }).await?;
                            let rix_ipc::IpcResponse::Desktop(base) = client.send(rix_ipc::IpcRequest::GetDesktop).await? else { anyhow::bail!("Expected desktop") };
                            client.send(rix_ipc::IpcRequest::EditDesktop { base, updated: desktop }).await?;
                            Ok(())
                        })
                    }, |_, ()| {});
                }
                8 => {
                    if config.get_theme() != original_theme || config.get_desktop() != original_desktop { return; }
                    phase = 9;
                    let path = format!("{output}/panel-config-transactions.png");
                    crate::jobs::run(
                        &window,
                        move || {
                            tokio::runtime::Handle::current().block_on(async {
                                let mut client = rix_ipc::IpcClient::connect().await?;
                                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                                loop {
                                    let windows = client.send(rix_ipc::IpcRequest::ListWindows).await?;
                                    if matches!(windows, rix_ipc::IpcResponse::Windows(windows) if windows.iter().any(|window| window.app_id == "org.rixde.Settings")) {
                                        break;
                                    }
                                    anyhow::ensure!(std::time::Instant::now() < deadline, "Settings did not map on the fixture Wayland server");
                                    tokio::time::sleep(Duration::from_millis(50)).await;
                                }
                                let response = client
                                    .send(rix_ipc::IpcRequest::Screenshot { path: Some(path) })
                                    .await?;
                                anyhow::ensure!(
                                    matches!(response, rix_ipc::IpcResponse::ScreenshotSaved(_)),
                                    "Unexpected screenshot response"
                                );
                                Ok(())
                            })
                        },
                        |_, ()| {
                            slint::quit_event_loop().unwrap();
                        },
                    );
                }
                _ => {}
            }
        },
    );
    window.run().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires disposable compositor/profile and RIX_TEST_POLISH_DIR"]
async fn native_power_lock_preferences() {
    let config = Arc::new(ConfigManager::init().unwrap());
    let window = create_settings_window().unwrap();
    sync_theme_to_ui(&window, &config.get_theme());
    power::install(&window, config.clone());
    window.set_active_tab(16);
    let weak = window.as_weak();
    let mut phase = 0;
    let mut ticks = 0;
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(150),
        move || {
            ticks += 1;
            assert!(ticks < 100, "Settings persistence timed out");
            let window = weak.upgrade().unwrap();
            let ui = window.global::<PowerPreferences>();
            if ticks < 4 || window.get_busy() {
                return;
            }
            match phase {
                0 => {
                    ui.set_lock_minutes(17);
                    ui.set_inhibitors(false);
                    ui.set_external_lid(false);
                    ui.invoke_apply();
                    phase = 1;
                }
                1 => {
                    let saved: rix_config::DesktopSettings = serde_json::from_slice(
                        &std::fs::read(config.config_dir().join("desktop.json")).unwrap(),
                    )
                    .unwrap();
                    assert_eq!(
                        saved.power.as_ref().unwrap().plans[ui.get_editing() as usize]
                            .idle
                            .lock_minutes,
                        17
                    );
                    assert!(!saved.session.respect_idle_inhibitors);
                    assert!(!saved.session.keep_running_with_external_display);
                    ui.set_lock_minutes(0);
                    ui.set_external_lid(true);
                    ui.invoke_apply();
                    phase = 2;
                }
                _ => {
                    assert_eq!(
                        config.get_desktop().power.as_ref().unwrap().plans
                            [ui.get_editing() as usize]
                            .idle
                            .lock_minutes,
                        0
                    );
                    assert!(
                        config
                            .get_desktop()
                            .session
                            .keep_running_with_external_display
                    );
                    slint::quit_event_loop().unwrap();
                }
            }
        },
    );
    window.run().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires disposable compositor/profile and RIX_TEST_POLISH_DIR"]
async fn native_slider_drag_keyboard_and_responsive_jobs() {
    let output = std::env::var("RIX_TEST_POLISH_DIR").unwrap();
    let config = ConfigManager::init().unwrap();
    let window = create_settings_window().unwrap();
    sync_theme_to_ui(&window, &config.get_theme());
    window
        .window()
        .set_size(slint::LogicalSize::new(1040., 760.));
    window.set_active_tab(0);
    let weak = window.as_weak();
    let timer = slint::Timer::default();
    let mut tick = 0;
    let mut busy_ticks = 0;
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(50),
        move || {
            tick += 1;
            let w = weak.upgrade().unwrap();
            if w.get_busy() {
                busy_ticks += 1;
            }
            if tick == 8 {
                use slint::platform::{PointerEventButton, WindowEvent};
                let start = slint::LogicalPosition::new(300., 174.);
                let end = slint::LogicalPosition::new(800., 174.);
                w.window().dispatch_event(WindowEvent::PointerPressed {
                    position: start,
                    button: PointerEventButton::Left,
                });
                let before = w.get_panel_opacity();
                w.window()
                    .dispatch_event(WindowEvent::PointerMoved { position: end });
                w.window().dispatch_event(WindowEvent::PointerReleased {
                    position: end,
                    button: PointerEventButton::Left,
                });
                assert!(
                    w.get_panel_opacity() > before + 0.4,
                    "Dragging must move value and thumb together"
                );
                for key in [slint::platform::Key::Home, slint::platform::Key::End] {
                    w.window()
                        .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
                }
                assert_eq!(w.get_panel_opacity(), 1.);
                crate::jobs::run(
                    &w,
                    || {
                        std::thread::sleep(Duration::from_millis(350));
                        Ok(())
                    },
                    |w, ()| w.set_status_message("".into()),
                );
            }
            if tick == 20 {
                assert!(!w.get_busy());
                assert!(
                    busy_ticks >= 3,
                    "UI timers must run while a disk job is pending"
                );
                w.set_active_tab(8);
                w.global::<SystemState>().set_wifi_hardware(true);
                w.global::<SystemState>().set_wifi_enabled(true);
            }
            if tick == 28 {
                let pixels = w.window().take_snapshot().unwrap();
                image::save_buffer(
                    format!("{output}/settings-wifi-toggle.png"),
                    pixels.as_bytes(),
                    pixels.width(),
                    pixels.height(),
                    image::ColorType::Rgba8,
                )
                .unwrap();
                slint::quit_event_loop().unwrap();
            }
        },
    );
    window.run().unwrap();
}

#[test]
#[ignore = "requires a disposable Wayland compositor and RIX_TEST_SETTINGS_PNG"]
fn native_launcher_editor() {
    let output = std::env::var("RIX_TEST_SETTINGS_PNG").expect("Set an output PNG path");
    let config = std::sync::Arc::new(ConfigManager::init().unwrap());
    let window = create_settings_window().unwrap();
    window.set_active_tab(4);
    launcher_editor::install(&window);
    panel_editor::install(&window, config.clone());
    sync_theme_to_ui(&window, &config.get_theme());
    let mut layout = config.get_layout();

    layout.launcher.footer = rix_config::LauncherConfig::default().footer;
    sync_layout_to_ui(&window, &layout);
    let before = panel_editor::widgets(&window, &layout.panels[0].widgets);
    window.invoke_move_panel_widget(0, 1);
    let mut row = window.get_panel_widgets().row_data(1).unwrap();
    row.section = 0;
    row.width = 72;
    window.invoke_edit_panel_widget(1, row);
    let after = panel_editor::widgets(&window, &layout.panels[0].widgets);
    assert_eq!(after[1].id, before[0].id);
    assert_eq!(after[1].properties["width"], 72);
    assert_eq!(after[1].section, rix_config::WidgetSection::Start);
    panel_editor::load(&window, &layout.panels[0]);

    let original = launcher_editor::read(&window);
    window.invoke_move_launcher_action(0, 1);
    let mut edited = window.get_launcher_actions().row_data(1).unwrap();
    edited.label = "Signature check".into();
    edited.icon_only = true;
    edited.end = true;
    window.invoke_edit_launcher_action(1, edited);
    let changed = launcher_editor::read(&window);
    assert_eq!(changed[0], original[1]);
    assert_eq!(changed[1].action, original[0].action);
    assert_eq!(changed[1].label, "Signature check");
    assert!(changed[1].icon_only && changed[1].end);

    launcher_editor::load(&window, &original);

    let weak = window.as_weak();
    let timer = slint::Timer::default();
    let mut ticks = 0;
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            ticks += 1;
            let window = weak.upgrade().unwrap();
            if ticks == 4 {
                let pixels = window.window().take_snapshot().unwrap();
                image::save_buffer(
                    format!("{output}-panel.png"),
                    pixels.as_bytes(),
                    pixels.width(),
                    pixels.height(),
                    image::ColorType::Rgba8,
                )
                .unwrap();
            }
            if ticks == 5 {
                window
                    .window()
                    .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                        position: slint::LogicalPosition::new(700., 400.),
                    });
            }
            if (6..=10).contains(&ticks) {
                window
                    .window()
                    .dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                        position: slint::LogicalPosition::new(700., 400.),
                        delta_x: 0.,
                        delta_y: -160.,
                    });
            }
            if ticks == 45 {
                assert!(
                    window.get_panel_scroll_position() < -100.,
                    "Mouse wheel did not scroll the settings form: {}",
                    window.get_panel_scroll_position()
                );
                let pixels = window.window().take_snapshot().unwrap();
                assert!(pixels.width() >= 900 && pixels.height() >= 600);
                image::save_buffer(
                    &output,
                    pixels.as_bytes(),
                    pixels.width(),
                    pixels.height(),
                    image::ColorType::Rgba8,
                )
                .unwrap();
                window.set_active_tab(0);
            }
            if ticks == 65 {
                let pixels = window.window().take_snapshot().unwrap();
                image::save_buffer(
                    format!("{output}-effects.png"),
                    pixels.as_bytes(),
                    pixels.width(),
                    pixels.height(),
                    image::ColorType::Rgba8,
                )
                .unwrap();
                slint::quit_event_loop().unwrap();
            }
        },
    );
    window.run().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires disposable compositor/profile and RIX_TEST_POLISH_DIR"]
async fn native_new_settings_pages() {
    let output = std::env::var("RIX_TEST_POLISH_DIR").unwrap();
    let config = Arc::new(ConfigManager::init().unwrap());
    let window = create_settings_window().unwrap();
    sync_theme_to_ui(&window, &config.get_theme());
    sync_layout_to_ui(&window, &config.get_layout());
    system_bridge::install(&window, config.clone());
    keyboard::install(&window, config.clone());
    autostart::install(&window, config.clone());
    behavior::install(&window, config);
    let weak = window.as_weak();
    let mut tick = 0;
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(200),
        move || {
            tick += 1;
            let w = weak.upgrade().unwrap();
            if [1, 8, 16, 24, 32].contains(&tick) {
                w.set_active_tab(match tick {
                    1 => 6,
                    8 => 7,
                    16 => 8,
                    24 => 9,
                    _ => 10,
                });
            }
            if [7, 15, 23, 31, 39].contains(&tick) {
                let pixels = w.window().take_snapshot().unwrap();
                image::save_buffer(
                    format!("{output}/settings-{}.png", w.get_active_tab()),
                    pixels.as_bytes(),
                    pixels.width(),
                    pixels.height(),
                    image::ColorType::Rgba8,
                )
                .unwrap();
            }
            if tick == 40 {
                w.set_active_tab(6);
            }
            let click = |x, y| {
                let position = slint::LogicalPosition::new(x, y);
                for event in [
                    slint::platform::WindowEvent::PointerMoved { position },
                    slint::platform::WindowEvent::PointerPressed {
                        position,
                        button: slint::platform::PointerEventButton::Left,
                    },
                    slint::platform::WindowEvent::PointerReleased {
                        position,
                        button: slint::platform::PointerEventButton::Left,
                    },
                ] {
                    w.window().dispatch_event(event);
                }
            };
            if tick == 43 {
                click(620., 407.);
            }
            if tick == 46 {
                w.global::<KeyboardSettings>().set_query("German".into());
                w.global::<KeyboardSettings>()
                    .invoke_search("German".into());
            }
            if tick == 49 {
                let pixels = w.window().take_snapshot().unwrap();
                image::save_buffer(
                    format!("{output}/keyboard-catalog.png"),
                    pixels.as_bytes(),
                    pixels.width(),
                    pixels.height(),
                    image::ColorType::Rgba8,
                )
                .unwrap();
                click(480., 195.);
            }
            if tick == 52 {
                let global = w.global::<KeyboardSettings>();
                assert_eq!(
                    global.get_active().row_count(),
                    4,
                    "Selecting a catalog row must add the layout"
                );
                assert_eq!(global.get_active().row_data(3).unwrap().code, "de");
                global.invoke_move(3, -1);
                global.invoke_apply();
                assert_eq!(global.get_error(), "");
                slint::quit_event_loop().unwrap();
            }
        },
    );
    window.run().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires isolated compositor/profile and RIX_TEST_POLISH_DIR"]
async fn native_wallpaper_and_autostart() {
    use slint::Model;
    let output = std::path::PathBuf::from(std::env::var("RIX_TEST_POLISH_DIR").unwrap());
    let config = Arc::new(ConfigManager::init().unwrap());
    let fixture = tempfile::tempdir().unwrap();
    let apps = fixture.path().join("applications");
    std::fs::create_dir_all(&apps).unwrap();
    std::fs::write(
        apps.join("org.example.AutostartTest.desktop"),
        "[Desktop Entry]\nType=Application\nName=Example startup app\nExec=true\n",
    )
    .unwrap();
    std::env::set_var("XDG_DATA_HOME", fixture.path());
    let photo = output.join("wallpaper-fixture.png");
    let pixels = image::RgbaImage::from_fn(640, 360, |x, y| {
        image::Rgba([(x * 255 / 640) as u8, (y * 190 / 360) as u8, 100, 255])
    });
    pixels.save(&photo).unwrap();
    let mut desktop = config.get_desktop();
    desktop.wallpaper.path = Some(photo.clone());
    config.save_desktop(desktop).unwrap();
    let window = create_settings_window().unwrap();
    sync_theme_to_ui(&window, &config.get_theme());
    autostart::install(&window, config.clone());
    wallpaper::install(&window, config.clone());
    behavior::install(&window, config.clone());
    window.set_active_tab(9);
    let weak = window.as_weak();
    let mut tick = 0;
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(150),
        move || {
            tick += 1;
            let window = weak.upgrade().unwrap();
            if tick == 2 {
                let ui = window.global::<AutostartSettings>();
                ui.set_query("Example startup".into());
                ui.invoke_search(ui.get_query());
                ui.set_picker_open(true);
                assert_eq!(ui.get_apps().row_count(), 1);
            }
            if tick == 5 {
                window
                    .global::<AutostartSettings>()
                    .invoke_add("org.example.AutostartTest".into());
            }
            if tick == 7 {
                let ui = window.global::<AutostartSettings>();
                assert!(ui
                    .get_entries()
                    .iter()
                    .any(|entry| entry.id == "org.example.AutostartTest.desktop" && entry.enabled));
                let shot = window.window().take_snapshot().unwrap();
                image::save_buffer(
                    output.join("autostart.png"),
                    shot.as_bytes(),
                    shot.width(),
                    shot.height(),
                    image::ColorType::Rgba8,
                )
                .unwrap();
                ui.invoke_toggle("org.example.AutostartTest.desktop".into(), false);
                assert!(
                    ui.get_entries()
                        .iter()
                        .any(|entry| entry.id == "org.example.AutostartTest.desktop"
                            && !entry.enabled)
                );
                ui.invoke_remove("org.example.AutostartTest.desktop".into());
                assert!(!ui
                    .get_entries()
                    .iter()
                    .any(|entry| entry.id == "org.example.AutostartTest.desktop"));
                window.set_active_tab(11);
            }
            if tick == 11 {
                let ui = window.global::<WallpaperPreferences>();
                assert!(ui.get_error().is_empty(), "{}", ui.get_error());
                ui.set_blur(18.);
                ui.set_dim(0.2);
                ui.set_fit(1);
                ui.invoke_apply();
                let saved = config.get_desktop().wallpaper;
                assert_eq!(saved.path, Some(photo.clone()));
                assert_eq!(saved.blur, 18.);
                assert_eq!(saved.fit, rix_config::WallpaperFit::Fit);
                let shot = window.window().take_snapshot().unwrap();
                image::save_buffer(
                    output.join("wallpaper-settings.png"),
                    shot.as_bytes(),
                    shot.width(),
                    shot.height(),
                    image::ColorType::Rgba8,
                )
                .unwrap();
            }
            if tick == 15 {
                slint::quit_event_loop().unwrap();
            }
        },
    );
    window.run().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires disposable compositor/profile and RIX_TEST_POLISH_DIR"]
async fn native_decoration_tracks_client_resize() {
    let output = std::env::var("RIX_TEST_POLISH_DIR").unwrap();
    let config = ConfigManager::init().unwrap();
    let window = create_settings_window().unwrap();
    sync_theme_to_ui(&window, &config.get_theme());
    window
        .window()
        .set_size(slint::LogicalSize::new(900., 640.));
    let weak = window.as_weak();
    let timer = slint::Timer::default();
    let mut tick = 0;
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(60),
        move || {
            tick += 1;
            let w = weak.upgrade().unwrap();
            if (10..=30).contains(&tick) {
                w.window().set_size(slint::LogicalSize::new(
                    900. + (tick - 10) as f32 * 12.,
                    640. + (tick - 10) as f32 * 5.,
                ));
            }
            if [9, 20, 40].contains(&tick) {
                let path = format!("{output}/decoration-resize-{tick}.png");
                std::thread::spawn(move || {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap()
                        .block_on(async move {
                            let mut client = rix_ipc::IpcClient::connect().await.unwrap();
                            let result = client
                                .send(rix_ipc::IpcRequest::Screenshot { path: Some(path) })
                                .await
                                .unwrap();
                            assert!(!matches!(result, rix_ipc::IpcResponse::Error(_)));
                        });
                })
                .join()
                .unwrap();
            }
            if tick == 45 {
                slint::quit_event_loop().unwrap();
            }
        },
    );
    window.run().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires disposable compositor, bundled resources and fresh HOME"]
async fn native_bundled_theme_package() {
    let config = Arc::new(ConfigManager::init().unwrap());
    let original = config.get_layout();
    let window = create_settings_window().unwrap();
    window.set_icon_themes(ModelRc::from(Rc::new(VecModel::from(
        scan_icon_themes()
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    ))));
    window.set_cursor_themes(ModelRc::from(Rc::new(VecModel::from(
        scan_cursor_themes()
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    ))));
    let editor = crate::theme_editor::State::new(config.get_theme());
    editor.load(&window, &config.get_theme());
    bundled_themes::install(&window, editor);
    let weak = window.as_weak();
    let output = std::env::var("RIX_TEST_POLISH_DIR").unwrap();
    let mut phase = 0;
    let mut ticks = 0;
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(150),
        move || {
            ticks += 1;
            assert!(ticks < 140, "Portable theme selection timed out");
            let window = weak.upgrade().unwrap();
            if window.get_busy() {
                return;
            }
            let global = window.global::<BundledThemes>();
            match phase {
                0 => {
                    let Some(package) = global
                        .get_packages()
                        .iter()
                        .find(|package| package.id == "fairywren-dark")
                    else {
                        return;
                    };
                    global.set_include_layout(false);
                    global.invoke_apply(package.id);
                    phase = 1;
                }
                1 => {
                    if config.get_theme().icon_theme != "Rix-FairyWren-Dark" {
                        return;
                    }
                    assert_eq!(
                        config.get_layout(),
                        original,
                        "Appearance-only selection changed panel or preferences"
                    );
                    let theme = config.get_theme();
                    assert_eq!(window.get_icon_themes().row_data(window.get_selected_icon_index() as usize).unwrap(), theme.icon_theme);
                    assert_eq!(window.get_cursor_themes().row_data(window.get_selected_cursor_index() as usize).unwrap(), theme.cursor_theme);
                    let mut icons = rix_assets::IconResolver::default();
                    for name in [
                        "duckstation",
                        "preferences-system-power-management",
                        "network-wireless",
                        "edit-copy",
                    ] {
                        let path = icons.resolve(name, &theme.icon_theme, 32, None).unwrap();
                        assert!(
                            path.to_str().unwrap().contains("resource-cache"),
                            "Icon came from the host: {path:?}"
                        );
                    }
                    assert!(
                        rix_assets::cursor::load(&theme.cursor_theme, &["left_ptr"], 24).is_some()
                    );
                    for reference in [
                        &theme.appearance.panel_frame,
                        &theme.appearance.task_active_frame,
                        &theme.appearance.menu_frame,
                    ] {
                        let (path, prefix) = reference.split_once('#').unwrap_or((reference, ""));
                        let path = rix_assets::resolve(path).unwrap();
                        assert!(path.to_str().unwrap().contains("resource-cache"));
                        assert!(rix_assets::load_frame_at_scale(&path, prefix, 2).is_ok());
                    }
                    assert!(
                        rix_assets::resolve("aurorae:Rix-ChromeOS-dark/decoration.svg")
                            .unwrap()
                            .is_file()
                    );
                    global.set_include_layout(true);
                    global.invoke_apply("fairywren-dark".into());
                    phase = 2;
                }
                2 => {
                    let layout = config.get_layout();
                    if layout.launcher.width != 697 {
                        return;
                    }
                    assert_eq!(layout.launcher.height, 583);
                    assert_eq!(layout.launcher.favorites, original.launcher.favorites);
                    assert_eq!(layout.keyboard, original.keyboard);
                    phase = 3;
                    let path = format!("{output}/bundled-theme.png");
                    crate::jobs::run(
                        &window,
                        move || {
                            tokio::runtime::Handle::current().block_on(async {
                                let mut client = rix_ipc::IpcClient::connect().await?;
                                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                                loop {
                                    let response = client.send(rix_ipc::IpcRequest::ListWindows).await?;
                                    if matches!(response, rix_ipc::IpcResponse::Windows(windows) if windows.iter().any(|window| window.app_id == "org.rixde.Settings")) { break; }
                                    anyhow::ensure!(std::time::Instant::now() < deadline, "Settings did not map on the fixture");
                                    tokio::time::sleep(Duration::from_millis(50)).await;
                                }
                                let settings_path = path.replace("bundled-theme.png", "bundled-theme-settings.png");
                                let response = client.send(rix_ipc::IpcRequest::Screenshot { path: Some(settings_path) }).await?;
                                anyhow::ensure!(matches!(response, rix_ipc::IpcResponse::ScreenshotSaved(_)), "Settings screenshot failed");
                                client.send(rix_ipc::IpcRequest::SetLauncher(true)).await?;
                                tokio::time::sleep(Duration::from_millis(600)).await;
                                let response = client
                                    .send(rix_ipc::IpcRequest::Screenshot { path: Some(path) })
                                    .await?;
                                anyhow::ensure!(
                                    matches!(response, rix_ipc::IpcResponse::ScreenshotSaved(_)),
                                    "Unexpected screenshot response: {response:?}"
                                );
                                Ok(())
                            })
                        },
                        |_, ()| slint::quit_event_loop().unwrap(),
                    );
                }
                _ => {}
            }
        },
    );
    window.run().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires disposable compositor/profile and RIX_TEST_POLISH_DIR"]
async fn native_window_appearance_transactions() {
    let output = std::path::PathBuf::from(std::env::var("RIX_TEST_POLISH_DIR").unwrap());
    let config = Arc::new(ConfigManager::init().unwrap());
    let original = config.get_desktop();
    let window = create_settings_window().unwrap();
    sync_theme_to_ui(&window, &config.get_theme());
    window_appearance::install(&window, config.clone());
    window.set_active_tab(17);
    let weak = window.as_weak();
    let mut phase = 0;
    let mut ticks = 0;
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            ticks += 1;
            assert!(
                ticks < 180,
                "Native window appearance transaction timed out"
            );
            let window = weak.upgrade().unwrap();
            if window.get_busy() {
                return;
            }
            let ui = window.global::<WindowAppearancePreferences>();
            match phase {
                0 => {
                    ui.invoke_add_rule();
                    ui.invoke_refresh_apps();
                    phase = 1;
                }
                1 => {
                    if ui.get_refreshing() {
                        return;
                    }
                    let Some(index) = ui
                        .get_running_apps()
                        .iter()
                        .position(|name| name.as_str().contains("org.rixde.Settings"))
                    else {
                        ui.invoke_refresh_apps();
                        return;
                    };
                    ui.invoke_choose_running(index as i32);
                    assert_eq!(ui.get_app_id(), "org.rixde.Settings");
                    ui.invoke_choose_theme(4);
                    ui.set_rule_material(false);
                    ui.invoke_store_rule();
                    assert!(!ui.get_editor_open(), "{}", ui.get_status());
                    ui.invoke_apply();
                    phase = 2;
                }
                2 => {
                    let saved: rix_config::DesktopSettings = serde_json::from_slice(
                        &std::fs::read(config.config_dir().join("desktop.json")).unwrap(),
                    )
                    .unwrap();
                    let Some(rule) = saved
                        .window_appearance
                        .rules
                        .iter()
                        .find(|rule| rule.app_id == "org.rixde.Settings")
                    else {
                        return;
                    };
                    assert!(!rule.use_desktop_material);
                    assert_eq!(rule.theme, Some(ThemeSettings::crisp_light()));
                    let mut expected = original.clone();
                    expected.window_appearance = saved.window_appearance.clone();
                    assert_eq!(
                        saved, expected,
                        "Appearance transaction lost an unrelated desktop preference"
                    );
                    let pixels = window.window().take_snapshot().unwrap();
                    image::save_buffer(
                        output.join("settings-window-appearance.png"),
                        pixels.as_bytes(),
                        pixels.width(),
                        pixels.height(),
                        image::ColorType::Rgba8,
                    )
                    .unwrap();
                    let base = saved;
                    let original = original.clone();
                    crate::jobs::run(
                        &window,
                        move || crate::jobs::commit_desktop(base, original),
                        |_, _| {},
                    );
                    phase = 3;
                }
                _ => {
                    if config.get_desktop() != original {
                        return;
                    }
                    slint::quit_event_loop().unwrap();
                }
            }
        },
    );
    window.run().unwrap();
}
