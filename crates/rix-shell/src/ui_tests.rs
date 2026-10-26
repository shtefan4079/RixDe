use super::*;
use slint::SharedString;
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
#[ignore = "requires a disposable Wayland compositor and profile"]
fn native_panel_popup_anchor() {
    let config = ConfigManager::init().unwrap();
    let surface = rix_shell_adapter::install(Default::default()).unwrap();
    let window = RixPanelWindow::new().unwrap();
    install_surface_geometry(&window, &surface);
    let weak = window.as_weak();
    surface.on_output_size_changed(move |_, height| {
        if let Some(window) = weak.upgrade() {
            window.set_available_height(height as i32);
        }
    });
    window.set_panel_width(config.get_display().width as i32);
    window.set_available_height(config.get_display().height as i32);
    let weak = window.as_weak();
    window.on_reflow_panel(move || {
        if let Some(window) = weak.upgrade() {
            layout::reflow(&window);
        }
    });
    apply_shell_theme(&window, &config.get_theme());
    apply_layout_to_ui(
        &window,
        &config.get_layout(),
        &[],
        config.get_desktop().clipboard.enabled,
    );
    let observed = Rc::new(std::cell::RefCell::new(None::<(String, f32)>));
    let saved = observed.clone();
    window.on_open_system_popup(move |kind, x| {
        *saved.borrow_mut() = Some((kind.to_string(), x));
    });
    let weak = window.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::SingleShot,
        Duration::from_millis(500),
        move || {
            let window = weak.upgrade().unwrap();
            let clock = window
                .get_widgets()
                .iter()
                .find(|item| item.kind == "clock")
                .unwrap();
            let x = window.get_panel_offset() + clock.x + clock.width / 2.;
            let y = window.window().size().height as f32 / window.window().scale_factor()
                - window.get_thickness() as f32 / 2.;
            let click = |x| {
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
                    window.window().dispatch_event(event);
                }
            };
            click(x);
            {
                let actual = observed.borrow();
                let (kind, anchor) = actual.as_ref().expect("Clock click must open a popup");
                assert_eq!(kind, "calendar");
                assert!(
                    (anchor - x).abs() < 1.,
                    "Calendar anchor {anchor} != clock {x}"
                );
            }
            let tray = window
                .get_widgets()
                .iter()
                .find(|item| item.kind == "tray")
                .unwrap();
            let modules = window.get_system_modules().iter().collect::<Vec<_>>();
            let cell_width = tray.width / modules.len() as f32;
            for (index, module) in modules.iter().enumerate() {
                *observed.borrow_mut() = None;
                let x = window.get_panel_offset() + tray.x + (index as f32 + 0.5) * cell_width;
                click(x);
                let actual = observed.borrow();
                let (kind, anchor) = actual
                    .as_ref()
                    .expect("Tray module click must open a popup");
                assert_eq!(kind, module.as_str());
                assert!(
                    (anchor - x).abs() < 1.,
                    "{kind} anchor {anchor} != module {x}"
                );
            }
            slint::quit_event_loop().unwrap();
        },
    );
    window.run().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a disposable Wayland compositor and RIX_TEST_TRAY_ADDRESS"]
async fn native_tray_popup_smoke() {
    let address = std::env::var("RIX_TEST_TRAY_ADDRESS").expect("Set a StatusNotifier address");
    let output = std::env::var("RIX_TEST_POPUP_PNG").expect("Set an output PNG path");
    let config = ConfigManager::init().unwrap();
    set_active_icon_theme(&config.get_theme().icon_theme);
    let surface = rix_shell_adapter::install(Default::default()).unwrap();
    let window = RixPanelWindow::new().unwrap();
    install_surface_geometry(&window, &surface);
    let fullscreen_surface = surface.clone();
    window.on_surface_output_fullscreen_changed(move |fullscreen| {
        fullscreen_surface.set_output_fullscreen(fullscreen)
    });
    let overlay_surface = surface.clone();
    window.on_surface_overlay_changed(move |active| overlay_surface.set_overlay_active(active));
    window
        .on_surface_state_changed(move |interactive| surface.set_keyboard_interactive(interactive));
    popup::install(&window);
    tray::install(&window);
    let close = window.as_weak();
    window.on_close_context_menu(move || {
        if let Some(window) = close.upgrade() {
            window.set_is_context_menu_open(false);
            window.invoke_tray_menu_closed();
        }
    });
    apply_shell_theme(&window, &config.get_theme());
    apply_layout_to_ui(
        &window,
        &config.get_layout(),
        &[],
        config.get_desktop().clipboard.enabled,
    );
    popup::set_items(
        &window,
        vec![
            ContextMenuItemData {
                label: "disabled".into(),
                disabled: true,
                ..Default::default()
            },
            ContextMenuItemData {
                label: "first".into(),
                ..Default::default()
            },
            ContextMenuItemData {
                separator: true,
                ..Default::default()
            },
            ContextMenuItemData {
                label: "last".into(),
                ..Default::default()
            },
        ],
    );
    window.invoke_navigate_context_menu(1);
    assert_eq!(window.get_context_selected_index(), 1);
    window.invoke_navigate_context_menu(1);
    assert_eq!(window.get_context_selected_index(), 3);
    window.invoke_navigate_context_menu(1);
    assert_eq!(window.get_context_selected_index(), 1);
    window.invoke_context_select_edge(true);
    assert_eq!(window.get_context_selected_index(), 3);
    window.invoke_context_select_edge(false);
    assert_eq!(window.get_context_selected_index(), 1);
    window.set_is_context_menu_open(true);
    window.set_is_app_menu_open(true);
    popup::dismiss_all(&window);
    assert!(!window.get_is_context_menu_open() && !window.get_is_app_menu_open());

    let weak = window.as_weak();
    let timer = slint::Timer::default();
    let success = Arc::new(AtomicBool::new(false));
    let captured = success.clone();
    let launched = Arc::new(AtomicBool::new(false));
    let launch_seen = launched.clone();
    window.on_launch_app(move |target| {
        assert_eq!(target.as_str(), "test://selected");
        launch_seen.store(true, Ordering::Relaxed);
    });
    let mut ticks = 0;
    let mut reopened = false;
    let mut launcher_tick = None;
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            ticks += 1;
            let window = weak.upgrade().unwrap();
            if let Some(opened_at) = launcher_tick {
                if ticks >= opened_at + 3 {
                    assert!(window.window().size().height >= config.get_layout().launcher.height);
                    for key in [
                        slint::platform::Key::DownArrow,
                        slint::platform::Key::Return,
                    ] {
                        let text: slint::SharedString = key.into();
                        window
                            .window()
                            .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                                text: text.clone(),
                            });
                        window
                            .window()
                            .dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
                    }
                    assert!(
                        launched.load(Ordering::Relaxed),
                        "Enter did not activate the selected launcher entry"
                    );
                    let pixels = window.window().take_snapshot().unwrap();
                    image::save_buffer(
                        format!("{output}-launcher.png"),
                        pixels.as_bytes(),
                        pixels.width(),
                        pixels.height(),
                        image::ColorType::Rgba8,
                    )
                    .unwrap();
                    captured.store(true, Ordering::Relaxed);
                    slint::quit_event_loop().unwrap();
                }
                return;
            }
            if ticks == 5 {
                window.invoke_tray_action(address.clone().into(), 1, 600., 5.);
            }
            if ticks > 8 && window.get_is_context_menu_open() {
                if !reopened {
                    reopened = true;
                    window.invoke_tray_action(
                        address.clone().into(),
                        1,
                        650.,
                        window.window().size().height as f32 / window.window().scale_factor() - 5.,
                    );
                    return;
                }
                assert_eq!(window.get_context_menu_x(), 650.);
                let rows = window.get_context_menu_items();
                assert!(rows.row_count() > 0);
                window.invoke_navigate_context_menu(1);
                let selected = rows
                    .row_data(window.get_context_selected_index() as usize)
                    .unwrap();
                assert!(!selected.disabled && !selected.separator);
                let pixels = window.window().take_snapshot().unwrap();
                image::save_buffer(
                    &output,
                    pixels.as_bytes(),
                    pixels.width(),
                    pixels.height(),
                    image::ColorType::Rgba8,
                )
                .unwrap();
                window.set_favorite_apps(ModelRc::from(Rc::new(VecModel::from(vec![
                    AppEntryData {
                        name: "Keyboard target".into(),
                        exec: "test://selected".into(),
                        ..Default::default()
                    },
                ]))));
                launcher::set_open(&window, true);
                assert!(
                    !window.get_is_context_menu_open(),
                    "Opening the launcher must close the service popup"
                );
                launcher_tick = Some(ticks);
            }
            if ticks >= 80 {
                slint::quit_event_loop().unwrap();
            }
        },
    );
    window.run().unwrap();
    assert!(success.load(Ordering::Relaxed), "Tray menu did not open");
}

#[test]
#[ignore = "requires a disposable Wayland compositor and RIX_TEST_CONTEXT_PNG"]
fn native_context_keyboard_and_click_away() {
    let output = std::env::var("RIX_TEST_CONTEXT_PNG").unwrap();
    let config = ConfigManager::init().unwrap();
    let surface = rix_shell_adapter::install(Default::default()).unwrap();
    let window = RixPanelWindow::new().unwrap();
    install_surface_geometry(&window, &surface);
    let fullscreen_surface = surface.clone();
    window.on_surface_output_fullscreen_changed(move |fullscreen| {
        fullscreen_surface.set_output_fullscreen(fullscreen)
    });
    let overlay_surface = surface.clone();
    window.on_surface_overlay_changed(move |active| overlay_surface.set_overlay_active(active));
    window
        .on_surface_state_changed(move |interactive| surface.set_keyboard_interactive(interactive));
    apply_shell_theme(&window, &config.get_theme());
    popup::install(&window);
    let weak = window.as_weak();
    window.on_close_context_menu(move || {
        if let Some(w) = weak.upgrade() {
            popup::close(&w);
        }
    });
    let selected = Rc::new(std::cell::RefCell::new(String::new()));
    let action = selected.clone();
    let weak = window.as_weak();
    window.on_context_menu_selected(move |id, _, _| {
        *action.borrow_mut() = id.to_string();
        if let Some(w) = weak.upgrade() {
            popup::close(&w);
        }
    });
    let weak = window.as_weak();
    let timer = slint::Timer::default();
    let passed = Rc::new(std::cell::Cell::new(false));
    let done = passed.clone();
    let mut ticks = 0;
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            ticks += 1;
            let w = weak.upgrade().unwrap();
            let key = |key: slint::platform::Key| {
                let text: slint::SharedString = key.into();
                w.window()
                    .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                        text: text.clone(),
                    });
                w.window()
                    .dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
            };
            match ticks {
                5 | 15 | 22 => {
                    let y = w.window().size().height as f32 / w.window().scale_factor() - 8.;
                    w.invoke_panel_context_menu(500., y);
                }
                9 => {
                    assert!(w.get_is_context_menu_open());
                    let theme = w.global::<Theme>();
                    let row = w.get_context_menu_items().row_data(1).unwrap();
                    let menu_height =
                        w.get_context_content_height() + 2. * theme.get_default_spacing();
                    let y = (w.window().size().height as f32 / w.window().scale_factor()
                        - w.get_context_menu_bottom()
                        - menu_height)
                        .max(theme.get_spacing_xs());
                    w.window()
                        .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                            position: slint::LogicalPosition::new(
                                540.,
                                y + theme.get_default_spacing() + row.row_y + row.row_height / 2.,
                            ),
                        });
                    assert_eq!(
                        w.get_context_selected_index(),
                        1,
                        "Mouse must set the same selection used by keyboard"
                    );
                    key(slint::platform::Key::UpArrow);
                    assert_eq!(w.get_context_selected_index(), 0);
                }
                12 => {
                    let pixels = w.window().take_snapshot().unwrap();
                    image::save_buffer(
                        &output,
                        pixels.as_bytes(),
                        pixels.width(),
                        pixels.height(),
                        image::ColorType::Rgba8,
                    )
                    .unwrap();
                    key(slint::platform::Key::Return);
                    assert_eq!(selected.borrow().as_str(), "panel-settings");
                    assert!(!w.get_is_context_menu_open());
                }
                18 => {
                    let position =
                        slint::LogicalPosition::new(10., w.window().size().height as f32 - 4.);
                    w.window()
                        .dispatch_event(slint::platform::WindowEvent::PointerPressed {
                            position,
                            button: slint::platform::PointerEventButton::Right,
                        });
                    w.window()
                        .dispatch_event(slint::platform::WindowEvent::PointerReleased {
                            position,
                            button: slint::platform::PointerEventButton::Right,
                        });
                    assert!(
                        !w.get_is_context_menu_open(),
                        "Click outside must dismiss without activating another item"
                    );
                }
                25 => {
                    key(slint::platform::Key::Escape);
                    assert!(!w.get_is_context_menu_open());
                    done.set(true);
                    slint::quit_event_loop().unwrap();
                }
                _ => {}
            }
        },
    );
    window.run().unwrap();
    assert!(passed.get());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a disposable compositor/profile and RIX_TEST_POLISH_DIR"]
async fn native_panel_polish() {
    let output = std::env::var("RIX_TEST_POLISH_DIR").unwrap();
    let config = Arc::new(ConfigManager::init().unwrap());
    set_active_icon_theme(&config.get_theme().icon_theme);
    let surface = rix_shell_adapter::install(Default::default()).unwrap();
    SHELL_SURFACE.with(|s| *s.borrow_mut() = Some(surface.clone()));
    let window = RixPanelWindow::new().unwrap();
    install_surface_geometry(&window, &surface);
    let weak = window.as_weak();
    window.on_reflow_panel(move || {
        if let Some(w) = weak.upgrade() {
            layout::reflow(&w);
        }
    });
    let fullscreen_surface = surface.clone();
    window.on_surface_output_fullscreen_changed(move |fullscreen| {
        fullscreen_surface.set_output_fullscreen(fullscreen)
    });
    let overlay_surface = surface.clone();
    window.on_surface_overlay_changed(move |active| overlay_surface.set_overlay_active(active));
    window
        .on_surface_state_changed(move |interactive| surface.set_keyboard_interactive(interactive));
    apply_shell_theme(&window, &config.get_theme());
    apply_layout_to_ui(
        &window,
        &config.get_layout(),
        &[],
        config.get_desktop().clipboard.enabled,
    );
    panel_editor::install(&window, config.clone(), Arc::new(vec![]));
    popup::install(&window);
    let mut pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(32, 32);
    for pixel in pixels.make_mut_slice() {
        *pixel = slint::Rgba8Pixel::new(250, 17, 83, 255);
    }
    let icon = slint::Image::from_rgba8(pixels);
    set_taskbar(
        &window,
        vec![TaskbarItemData {
            id: 1,
            icon: icon.clone(),
            has_icon: true,
            is_active: true,
            is_running: true,
            ..Default::default()
        }],
    );
    let entries = (0..40)
        .map(|i| AppEntryData {
            name: format!("Application {i:02}").into(),
            exec: format!("item-{i}").into(),
            icon: icon.clone(),
            has_icon: true,
            ..Default::default()
        })
        .collect::<Vec<_>>();
    window.set_favorite_apps(ModelRc::from(Rc::new(VecModel::from(
        entries[..24].to_vec(),
    ))));
    window.set_apps(ModelRc::from(Rc::new(VecModel::from(entries))));
    let launched = Rc::new(std::cell::RefCell::new(String::new()));
    let seen = launched.clone();
    window.on_launch_app(move |exec| *seen.borrow_mut() = exec.to_string());
    let timer = slint::Timer::default();
    let weak = window.as_weak();
    let success = Rc::new(std::cell::Cell::new(false));
    let done = success.clone();
    let mut ticks = 0;
    let deadline = std::time::Instant::now() + Duration::from_secs(40);
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            ticks += 1;
            let w = weak.upgrade().unwrap();
            assert!(
                std::time::Instant::now() < deadline,
                "Compositor did not finish resizing"
            );
            if matches!(ticks, 4 | 8) && w.window().size().height != w.get_thickness() as u32 {
                ticks -= 1;
                return;
            }
            let snapshot = |name: &str| {
                let pixels = w.window().take_snapshot().unwrap();
                image::save_buffer(
                    format!("{output}/{name}.png"),
                    pixels.as_bytes(),
                    pixels.width(),
                    pixels.height(),
                    image::ColorType::Rgba8,
                )
                .unwrap();
                pixels
            };
            let pointer = |x: f32, y: f32, kind: i32| {
                use slint::platform::{PointerEventButton, WindowEvent};
                let position = slint::LogicalPosition::new(x, y);
                w.window().dispatch_event(match kind {
                    0 => WindowEvent::PointerMoved { position },
                    1 => WindowEvent::PointerPressed {
                        position,
                        button: PointerEventButton::Left,
                    },
                    _ => WindowEvent::PointerReleased {
                        position,
                        button: PointerEventButton::Left,
                    },
                });
            };
            let click = |x, y| {
                pointer(x, y, 0);
                pointer(x, y, 1);
                pointer(x, y, 2);
            };
            let theme = w.global::<Theme>();
            let menu_x = (w.get_launcher_x() + w.get_launcher_width() * w.get_menu_alignment()
                - w.get_menu_width() as f32 * w.get_menu_alignment())
            .clamp(
                0.,
                w.window().size().width as f32 - w.get_menu_width() as f32,
            );
            match ticks {
                4 | 8 => {
                    let pixels = snapshot(&format!("icons-{}", w.get_thickness()));
                    let mut bounds = (u32::MAX, 0);
                    let mut x_bounds = (u32::MAX, 0);
                    for (i, p) in pixels.as_slice().iter().enumerate() {
                        if (p.r, p.g, p.b) == (250, 17, 83) {
                            let y = i as u32 / pixels.width();
                            let x = i as u32 % pixels.width();
                            x_bounds.0 = x_bounds.0.min(x);
                            x_bounds.1 = x_bounds.1.max(x);
                            bounds.0 = bounds.0.min(y);
                            bounds.1 = bounds.1.max(y);
                        }
                    }
                    assert_ne!(bounds.0, u32::MAX, "Task icon was not painted");
                    let margin = if w.get_floating() {
                        w.get_floating_margin()
                    } else {
                        0
                    } as f32;
                    let expected = pixels.height() as f32 - margin - w.get_thickness() as f32 / 2.;
                    assert!(
                        ((bounds.0 + bounds.1 + 1) as f32 / 2. - expected).abs() <= 0.5,
                        "Icon bounds {bounds:?} should be centred at {expected}"
                    );
                    let task = w
                        .get_widgets()
                        .iter()
                        .find(|row| row.kind == "taskbar")
                        .unwrap();
                    let expected_x = w.get_panel_offset() + task.x + theme.get_task_width() / 2.;
                    assert!(
                        ((x_bounds.0 + x_bounds.1 + 1) as f32 / 2. - expected_x).abs() <= 0.5,
                        "Icon must be centred horizontally"
                    );
                    if ticks == 4 {
                        w.invoke_set_panel_option("height".into(), 80.);
                    } else {
                        w.set_favorite_apps(ModelRc::from(Rc::new(VecModel::from(
                            w.get_apps().iter().take(24).collect::<Vec<_>>(),
                        ))));
                        theme.set_popup_duration(1000.);
                        launcher::set_open(&w, true);
                    }
                }
                9 => {
                    assert!(
                        w.get_menu_progress() > 0. && w.get_menu_progress() < 1.,
                        "Popup must animate, not jump"
                    );
                    launcher::set_open(&w, false);
                }
                10 => {
                    launcher::set_open(&w, true);
                }
                23 => {
                    assert_eq!(w.get_menu_progress(), 1.);
                    snapshot("launcher-open");
                }
                24 | 26 => {
                    let x = menu_x + w.get_menu_width() as f32
                        - theme.get_menu_padding()
                        - theme.get_menu_action_size() / 2.;
                    click(x, theme.get_menu_header_height() / 2.);
                }
                25 => {
                    assert!(w.get_launcher_pinned());
                    popup::dismiss_transient(&w);
                    assert!(w.get_is_app_menu_open());
                }
                27 => {
                    assert!(!w.get_launcher_pinned());
                    popup::dismiss_transient(&w);
                    assert!(!w.get_is_app_menu_open());
                }
                40 => {
                    assert!(
                        w.window().size().height < 150,
                        "Closing must release the expanded layer size"
                    );
                    w.set_selected_category("all".into());
                    launcher::set_open(&w, true);
                }
                54 => {
                    let view_height = w.get_menu_height() as f32
                        - theme.get_menu_header_height()
                        - theme.get_menu_footer_height();
                    let thumb_height = (view_height * view_height
                        / (40. * theme.get_app_row_height()))
                    .max(theme.get_scrollbar_min_thumb());
                    let x = menu_x + w.get_menu_width() as f32
                        - theme.get_menu_padding()
                        - theme.get_scrollbar_gutter() / 2.;
                    let y = theme.get_menu_header_height();
                    pointer(x, y + thumb_height / 2., 0);
                    pointer(x, y + thumb_height / 2., 1);
                    pointer(x, y + view_height - thumb_height / 2., 0);
                    pointer(x, y + view_height - thumb_height / 2., 2);
                }
                56 => {
                    snapshot("launcher-scrolled");
                    click(
                        menu_x + theme.get_menu_padding() + theme.get_menu_sidebar_width() + 60.,
                        w.get_menu_height() as f32
                            - theme.get_menu_footer_height()
                            - theme.get_app_row_height() / 2.,
                    );
                }
                57 => {
                    assert_eq!(
                        launched.borrow().as_str(),
                        "item-39",
                        "Dragging the thumb must reach the last row"
                    );
                    panel_editor::set_open(&w, true);
                }
                68 => {
                    let rows = w.get_widgets();
                    let clock = rows.iter().find(|row| row.kind == "clock").unwrap();
                    let launcher = rows.iter().find(|row| row.kind == "launcher").unwrap();
                    let x = w.get_panel_offset() + clock.x + clock.width / 2.;
                    let y = w.window().size().height as f32 - w.get_thickness() as f32 / 2.;
                    pointer(x, y, 0);
                    pointer(x, y, 1);
                    let destination = w.get_panel_offset() + launcher.x + launcher.width / 2. - 4.;
                    pointer(destination, y, 0);
                    pointer(destination, y, 2);
                }
                69 => {
                    assert_eq!(
                        config.get_layout().panels[0]
                            .widgets
                            .iter()
                            .find(|w| w.kind == rix_config::WidgetKind::Clock)
                            .unwrap()
                            .section,
                        rix_config::WidgetSection::Center
                    );
                }
                70 => {
                    snapshot("editor-bottom");
                    let editor_top = w.window().size().height as f32
                        - w.get_thickness() as f32
                        - theme.get_editor_height();
                    click(
                        w.get_panel_offset() + w.get_panel_content_width() / 2.,
                        editor_top + 187.,
                    );
                }
                71 => {
                    snapshot("editor-combobox");
                    for key in [
                        slint::platform::Key::DownArrow,
                        slint::platform::Key::Return,
                    ] {
                        let text: slint::SharedString = key.into();
                        w.window()
                            .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                                text: text.clone(),
                            });
                        w.window()
                            .dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
                    }
                }
                72 => {
                    assert_eq!(
                        config.get_layout().panels[0].alignment,
                        rix_core::Alignment::End,
                        "Editor combo selection must persist"
                    );
                    for (key, value) in [
                        ("length", 700.),
                        ("alignment", 2.),
                        ("floating", 1.),
                        ("margin", 10.),
                        ("height", 64.),
                        ("opacity", 0.9),
                        ("position", 1.),
                    ] {
                        w.invoke_set_panel_option(key.into(), value);
                    }
                }
                75 => {
                    assert_eq!(w.get_panel_content_width(), 700.);
                    assert_eq!(w.get_panel_offset(), w.window().size().width as f32 - 710.);
                    assert!(w.get_panel_at_top());
                    assert_eq!(config.get_layout().panels[0].opacity, 0.9);
                    snapshot("editor-top-fixed");
                    done.set(true);
                    slint::quit_event_loop().unwrap();
                }
                _ => {}
            }
        },
    );
    window.run().unwrap();
    assert!(success.get());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires disposable compositor/profile and RIX_TEST_POLISH_DIR"]
async fn native_system_menus_and_pins() {
    let output = std::env::var("RIX_TEST_POLISH_DIR").unwrap();
    let config = Arc::new(ConfigManager::init().unwrap());
    set_active_icon_theme(&config.get_theme().icon_theme);
    let surface = rix_shell_adapter::install(Default::default()).unwrap();
    SHELL_SURFACE.with(|s| *s.borrow_mut() = Some(surface.clone()));
    let window = RixPanelWindow::new().unwrap();
    install_surface_geometry(&window, &surface);
    window.set_panel_width(config.get_display().width as i32);
    window.set_available_height(config.get_display().height as i32);
    let weak = window.as_weak();
    window.on_reflow_panel(move || {
        if let Some(w) = weak.upgrade() {
            layout::reflow(&w);
        }
    });
    let fullscreen_surface = surface.clone();
    window.on_surface_output_fullscreen_changed(move |fullscreen| {
        fullscreen_surface.set_output_fullscreen(fullscreen)
    });
    let overlay_surface = surface.clone();
    window.on_surface_overlay_changed(move |active| overlay_surface.set_overlay_active(active));
    window
        .on_surface_state_changed(move |interactive| surface.set_keyboard_interactive(interactive));
    let apps = Arc::new(scan_applications());
    apply_shell_theme(&window, &config.get_theme());
    apply_layout_to_ui(
        &window,
        &config.get_layout(),
        &apps,
        config.get_desktop().clipboard.enabled,
    );
    popup::install(&window);
    application_actions::install(&window, config.clone(), apps.clone());
    system_bridge::install(&window, config.clone());
    window.global::<SystemState>().on_network_visible(|_| {});
    let _preview_timer = preview::install(&window);
    let app = apps.iter().next().unwrap().clone();
    let weak = window.as_weak();
    window.on_open_system_popup(move |kind, x| {
        if let Some(w) = weak.upgrade() {
            let close = w.get_system_popup() == kind;
            popup::dismiss_all(&w);
            w.set_system_anchor(x);
            w.set_system_popup(if close { "".into() } else { kind });
        }
    });
    let credential = Rc::new(std::cell::RefCell::new(None::<(String, String, bool)>));
    let capture = credential.clone();
    window
        .global::<SystemState>()
        .on_network_connect(move |path, password, remember| {
            *capture.borrow_mut() = Some((path.into(), password.into(), remember));
        });
    let ticks = Rc::new(std::cell::Cell::new(0));
    let mut keyboard_waits = 0;
    let weak = window.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(150),
        move || {
            let n = ticks.get() + 1;
            ticks.set(n);
            let w = weak.upgrade().unwrap();
            let snapshot = |name: &str| {
                let pixels = w.window().take_snapshot().unwrap();
                image::save_buffer(
                    format!("{output}/{name}.png"),
                    pixels.as_bytes(),
                    pixels.width(),
                    pixels.height(),
                    image::ColorType::Rgba8,
                )
                .unwrap();
                let destination = format!("{output}/{name}-desktop.png");
                tokio::spawn(async move {
                    let mut client = rix_ipc::IpcClient::connect().await.unwrap();
                    client
                        .send(rix_ipc::IpcRequest::Screenshot {
                            path: Some(destination),
                        })
                        .await
                        .unwrap();
                });
            };
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
            let type_text = |value: &str| {
                for ch in value.chars() {
                    let text: SharedString = ch.to_string().into();
                    w.window()
                        .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                            text: text.clone(),
                        });
                    w.window()
                        .dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
                }
            };
            match n {
                4 => {

                    let mut task =
                        build_taskbar_items(&apps, std::slice::from_ref(&app.desktop_id)).remove(0);
                    task.id = 777;
                    task.is_pinned = false;
                    task.is_running = true;
                    set_taskbar(&w, vec![task]);
                    w.invoke_app_context_menu(app.exec.as_str().into(), 800., 20.);
                    assert!(w.get_context_menu_items().iter().any(|e| e.id == "pin_app"));
                    w.invoke_context_menu_selected("pin_app".into(), 777, app.exec.as_str().into());
                    assert_eq!(read_taskbar(&w).len(), 1);
                    assert!(read_taskbar(&w)[0].is_pinned);
                    assert_eq!(read_taskbar(&w)[0].id, 777);
                    assert_eq!(
                        load_pinned_execs(&config),
                        std::slice::from_ref(&app.desktop_id)
                    );
                    w.invoke_context_menu_selected(
                        "unfavorite".into(),
                        777,
                        app.exec.as_str().into(),
                    );
                    assert!(!config
                        .get_layout()
                        .launcher
                        .favorites
                        .contains(&app.desktop_id));
                    w.invoke_context_menu_selected(
                        "favorite".into(),
                        777,
                        app.exec.as_str().into(),
                    );
                    assert!(config
                        .get_layout()
                        .launcher
                        .favorites
                        .contains(&app.desktop_id));

                    let restored = rix_config::parse_layout_kdl(
                        &std::fs::read_to_string(config.config_dir().join("layout.kdl")).unwrap(),
                    )
                    .unwrap();
                    assert!(restored.launcher.favorites.contains(&app.desktop_id));
                    w.set_system_anchor(1050.);
                    w.set_system_popup("audio".into());
                }
                18 => {
                    assert!(
                        w.global::<SystemState>().get_audio().row_count() > 0,
                        "Live audio snapshot absent: {}",
                        w.global::<SystemState>().get_audio_error()
                    );
                    snapshot("audio");
                }
                19 => {
                    w.set_system_popup("network".into());
                }
                30 => {
                    assert!(
                        w.global::<SystemState>().get_networks().row_count() > 0,
                        "Live network snapshot absent: {}",
                        w.global::<SystemState>().get_network_error()
                    );
                    snapshot("network");
                }
                31 => {
                    w.set_system_popup("calendar".into());
                }
                35 => {
                    snapshot("calendar");
                }
                36 => {
                    w.global::<SystemState>().invoke_calendar_month(1);
                }
                39 => {
                    assert_eq!(w.global::<SystemState>().get_days().row_count(), 42);
                    w.set_system_popup("keyboard".into());
                }
                43 => {
                    assert_eq!(w.global::<SystemState>().get_layouts().row_count(), 3);
                    snapshot("keyboard");
                }
                44 => {
                    w.global::<SystemState>().invoke_keyboard_select(1);
                }
                48 => {
                    if w.global::<SystemState>().get_layout_label() != "UA" && keyboard_waits < 33 {
                        keyboard_waits += 1;
                        ticks.set(47);
                        return;
                    }
                    assert_eq!(w.global::<SystemState>().get_layout_label(), "UA");
                    w.global::<SystemState>().invoke_keyboard_select(0);
                    w.set_system_popup("".into());
                }
                52 => {
                    let clock = w
                        .get_widgets()
                        .iter()
                        .find(|widget| widget.kind == "clock")
                        .unwrap();
                    click(
                        w.get_panel_offset() + clock.x + clock.width / 2.,
                        w.window().size().height as f32 - w.get_thickness() as f32 / 2.,
                    );
                    assert_eq!(
                        w.get_system_popup(),
                        "calendar",
                        "Clock click must open calendar"
                    );
                }
                56 => {
                    click(10., 10.);
                    assert_eq!(w.get_system_popup(), "");
                }
                59 => {
                    w.set_favorite_apps(ModelRc::from(Rc::new(VecModel::from(
                        convert_apps_to_menu_data(std::slice::from_ref(&app), 48),
                    ))));
                    launcher::set_open(&w, true);
                }
                63 => {
                    let theme = w.global::<Theme>();
                    let menu_x = (w.get_launcher_x()
                        + w.get_launcher_width() * w.get_menu_alignment()
                        - w.get_menu_width() as f32 * w.get_menu_alignment())
                    .clamp(
                        0.,
                        w.window().size().width as f32 - w.get_menu_width() as f32,
                    );
                    let position = slint::LogicalPosition::new(
                        menu_x + theme.get_menu_sidebar_width() + 70.,
                        110.,
                    );
                    w.window()
                        .dispatch_event(slint::platform::WindowEvent::PointerMoved { position });
                    w.window()
                        .dispatch_event(slint::platform::WindowEvent::PointerPressed {
                            position,
                            button: slint::platform::PointerEventButton::Right,
                        });
                    w.window()
                        .dispatch_event(slint::platform::WindowEvent::PointerReleased {
                            position,
                            button: slint::platform::PointerEventButton::Right,
                        });
                }
                67 => {
                    assert!(
                        w.get_is_context_menu_open(),
                        "Real right-click must open application actions"
                    );
                    assert!(w
                        .get_context_menu_items()
                        .iter()
                        .any(|entry| entry.id == "unpin_app"));
                    snapshot("application-actions");
                }
                68 => {
                    w.invoke_navigate_context_menu(1);
                    w.invoke_navigate_context_menu(1);
                    w.invoke_activate_context_selection();
                    assert!(!read_taskbar(&w)[0].is_pinned);
                    assert!(load_pinned_execs(&config).is_empty());
                }
                71 => {
                    launcher::set_open(&w, false);
                    w.global::<SystemState>()
                        .set_networks(ModelRc::from(Rc::new(VecModel::from(vec![NetworkRow {
                            path: "/test/ap".into(),
                            name: "Test Secure Network".into(),
                            detail: "WPA2 · 92%".into(),
                            password: true,
                            supported: true,
                            ..Default::default()
                        }]))));
                    w.set_system_popup("network".into());
                }
                76 => {
                    click(1180., 165.);
                }
                80 => {
                    click(1000., 168.);
                    type_text("rixde-test-password");
                }
                82 => {
                    snapshot("network-password");
                }
                83 => {
                    click(885., 234.);
                    click(1140., 272.);
                }
                85 => {
                    assert_eq!(
                        *credential.borrow(),
                        Some(("/test/ap".into(), "rixde-test-password".into(), false)),
                        "Password and persistence choice must reach the controller"
                    );
                    click(1180., 165.);
                }
                89 => {
                    click(1000., 168.);
                    type_text("second-password");
                    let text: SharedString = slint::platform::Key::Return.into();
                    w.window()
                        .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                            text: text.clone(),
                        });
                    w.window()
                        .dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
                }
                93 => {
                    assert_eq!(
                        credential.borrow().as_ref().unwrap().1,
                        "second-password",
                        "A new prompt must not retain the previous secret"
                    );
                    slint::quit_event_loop().unwrap();
                }
                _ => {}
            }
        },
    );
    window.run().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a disposable compositor with a Settings window and RIX_TEST_POLISH_DIR"]
async fn native_live_window_preview() {
    let output = std::env::var("RIX_TEST_POLISH_DIR").unwrap();
    let mut client = rix_ipc::IpcClient::connect().await.unwrap();
    let rix_ipc::IpcResponse::Windows(windows) =
        client.send(rix_ipc::IpcRequest::ListWindows).await.unwrap()
    else {
        panic!("windows response")
    };
    let target = windows
        .iter()
        .find(|w| w.app_id == "org.rixde.Settings")
        .expect("Open Settings in the test compositor")
        .clone();
    let config = ConfigManager::init().unwrap();
    let surface = rix_shell_adapter::install(Default::default()).unwrap();
    let window = RixPanelWindow::new().unwrap();
    install_surface_geometry(&window, &surface);
    let fullscreen_surface = surface.clone();
    window.on_surface_output_fullscreen_changed(move |fullscreen| {
        fullscreen_surface.set_output_fullscreen(fullscreen)
    });
    let overlay_surface = surface.clone();
    window.on_surface_overlay_changed(move |active| overlay_surface.set_overlay_active(active));
    window
        .on_surface_state_changed(move |interactive| surface.set_keyboard_interactive(interactive));
    window.set_panel_width(config.get_display().width as i32);
    window.set_available_height(config.get_display().height as i32);
    apply_shell_theme(&window, &config.get_theme());
    apply_layout_to_ui(
        &window,
        &config.get_layout(),
        &[],
        config.get_desktop().clipboard.enabled,
    );
    set_taskbar(
        &window,
        vec![TaskbarItemData {
            id: target.id as i32,
            title: target.title.clone().into(),
            app_id: target.app_id.into(),
            is_running: true,
            is_active: true,
            windows: ModelRc::from(Rc::new(VecModel::from(vec![TaskWindowData {
                id: target.id as i32,
                title: target.title.clone().into(),
                active: true,
            }]))),
            ..Default::default()
        }],
    );
    let _preview = preview::install(&window);
    let weak = window.as_weak();
    let mut tick = 0;
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            tick += 1;
            let w = weak.upgrade().unwrap();
            if tick == 3 {
                w.invoke_task_hover(target.id as i32, true, 600.);
            }
            if tick == 25 {
                assert!(w.get_preview_visible());
                let row = w.get_preview_items().row_data(0).unwrap();
                assert!(!row.failed);
                let size = row.image.size();
                assert!(
                    size.width > 100 && size.height > 50,
                    "Missing actual window image"
                );
                let pixels = w.window().take_snapshot().unwrap();
                image::save_buffer(
                    format!("{output}/window-preview.png"),
                    pixels.as_bytes(),
                    pixels.width(),
                    pixels.height(),
                    image::ColorType::Rgba8,
                )
                .unwrap();
                let destination = format!("{output}/window-preview-desktop.png");
                tokio::spawn(async move {
                    let mut client = rix_ipc::IpcClient::connect().await.unwrap();
                    client
                        .send(rix_ipc::IpcRequest::Screenshot {
                            path: Some(destination),
                        })
                        .await
                        .unwrap();
                });
                w.invoke_task_hover(target.id as i32, false, 600.);
                let preview_region = w
                    .get_surface_regions()
                    .row_data(4)
                    .expect("Preview input region must be present");
                w.window()
                    .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                        position: slint::LogicalPosition::new(
                            preview_region.x + preview_region.width / 2.,
                            preview_region.y + preview_region.height / 2.,
                        ),
                    });
            }
            if tick == 32 {
                assert!(
                    w.get_preview_visible(),
                    "Preview remains visible while the pointer is over its content"
                );
                w.window()
                    .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                        position: slint::LogicalPosition::new(1., 1.),
                    });
            }
            if tick == 40 {
                assert!(
                    !w.get_preview_visible(),
                    "Preview must close after pointer leaves"
                );
                w.invoke_task_hover(target.id as i32, true, 600.);
            }
            if tick == 48 {
                assert!(w.get_preview_visible());
                w.invoke_preview_hover(true);
                w.set_system_popup("calendar".into());
            }
            if tick == 52 {
                assert!(!w.get_preview_visible());
                w.set_system_popup("".into());
            }
            if tick == 56 {
                assert!(
                    !w.get_preview_visible(),
                    "Dismissed previews must not reopen from stale hover state"
                );
                slint::quit_event_loop().unwrap();
            }
        },
    );
    window.run().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires isolated compositor/profile and RIX_TEST_POLISH_DIR"]
async fn native_desktop_menu_and_switcher() {
    use slint::Model;
    let output = std::env::var("RIX_TEST_POLISH_DIR").unwrap();
    let config = Arc::new(ConfigManager::init().unwrap());
    let window = create_test_window(&config);
    popup::install(&window);
    let _switcher = crate::switcher::install(&window);
    let apps = crate::apps::scan_applications();
    let weak = window.as_weak();
    let mut tick = 0;
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(150),
        move || {
            tick += 1;
            let w = weak.upgrade().unwrap();
            if tick == 2 {
                crate::popup::open_desktop(&w, 900., 300.);
            }
            if tick == 6 {
                assert!(w.get_desktop_context());
                assert_eq!(w.get_context_menu_items().row_count(), 3);
                save_window(&w, &format!("{output}/desktop-menu.png"));
                crate::popup::close(&w);
                let windows = (1..=4)
                    .map(|id| rix_ipc::WindowInfo {
                        outputs: Vec::new(),
                        foreground_outputs: None,
                        id,
                        title: format!("Example window {id}"),
                        app_id: "org.rixde.Settings".into(),
                        is_active: id == 1,
                        is_minimized: id == 4,
                        is_maximized: false,
                        is_fullscreen: false,
                    })
                    .collect();
                crate::switcher::snapshot(
                    &w,
                    rix_ipc::SwitcherState {
                        target: None,
                        generation: 1,
                        selected: 1,
                        windows,
                    },
                    &config,
                    &apps,
                );
            }
            if tick == 12 {
                assert!(w.get_switcher_visible());
                save_window(&w, &format!("{output}/switcher.png"));
            }
            if tick == 14 {
                crate::switcher::snapshot(
                    &w,
                    rix_ipc::SwitcherState {
                        target: None,
                        generation: 2,
                        selected: 0,
                        windows: vec![],
                    },
                    &config,
                    &apps,
                );
                assert!(!w.get_switcher_visible());
                slint::quit_event_loop().unwrap();
            }
        },
    );
    window.run().unwrap();
}

fn create_test_window(config: &ConfigManager) -> RixPanelWindow {
    set_active_icon_theme(&config.get_theme().icon_theme);
    let surface = rix_shell_adapter::install(Default::default()).unwrap();
    SHELL_SURFACE.with(|s| *s.borrow_mut() = Some(surface.clone()));
    let window = RixPanelWindow::new().unwrap();
    install_surface_geometry(&window, &surface);
    window.set_panel_width(config.get_display().width as i32);
    window.set_available_height(config.get_display().height as i32);
    let weak = window.as_weak();
    window.on_reflow_panel(move || {
        if let Some(w) = weak.upgrade() {
            layout::reflow(&w);
        }
    });
    let fullscreen_surface = surface.clone();
    window.on_surface_output_fullscreen_changed(move |fullscreen| {
        fullscreen_surface.set_output_fullscreen(fullscreen)
    });
    let overlay_surface = surface.clone();
    window.on_surface_overlay_changed(move |active| overlay_surface.set_overlay_active(active));
    window
        .on_surface_state_changed(move |interactive| surface.set_keyboard_interactive(interactive));
    apply_shell_theme(&window, &config.get_theme());
    apply_layout_to_ui(
        &window,
        &config.get_layout(),
        &[],
        config.get_desktop().clipboard.enabled,
    );
    window
}
fn save_window(window: &RixPanelWindow, path: &str) {
    let pixels = window.window().take_snapshot().unwrap();
    image::save_buffer(
        path,
        pixels.as_bytes(),
        pixels.width(),
        pixels.height(),
        image::ColorType::Rgba8,
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires disposable compositor/profile and RIX_TEST_POLISH_DIR"]
async fn native_group_audio_clipboard_and_transitions() {
    use slint::Model;
    let output = std::env::var("RIX_TEST_POLISH_DIR").unwrap();
    let config = Arc::new(ConfigManager::init().unwrap());
    let window = create_test_window(&config);
    let _preview = preview::install(&window);
    let app = AppInfo {
        desktop_file: "/test/org.example.Player.desktop".into(),
        name: "Player".into(),
        desktop_id: "org.example.Player".into(),
        exec: "fixture-player".into(),
        icon_name: "multimedia-player".into(),
        icon_path: None,
        categories: vec![],
        startup_wm_class: "fixture-player".into(),
    };
    let apps = Arc::new(vec![app]);
    popup::install(&window);
    launcher::install(&window, config.clone());
    application_actions::install(&window, config.clone(), apps.clone());
    task_audio::install(&window, apps.clone());
    let weak = window.as_weak();
    window.on_close_context_menu(move || {
        if let Some(w) = weak.upgrade() {
            popup::close(&w);
        }
    });
    let commands = Rc::new(std::cell::RefCell::new(Vec::new()));
    let received = commands.clone();
    window
        .global::<SystemState>()
        .on_audio_mute(move |key, muted| {
            received
                .borrow_mut()
                .push((key.to_string(), if muted { -1 } else { -2 }))
        });
    let received = commands.clone();
    window
        .global::<SystemState>()
        .on_audio_volume(move |key, volume| received.borrow_mut().push((key.to_string(), volume)));
    let weak = window.as_weak();
    let pending = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut tick = 0;
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            if pending.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            tick += 1;
            let w = weak.upgrade().unwrap();
            let shot = |name: &str| {
                let path = format!("{output}/{name}.png");
                pending.store(true, std::sync::atomic::Ordering::Relaxed);
                let pending = pending.clone();
                tokio::spawn(async move {
                    let mut client = rix_ipc::IpcClient::connect().await.unwrap();
                    client
                        .send(rix_ipc::IpcRequest::Screenshot { path: Some(path) })
                        .await
                        .unwrap();
                    pending.store(false, std::sync::atomic::Ordering::Relaxed);
                });
            };
            match tick {
                2 => {
                    set_taskbar(
                        &w,
                        build_taskbar_items(&apps, &["org.example.Player".into()]),
                    );
                    let windows: Vec<_> = (1..=2)
                        .map(|id| rix_ipc::WindowInfo {
                            outputs: Vec::new(),
                            foreground_outputs: None,
                            id,
                            title: format!("Player {id}"),
                            app_id: "fixture-player".into(),
                            is_active: id == 2,
                            is_minimized: false,
                            is_maximized: false,
                            is_fullscreen: false,
                        })
                        .collect();
                    tasks::snapshot(&w, windows.clone(), &apps, true);
                    assert_eq!(w.get_taskbar_items().row_count(), 1);
                    let group = w.get_taskbar_items().row_data(0).unwrap();
                    assert_eq!(group.windows.row_count(), 2);
                    assert!(group.is_pinned);
                    assert_eq!(group.id, 2);
                    w.global::<SystemState>()
                        .set_audio(ModelRc::from(Rc::new(VecModel::from(vec![
                            AudioRow {
                                key: "sink-input:1".into(),
                                app_id: "org.example.Player".into(),
                                kind: "sink-input".into(),
                                volume: 60,
                                ..Default::default()
                            },
                            AudioRow {
                                key: "sink-input:2".into(),
                                binary: "fixture-player".into(),
                                kind: "sink-input".into(),
                                volume: 40,
                                ..Default::default()
                            },
                            AudioRow {
                                key: "sink-input:3".into(),
                                binary: "unrelated".into(),
                                kind: "sink-input".into(),
                                volume: 90,
                                ..Default::default()
                            },
                        ]))));
                    w.global::<SystemState>().invoke_audio_changed();
                    let group = w.get_taskbar_items().row_data(0).unwrap();
                    assert!(group.has_audio);
                    assert_eq!(group.audio_volume, 60);
                    w.invoke_task_audio_toggle(2);
                    w.invoke_task_audio_volume(2, 25);
                    assert_eq!(
                        *commands.borrow(),
                        vec![
                            ("sink-input:1".into(), -1),
                            ("sink-input:2".into(), -1),
                            ("sink-input:1".into(), 25),
                            ("sink-input:2".into(), 25)
                        ]
                    );
                    w.invoke_taskbar_item_clicked(2);
                    assert!(w.get_preview_pinned());
                    tasks::snapshot(&w, vec![windows[0].clone()], &apps, true);
                    tasks::snapshot(&w, windows, &apps, true);
                }
                7 => {
                    shot("grouped-tasks");
                }
                9 => {
                    w.set_preview_visible(false);
                }
                13 => {
                    assert_eq!(w.get_system_progress(), 0.);
                    tasks::regroup(&w, &apps, false);
                    assert_eq!(w.get_taskbar_items().row_count(), 2);
                    popup::open_desktop(&w, 500., 350.);
                }
                18 => {
                    shot("context-menu");
                }
                20 => {
                    popup::close(&w);
                }
                24 => {
                    launcher::set_open(&w, true);
                }
                28 => {
                    w.invoke_power_action("leave".into());
                    assert!(w.get_is_context_menu_open());
                    assert!(w
                        .get_context_menu_items()
                        .iter()
                        .all(|i| i.id.starts_with("session:")));
                }
                30 => shot("session-menu"),
                31 => {
                    popup::dismiss_all(&w);
                    w.set_system_anchor(1200.);
                    w.global::<ClipboardState>()
                        .set_items(ModelRc::from(Rc::new(VecModel::from(vec![
                            ClipboardItem {
                                id: "1".into(),
                                label: "Test clipboard entry".into(),
                                pinned: true,
                                image: false,
                            },
                        ]))));
                    w.set_system_popup("clipboard".into());
                }
                36 => {
                    shot("clipboard-menu");
                }
                42 => {
                    tasks::snapshot(&w, vec![], &apps, true);
                    let pin = w.get_taskbar_items().row_data(0).unwrap();
                    assert!(pin.is_pinned && !pin.is_running && pin.id < 0);
                    assert_eq!(pin.title, "Player");
                    slint::quit_event_loop().unwrap();
                }
                _ => {}
            }
        },
    );
    window.run().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires disposable compositor/profile; prints measured frame cadence"]
async fn native_animation_frame_cadence() {
    let config = ConfigManager::init().unwrap();
    let window = create_test_window(&config);
    window.global::<Theme>().set_popup_duration(2000.0);
    let weak = window.as_weak();
    let timer = slint::Timer::default();
    let mut ticks = 0;
    let mut started = None;
    let mut before = (0, Duration::ZERO);
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            ticks += 1;
            let w = weak.upgrade().unwrap();
            if ticks == 5 {
                before = SHELL_SURFACE.with(|s| s.borrow().as_ref().unwrap().frame_stats());
                started = Some(std::time::Instant::now());
                launcher::set_open(&w, true);
            }
            if started.is_some_and(|t| t.elapsed() >= Duration::from_millis(1900)) {
                let after = SHELL_SURFACE.with(|s| s.borrow().as_ref().unwrap().frame_stats());
                let count = after.0 - before.0;
                let elapsed = started.unwrap().elapsed().as_secs_f64();
                eprintln!(
                    "Animation cadence: {:.1} frames/s; {} frames; render submission mean {:.2} ms",
                    count as f64 / elapsed,
                    count,
                    (after.1 - before.1).as_secs_f64() * 1000. / count.max(1) as f64
                );
                assert!(
                    count > 1,
                    "Animation must receive successive compositor frames"
                );
                slint::quit_event_loop().unwrap();
            }
        },
    );
    window.run().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a disposable compositor with two Settings windows and RIX_TEST_POLISH_DIR"]
async fn native_gpu_group_previews_and_stable_panel() {
    let output = std::env::var("RIX_TEST_POLISH_DIR").unwrap();
    let mut client = rix_ipc::IpcClient::connect().await.unwrap();
    let rix_ipc::IpcResponse::Windows(windows) =
        client.send(rix_ipc::IpcRequest::ListWindows).await.unwrap()
    else {
        panic!("No windows")
    };
    let members: Vec<_> = windows
        .into_iter()
        .filter(|w| w.app_id == "org.rixde.Settings")
        .collect();
    assert!(
        members.len() >= 2,
        "Launch two Settings windows in the disposable compositor"
    );
    let config = ConfigManager::init().unwrap();
    let window = create_test_window(&config);
    let _preview = preview::install(&window);
    tasks::snapshot(&window, members.clone(), &[], true);
    let task = window
        .get_taskbar_items()
        .iter()
        .find(|t| t.windows.row_count() >= 2)
        .unwrap();
    let id = task.id;
    let weak = window.as_weak();
    let timer = slint::Timer::default();
    let mut ticks = 0;
    let mut stable_size = slint::PhysicalSize::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            ticks += 1;
            let w = weak.upgrade().unwrap();
            if ticks == 5 {
                stable_size = w.window().size();
                w.invoke_task_hover(id, true, 500.);
            }
            if ticks == 25 {
                assert!(w.get_preview_visible());
                assert_eq!(w.get_preview_items().row_count(), members.len());
                assert!(w
                    .get_preview_items()
                    .iter()
                    .all(|row| row.image.size().width > 0 && !row.failed));
                assert_eq!(
                    w.window().size(),
                    stable_size,
                    "Opening a preview must not resize the layer"
                );
                save_compositor(&format!("{output}/gpu-group-previews.png"));
                let mut changed = members.clone();
                for (index, member) in changed.iter_mut().enumerate() {
                    member.is_active = index == 0;
                }
                tasks::snapshot(&w, changed, &[], true);
            }
            if ticks == 30 {
                assert!(
                    w.get_preview_visible(),
                    "Changing the representative window must preserve the group"
                );
                tasks::snapshot(&w, vec![members[0].clone()], &[], true);
            }
            if ticks == 35 {
                assert_eq!(w.get_preview_items().row_count(), 1);
                w.invoke_task_hover(w.get_preview_id(), false, 500.);
                w.invoke_preview_hover(false);
            }
            if ticks == 45 {
                assert!(!w.get_preview_visible());
                launcher::set_open(&w, true);
            }
            if ticks == 50 {
                assert_eq!(w.window().size(), stable_size);
                save_compositor(&format!("{output}/gpu-launcher.png"));
                launcher::set_open(&w, false);
            }
            if ticks == 55 {
                assert_eq!(w.window().size(), stable_size);
                slint::quit_event_loop().unwrap();
            }
        },
    );
    window.run().unwrap();
}

fn save_compositor(path: &str) {
    let path = path.to_owned();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let mut client = rix_ipc::IpcClient::connect().await.unwrap();
                let response = client
                    .send(rix_ipc::IpcRequest::Screenshot { path: Some(path) })
                    .await
                    .unwrap();
                assert!(
                    !matches!(response, rix_ipc::IpcResponse::Error(_)),
                    "{response:?}"
                );
            });
    })
    .join()
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a disposable compositor/profile and RIX_TEST_POLISH_DIR"]
async fn native_launcher_selection_and_application_context() {
    let output = std::env::var("RIX_TEST_POLISH_DIR").unwrap();
    let config = Arc::new(ConfigManager::init().unwrap());
    let apps = Arc::new(scan_applications());
    let app = apps
        .iter()
        .find(|a| {
            a.categories.iter().any(|c| c == "Network")
                && !rix_services::applications::desktop_actions(&a.desktop_file).is_empty()
        })
        .expect("Installed desktop actions")
        .clone();
    let window = create_test_window(&config);
    popup::install(&window);
    application_actions::install(&window, config.clone(), apps.clone());
    launcher::install(&window, config.clone());
    window.set_categories(ModelRc::from(Rc::new(VecModel::from(menu_categories(
        &window, &apps,
    )))));
    window.set_favorite_apps(ModelRc::from(Rc::new(VecModel::from(
        convert_apps_to_menu_data(&apps[..apps.len().min(24)], 48),
    ))));
    let mut windows = Vec::new();
    for id in [101, 102] {
        windows.push(rix_ipc::WindowInfo {
            outputs: Vec::new(),
            foreground_outputs: None,
            id,
            app_id: app.desktop_id.clone(),
            title: format!("Window {id}"),
            is_active: id == 102,
            is_minimized: false,
            is_maximized: false,
            is_fullscreen: false,
        });
    }
    tasks::snapshot(&window, windows.clone(), &apps, true);
    let model = window.get_taskbar_items();
    let weak = window.as_weak();
    let timer = slint::Timer::default();
    let mut tick = 0;
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            tick += 1;
            let w = weak.upgrade().unwrap();
            match tick {
                5 => launcher::set_open(&w, true),
                10 => {
                    save_compositor(&format!("{output}/launcher-before-hover.png"));
                    let theme = w.global::<Theme>();
                    let skin = theme.get_menu_frame();
                    if skin.center_is_solid {
                        let pixels = w.window().take_snapshot().unwrap();
                        let rect = w.get_surface_regions().row_data(1).unwrap();
                        let scale = w.window().scale_factor();
                        for local_y in [
                            skin.top_height / 2.,
                            theme.get_menu_header_height() - theme.get_default_spacing(),
                            rect.height - skin.bottom_height / 2.,
                        ] {
                            let y = ((rect.y + local_y) * scale) as usize;
                            for x in [
                                rect.x + skin.left_width,
                                rect.x + rect.width - skin.right_width,
                            ] {
                                let x = (x * scale) as usize;
                                let alphas: Vec<_> = (x - 2..=x + 2)
                                    .map(|x| {
                                        pixels.as_bytes()[(y * pixels.width() as usize + x) * 4 + 3]
                                    })
                                    .collect();
                                assert!(
                                    alphas.iter().max().unwrap() - alphas.iter().min().unwrap()
                                        <= 2,
                                    "Visible frame seam: {alphas:?}"
                                );
                            }
                        }
                    }
                }
                11 => {
                    let rect = w.get_surface_regions().row_data(1).unwrap();
                    let theme = w.global::<Theme>();
                    w.window()
                        .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                            position: slint::LogicalPosition::new(
                                rect.x + theme.get_menu_padding() + 100.,
                                rect.y
                                    + theme.get_menu_header_height()
                                    + theme.get_menu_row_height() * 1.5,
                            ),
                        });
                }
                14 => save_compositor(&format!("{output}/launcher-after-hover.png")),
                16 => {
                    w.invoke_app_context_menu(app.desktop_id.as_str().into(), 600., 400.);
                    assert!(w.get_is_app_menu_open());
                    assert!(w
                        .get_context_menu_items()
                        .iter()
                        .any(|row| row.id.starts_with("desktop-action:")));
                }
                20 => save_compositor(&format!("{output}/application-context.png")),
                22 => {
                    popup::close(&w);
                    w.invoke_taskbar_context_menu(102, 600., w.window().size().height as f32 - 25.);
                    assert!(w
                        .get_context_menu_items()
                        .iter()
                        .any(|row| row.id == "close_all"));
                    assert!(w
                        .get_context_menu_items()
                        .iter()
                        .all(|row| row.id != "minimize" && row.id != "maximize"));
                    assert_eq!(
                        w.get_context_target_windows().iter().collect::<Vec<_>>(),
                        vec![101, 102]
                    );
                    let mut changed = windows.clone();
                    changed[0].is_active = true;
                    changed[1].is_active = false;
                    tasks::snapshot(&w, changed, &apps, true);
                    assert!(
                        std::ptr::eq(model.as_any(), w.get_taskbar_items().as_any()),
                        "Focus changes must retain row delegates"
                    );
                    assert_eq!(
                        w.get_context_target_windows().iter().collect::<Vec<_>>(),
                        vec![101, 102]
                    );
                }
                26 => save_compositor(&format!("{output}/group-context.png")),
                28 => {
                    popup::dismiss_all(&w);
                    popup::open_desktop(&w, 600., 400.);
                }
                32 => save_compositor(&format!("{output}/desktop-context.png")),
                34 => {
                    popup::dismiss_all(&w);
                    launcher::set_open(&w, true);
                }
                39 => {
                    let rect = w.get_surface_regions().row_data(1).unwrap();
                    let theme = w.global::<Theme>();
                    let position = slint::LogicalPosition::new(
                        rect.x + rect.width - theme.get_menu_padding() - theme.get_icon_size() / 2.,
                        rect.y + rect.height - theme.get_menu_footer_height() / 2.,
                    );

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
                    assert!(
                        w.get_is_context_menu_open(),
                        "Arrow must directly open additional session actions"
                    );
                    assert!(w
                        .get_context_menu_items()
                        .iter()
                        .any(|i| i.id == "session:logout"));
                    assert!(
                        w.get_context_menu_items()
                            .iter()
                            .all(|i| i.id != "session:poweroff"),
                        "Visible footer actions must not be duplicated"
                    );
                }
                42 => save_compositor(&format!("{output}/launcher-session-arrow.png")),
                44 => slint::quit_event_loop().unwrap(),
                _ => {}
            }
        },
    );
    window.run().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires disposable compositor/profile and RIX_TEST_POLISH_DIR; samples GPU motion"]
async fn native_theme_corners_stay_coherent_during_slide() {
    use slint::Model;
    let output = std::env::var("RIX_TEST_POLISH_DIR").unwrap();
    let config = ConfigManager::init().unwrap();
    let window = create_test_window(&config);
    window.global::<Theme>().set_popup_duration(1000.);
    let phase = Rc::new(std::cell::Cell::new(0usize));
    let samples = Rc::new(std::cell::RefCell::new([0usize; 4]));
    let weak = window.as_weak();
    let rendered_phase = phase.clone();
    let rendered_samples = samples.clone();
    let full_height = std::cell::Cell::new(0.);

    window
        .window()
        .set_rendering_notifier(move |state, _| {
            if !matches!(state, slint::RenderingState::AfterRendering) {
                return;
            }
            let Some(w) = weak.upgrade() else { return };
            let progress = w.get_menu_progress();
            if !(0.15..0.98).contains(&progress) {
                return;
            }
            let rect = w.get_surface_regions().row_data(1).unwrap();
            let phase = rendered_phase.get();
            if full_height.get() == 0. {
                full_height.set(rect.height);
            }
            assert_eq!(
                rect.height,
                full_height.get(),
                "Sliding must translate the full frame, not resize its round corners"
            );
            let clip_bottom = rect.clip_y + rect.clip_height;
            let expected_y = if w.get_panel_at_top() {
                rect.clip_y + w.global::<Theme>().get_menu_gap() - rect.height * (1. - progress)
            } else {
                clip_bottom - w.global::<Theme>().get_menu_gap() - rect.height * progress
            };
            assert!((rect.y - expected_y).abs() < 0.01);
            if !(0.1..0.9).contains(&rect.y.fract().abs()) {
                return;
            }
            let theme = w.global::<Theme>();
            let skin = theme.get_menu_frame();
            let scale = w.window().scale_factor();
            let local_y = if w.get_panel_at_top() {
                rect.height - 3.
            } else {
                3.
            };
            let logical_y = rect.y + local_y;
            if logical_y < rect.clip_y + 1. || logical_y >= clip_bottom - 1. {
                return;
            }
            let pixels = w.window().take_snapshot().unwrap();
            let y = (logical_y * scale) as usize;
            if skin.center_is_solid {
                for x in [
                    rect.x + skin.left_width,
                    rect.x + rect.width - skin.right_width,
                ] {
                    let x = (x * scale) as usize;
                    let alpha: Vec<_> = (x - 2..=x + 2)
                        .map(|x| pixels.as_bytes()[(y * pixels.width() as usize + x) * 4 + 3])
                        .collect();
                    assert!(
                        alpha.iter().max().unwrap() - alpha.iter().min().unwrap() <= 2,
                        "Moving frame seam, phase {phase}, y={} alpha={alpha:?}",
                        rect.y
                    );
                }
            }
            let mut samples = rendered_samples.borrow_mut();
            if samples[phase] < 2 {
                image::save_buffer(
                    format!("{output}/corners-motion-{phase}-{}.png", samples[phase]),
                    pixels.as_bytes(),
                    pixels.width(),
                    pixels.height(),
                    image::ColorType::Rgba8,
                )
                .unwrap();
            }
            samples[phase] += 1;
        })
        .unwrap();
    let weak = window.as_weak();
    let timer = slint::Timer::default();
    let mut tick = 0;
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(50),
        move || {
            tick += 1;
            let w = weak.upgrade().unwrap();
            match tick {
                5 => {
                    w.set_panel_at_top(false);
                    launcher::set_open(&w, true);
                }
                30 => {
                    phase.set(1);
                    launcher::set_open(&w, false);
                }
                55 => {
                    phase.set(2);
                    w.set_panel_at_top(true);
                    launcher::set_open(&w, true);
                }
                80 => {
                    phase.set(3);
                    launcher::set_open(&w, false);
                }
                105 => {
                    let samples = samples.borrow();
                    assert!(
                        samples.iter().all(|n| *n >= 2),
                        "Need fractional positions in every direction: {samples:?}"
                    );
                    eprintln!("Checked fractional GPU frames by direction: {samples:?}");
                    slint::quit_event_loop().unwrap();
                }
                _ => {}
            }
        },
    );
    window.run().unwrap();
}
