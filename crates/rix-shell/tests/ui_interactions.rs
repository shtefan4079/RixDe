use slint::{
    platform::{PointerEventButton, WindowEvent},
    ComponentHandle, LogicalPosition,
};

slint::slint! {
    import { Slider } from "../../../ui/common/controls.slint";
    import { ScrollArea } from "../../../ui/common/scroll_area.slint";
    import { PopupGeometry } from "../../../ui/common/popup_geometry.slint";
    import { RixPanelWindow, ContextMenuItemData } from "../ui/main.slint";
    import { ContextMenu } from "../ui/widgets/context_menu.slint";
    import { TaskbarWidget, TaskbarItemData } from "../ui/widgets/taskbar.slint";
    import { ShowDesktopButton } from "../ui/panel.slint";
    import { SkinRenderer, SkinFrame } from "../../../ui/common/frame.slint";
    import { Theme } from "../../../ui/common/theme.slint";
    export { RixPanelWindow, ContextMenuItemData, TaskbarItemData, SkinRenderer, SkinFrame, Theme }
    export component ControlsProbe inherits Window {
        width: 400px; height: 240px;
        in-out property <float> value <=> slider.value;
        out property <length> scroll-position <=> area.viewport-y;
        public pure function menu-coordinate(anchor: length, size: length, available: length, before: bool) -> length {
            return PopupGeometry.coordinate(anchor, size, available, 4px, before);
        }
        public pure function menu-extent(requested: length, available: length) -> length {
            return PopupGeometry.extent(requested, available, 4px);
        }
        area := ScrollArea {
            width: 400px; height: 240px; content-height: 600px;
            slider := Slider { x: 24px; y: 60px; width: 320px; height: 40px; maximum: 100; }
            Rectangle { y: 380px; width: 300px; height: 100px; background: red; }
        }
    }
    export component MenuProbe inherits Window {
        width: 400px; height: 240px;
        out property <length> menu-x: menu.x;
        out property <length> menu-y: menu.y;
        out property <length> menu-width: menu.width;
        out property <length> menu-height: menu.height;
        menu := ContextMenu {
            menu-x: 398px; menu-bottom: 5px;
            window-width: root.width; window-height: root.height;
            content-height: 180px; prefer-below: true;
        }
    }
    export component MenuAppearanceProbe inherits Window {
        width: 400px; height: 240px; background: black;
        in-out property <bool> embedded-popup;
        menu := ContextMenu {
            opaque: root.embedded-popup;
            menu-x: 40px; menu-bottom: 200px;
            window-width: root.width; window-height: root.height;
            content-height: 100px; prefer-below: true;
        }
    }
    export component MenuInteractionsProbe inherits Window {
        width: 400px; height: 240px;
        in property <[ContextMenuItemData]> items;
        in property <length> content-height;
        in-out property <int> selected-index <=> menu.selected-index;
        in-out property <bool> can-go-back <=> menu.can-go-back;
        in-out property <bool> menu-enabled <=> menu.enabled;
        out property <length> menu-x: menu.x;
        out property <length> menu-y: menu.y;
        out property <length> menu-height: menu.height;
        callback activated(string);
        callback navigate(int);
        callback enter-submenu();
        callback go-back();
        callback select-edge(bool);
        callback activate-selected();
        callback dismissed();
        menu := ContextMenu {
            menu-x: 40px; menu-bottom: 200px; prefer-below: true;
            window-width: root.width; window-height: root.height;
            items: root.items; content-height: root.content-height;
            hovered(index) => { root.selected-index = index; }
            item-selected(action) => { root.activated(action); }
            navigate(direction) => { root.navigate(direction); }
            enter-submenu => { root.enter-submenu(); }
            go-back => { root.go-back(); }
            select-edge(end) => { root.select-edge(end); }
            activate-selected => { root.activate-selected(); }
            close-menu => { root.dismissed(); }
        }
    }
    export component PanelProbe inherits Window {
        width: 140px; height: 80px; background: black;
        in property <[TaskbarItemData]> items;
        callback show-desktop();
        TaskbarWidget { x: 20px; y: 16px; width: 48px; height: 48px; content-width: 48; items: root.items; }
        ShowDesktopButton { x: 96px; y: 16px; width: 20px; height: 48px; clicked => { root.show-desktop(); } }
    }
}

#[test]
fn slider_keeps_drag_even_with_vertical_pointer_drift_and_scroll_still_works() {
    i_slint_backend_testing::init_no_event_loop();
    let window = ControlsProbe::new().unwrap();
    window.show().unwrap();
    let start = LogicalPosition::new(50., 80.);
    window.window().dispatch_event(WindowEvent::PointerPressed {
        position: start,
        button: PointerEventButton::Left,
    });
    for position in [
        LogicalPosition::new(100., 85.),
        LogicalPosition::new(220., 100.),
        LogicalPosition::new(340., 110.),
    ] {
        window
            .window()
            .dispatch_event(WindowEvent::PointerMoved { position });
    }
    assert!(
        window.get_value() > 90.,
        "The slider must retain the drag outside its initial row"
    );
    assert_eq!(
        window.get_scroll_position(),
        0.,
        "Slider movement must not pan settings content"
    );
    window
        .window()
        .dispatch_event(WindowEvent::PointerReleased {
            position: LogicalPosition::new(340., 110.),
            button: PointerEventButton::Left,
        });
    window
        .window()
        .dispatch_event(WindowEvent::PointerScrolled {
            position: LogicalPosition::new(180., 180.),
            delta_x: 0.,
            delta_y: -50.,
        });
    assert!(
        window.get_scroll_position() < 0.,
        "Disabling drag panning must preserve normal wheel scrolling"
    );
}

#[test]
fn menus_flip_to_the_available_side_and_stay_inside_small_outputs() {
    i_slint_backend_testing::init_no_event_loop();
    let window = ControlsProbe::new().unwrap();
    assert_eq!(window.invoke_menu_coordinate(30., 160., 600., true), 30.);
    assert_eq!(window.invoke_menu_coordinate(570., 160., 600., false), 410.);
    assert_eq!(window.invoke_menu_coordinate(300., 160., 600., false), 300.);
    assert_eq!(window.invoke_menu_coordinate(300., 160., 600., true), 140.);
    assert_eq!(window.invoke_menu_extent(240., 100.), 92.);
    for available in [5., 100., 400., 1920.] {
        let extent = window.invoke_menu_extent(240., available);
        for anchor in [-100., 0., 30., available - 1., available + 100.] {
            for before in [false, true] {
                let coordinate = window.invoke_menu_coordinate(anchor, extent, available, before);
                assert!(
                    coordinate >= 0. && coordinate + extent <= available,
                    "Menu {coordinate} + {extent} must fit output {available}"
                );
            }
        }
    }
}

#[test]
fn painted_context_menu_and_wayland_hit_region_have_the_same_bounds() {
    use slint::{Model, ModelRc, VecModel};
    use std::rc::Rc;
    i_slint_backend_testing::init_no_event_loop();
    let panel = RixPanelWindow::new().unwrap();
    panel.set_panel_width(400);
    panel.set_available_height(240);
    panel.set_context_content_height(180.);
    panel.set_context_menu_items(ModelRc::from(Rc::new(VecModel::from(vec![
        ContextMenuItemData {
            label: "Test action".into(),
            row_height: 36.,
            ..Default::default()
        },
    ]))));
    panel.set_context_menu_x(398.);
    panel.set_context_menu_top(235.);
    panel.set_context_menu_bottom(5.);
    panel.set_context_prefer_below(true);
    panel.set_is_context_menu_open(true);
    panel.show().unwrap();
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_secs(1));
    let menu = MenuProbe::new().unwrap();
    menu.show().unwrap();
    let position = slint::LogicalPosition::new(menu.get_menu_x(), menu.get_menu_y());
    let size = slint::LogicalSize::new(menu.get_menu_width(), menu.get_menu_height());
    let region = panel.get_surface_regions().row_data(5).unwrap();
    assert!(
        (position.x - region.x).abs() < 0.01 && (position.y - region.y).abs() < 0.01,
        "Painted menu {position:?} and input region ({}, {}) must agree",
        region.x,
        region.y
    );
    assert!((size.width - region.width).abs() < 0.01 && (size.height - region.height).abs() < 0.01);
    assert!(position.x >= 0. && position.x + size.width <= 400.);
    assert!(position.y >= 0. && position.y + size.height <= 240.);

    let theme = panel.global::<Theme>();
    theme.set_context_menu_radius(3.);
    theme.set_menu_frame(SkinFrame {
        available: true,
        corner_radius: 12.,
        ..Default::default()
    });
    assert_eq!(
        panel.get_surface_regions().row_data(5).unwrap().radius,
        3.,
        "The context menu input region must use the common compact radius, not SVG shadow padding"
    );
    theme.set_menu_frame(SkinFrame::default());
    assert_eq!(
        panel.get_surface_regions().row_data(5).unwrap().radius,
        3.,
        "An unskinned context menu must retain its configured fallback radius"
    );
}

#[test]
fn context_menu_uses_the_current_menu_frame_without_a_second_border() {
    use slint::{
        platform::{
            software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
            Platform, PlatformError, WindowAdapter,
        },
        Rgb8Pixel,
    };
    use std::{cell::RefCell, rc::Rc};

    struct SoftwarePlatform(Rc<MinimalSoftwareWindow>);
    impl Platform for SoftwarePlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
            Ok(self.0.clone())
        }
    }
    let surface = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(SoftwarePlatform(surface.clone()))).unwrap();
    let menu = MenuAppearanceProbe::new().unwrap();
    let theme = menu.global::<Theme>();
    theme.set_context_menu_width(294.);
    theme.set_font_size_md(14.);
    theme.set_is_aero(false);
    theme.set_context_menu_max_height(180.);
    theme.set_context_menu_padding(10.);
    theme.set_context_menu_radius(0.);
    theme.set_spacing_xs(4.);
    theme.set_menu_bg(slint::Color::from_rgb_u8(0, 40, 240));
    theme.set_surface(slint::Color::from_rgb_u8(240, 240, 0));
    theme.set_border_highlight(slint::Color::from_rgb_u8(240, 0, 0));
    theme.set_material_opacity(1.);

    let requests = Rc::new(RefCell::new(Vec::new()));
    let observed_requests = requests.clone();
    let green = slint::Image::from_rgba8(
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&[0u8, 240, 40, 255], 1, 1),
    );
    let purple = slint::Image::from_rgba8(
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            &[160u8, 0, 240, 255],
            1,
            1,
        ),
    );
    menu.global::<SkinRenderer>()
        .on_render_rounded(move |skin, width, height, square, radius| {
            observed_requests
                .borrow_mut()
                .push((skin.cache_key, width, height, square, radius));
            match skin.cache_key {
                11 => green.clone(),
                22 => purple.clone(),
                _ => panic!("Context menu requested a frame other than Theme.menu-frame"),
            }
        });
    menu.show().unwrap();
    surface.set_size(slint::PhysicalSize::new(400, 240));
    let render = || {
        slint::platform::update_timers_and_animations();
        surface.request_redraw();
        let mut pixels = vec![Rgb8Pixel::default(); 400 * 240];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 400);
        });
        pixels
    };
    let rgb = |pixels: &[Rgb8Pixel], x: usize, y: usize| {
        let pixel = pixels[y * 400 + x];
        (pixel.r, pixel.g, pixel.b)
    };

    let fallback = render();
    assert_eq!(
        rgb(&fallback, 140, 100),
        (0, 40, 240),
        "Unskinned context menus must use the menu palette, not the surface palette"
    );
    assert!(requests.borrow().is_empty());

    theme.set_is_aero(true);
    theme.set_menu_frame(SkinFrame {
        available: true,
        cache_key: 11,
        ..Default::default()
    });
    let skinned = render();
    for (x, y) in [(140, 100), (140, 40), (40, 100), (333, 100)] {
        assert_eq!(
            rgb(&skinned, x, y),
            (0, 240, 40),
            "The frame must supply both the center and edges without a palette border overlay"
        );
    }
    assert!(requests
        .borrow()
        .iter()
        .any(|request| { *request == (11, 294., 120., false, 0.) }));

    theme.set_menu_frame(SkinFrame {
        available: true,
        cache_key: 22,
        ..Default::default()
    });
    let switched = render();
    assert_eq!(
        rgb(&switched, 140, 100),
        (160, 0, 240),
        "An already open context menu must redraw after a theme-frame change"
    );
    assert!(requests.borrow().iter().any(|request| request.0 == 22));

    theme.set_material_opacity(0.5);
    let translucent = render();
    let center = rgb(&translucent, 140, 100);
    assert!(
        (79..=81).contains(&center.0) && center.1 == 0 && (119..=121).contains(&center.2),
        "The menu frame must honor the theme material opacity: {center:?}"
    );

    menu.set_embedded_popup(true);
    let opaque_frame = render();
    let center = rgb(&opaque_frame, 140, 100);
    assert!(
        (79..=81).contains(&center.0)
            && (19..=21).contains(&center.1)
            && (239..=241).contains(&center.2),
        "A host without native popups must keep its frame over a solid menu-palette backing: {center:?}"
    );
    assert!(requests.borrow().iter().any(|request| request.0 == 22));

    theme.set_material_opacity(1.);
    theme.set_menu_frame(SkinFrame::default());
    theme.set_is_aero(false);
    menu.set_embedded_popup(true);
    let restored_fallback = render();
    assert_eq!(
        rgb(&restored_fallback, 140, 100),
        (0, 40, 240),
        "Menus over shell content must retain the chosen menu palette"
    );

    menu.set_embedded_popup(false);
    theme.set_is_aero(true);
    theme.set_glass_start(slint::Color::from_rgb_u8(20, 60, 220));
    theme.set_glass_end(slint::Color::from_rgb_u8(120, 180, 20));
    let glass_fallback = render();
    let top = rgb(&glass_fallback, 140, 55);
    let bottom = rgb(&glass_fallback, 140, 145);
    assert!(
        top.2 > bottom.2 && top.1 < bottom.1,
        "An unskinned glass menu must use the shared glass gradient: {top:?}, {bottom:?}"
    );
    menu.set_embedded_popup(true);
    let opaque_fallback = render();
    assert_ne!(
        rgb(&opaque_fallback, 140, 100),
        (240, 240, 0),
        "An overlapping menu must continue using the shared menu palette or glass tint"
    );
}

#[test]
fn common_menu_rows_preserve_actions_submenus_and_keyboard_scrolling_at_all_scales() {
    use slint::{platform::Key, ModelRc, VecModel};
    use std::{cell::RefCell, rc::Rc};
    i_slint_backend_testing::init_no_event_loop();
    for scale in [1., 1.5, 2.] {
        let menu = MenuInteractionsProbe::new().unwrap();
        menu.set_menu_enabled(true);
        menu.set_can_go_back(true);
        let theme = menu.global::<Theme>();
        theme.set_context_menu_max_height(180.);
        theme.set_context_menu_padding(4.);
        let mut rows = vec![
            ContextMenuItemData {
                id: "normal".into(),
                label: "Normal".into(),
                ..Default::default()
            },
            ContextMenuItemData {
                separator: true,
                ..Default::default()
            },
            ContextMenuItemData {
                id: "disabled".into(),
                label: "Disabled".into(),
                disabled: true,
                ..Default::default()
            },
            ContextMenuItemData {
                id: "checked".into(),
                label: "Checked".into(),
                checkable: true,
                checked: true,
                ..Default::default()
            },
            ContextMenuItemData {
                id: "radio".into(),
                label: "Radio".into(),
                checkable: true,
                radio: true,
                checked: true,
                ..Default::default()
            },
            ContextMenuItemData {
                id: "submenu".into(),
                label: "Submenu".into(),
                submenu: true,
                ..Default::default()
            },
            ContextMenuItemData {
                id: "last".into(),
                label: "Last".into(),
                is_destructive: true,
                ..Default::default()
            },
        ];
        let mut content_height = 0.;
        for row in &mut rows {
            row.row_y = content_height;
            row.row_height = if row.separator { 9. } else { 34. };
            content_height += row.row_height;
        }
        menu.set_items(ModelRc::from(Rc::new(VecModel::from(rows))));
        menu.set_content_height(content_height);
        let actions = Rc::new(RefCell::new(Vec::new()));
        let observed = actions.clone();
        menu.on_activated(move |action| observed.borrow_mut().push(action.to_string()));
        let keys = Rc::new(RefCell::new(Vec::new()));
        let observed = keys.clone();
        menu.on_navigate(move |direction| {
            observed.borrow_mut().push(format!("navigate {direction}"))
        });
        let observed = keys.clone();
        menu.on_enter_submenu(move || observed.borrow_mut().push("submenu".to_string()));
        let observed = keys.clone();
        menu.on_go_back(move || observed.borrow_mut().push("back".to_string()));
        let observed = keys.clone();
        menu.on_select_edge(move |end| observed.borrow_mut().push(format!("edge {end}")));
        let observed = keys.clone();
        menu.on_activate_selected(move || observed.borrow_mut().push("activate".to_string()));
        let observed = keys.clone();
        menu.on_dismissed(move || observed.borrow_mut().push("dismiss".to_string()));
        menu.show().unwrap();
        menu.window()
            .dispatch_event(WindowEvent::ScaleFactorChanged {
                scale_factor: scale,
            });
        menu.window().dispatch_event(WindowEvent::Resized {
            size: slint::LogicalSize::new(400., 240.),
        });
        let click = |y: f32| {
            let position =
                LogicalPosition::new(menu.get_menu_x() + 100., menu.get_menu_y() + 4. + y);
            for event in [
                WindowEvent::PointerMoved { position },
                WindowEvent::PointerPressed {
                    position,
                    button: PointerEventButton::Left,
                },
                WindowEvent::PointerReleased {
                    position,
                    button: PointerEventButton::Left,
                },
            ] {
                menu.window().dispatch_event(event);
            }
        };
        click(17.);
        click(38.5);
        click(60.);
        click(94.);
        click(128.);
        assert_eq!(
            &*actions.borrow(),
            &["normal", "checked", "radio"],
            "Disabled and separator rows must not activate at scale {scale}"
        );
        assert_eq!(menu.get_selected_index(), 4);
        for key in [
            Key::DownArrow,
            Key::UpArrow,
            Key::RightArrow,
            Key::LeftArrow,
            Key::Home,
            Key::End,
            Key::Return,
            Key::Escape,
        ] {
            menu.window()
                .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
            menu.window()
                .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
        }
        assert_eq!(
            &*keys.borrow(),
            &[
                "navigate 1",
                "navigate -1",
                "submenu",
                "back",
                "edge false",
                "edge true",
                "activate",
                "back"
            ]
        );
        menu.set_can_go_back(false);
        menu.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::Escape.into(),
        });
        assert_eq!(keys.borrow().last().unwrap(), "dismiss");

        menu.set_selected_index(6);
        click(menu.get_menu_height() - 8. - 17.);
        assert_eq!(
            actions.borrow().last().unwrap(),
            "last",
            "Keyboard selection must reveal the final action at scale {scale}"
        );
        menu.set_menu_enabled(false);
        click(menu.get_menu_height() - 8. - 17.);
        assert_eq!(
            actions.borrow().len(),
            4,
            "A closing menu must not activate another action"
        );
        menu.hide().unwrap();
    }
}

#[test]
fn theme_hover_replaces_the_running_marker_and_desktop_button_keeps_its_shape() {
    use slint::{
        platform::{
            software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
            Platform, PlatformError, WindowAdapter,
        },
        ModelRc, Rgb8Pixel, VecModel,
    };
    use std::{cell::Cell, rc::Rc};
    struct SoftwarePlatform(Rc<MinimalSoftwareWindow>);
    impl Platform for SoftwarePlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
            Ok(self.0.clone())
        }
    }
    let surface = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(SoftwarePlatform(surface.clone()))).unwrap();
    let panel = PanelProbe::new().unwrap();
    let theme = panel.global::<Theme>();
    theme.set_task_width(48.);
    theme.set_task_padding(8.);
    theme.set_task_highlight_scale(1.);
    theme.set_task_highlight_radius_scale(1.);
    theme.set_task_indicator_height(3.);
    theme.set_task_indicator_width(12.);
    theme.set_task_indicator_active_width(24.);
    theme.set_task_hover_frame(SkinFrame {
        available: true,
        ..Default::default()
    });
    theme.set_spacing_xs(4.);
    theme.set_radius_sm(4.);
    theme.set_hover_duration(0.);
    theme.set_text_muted(slint::Color::from_rgb_u8(120, 120, 120));
    theme.set_border_highlight(slint::Color::from_rgb_u8(180, 180, 180));
    theme.set_accent(slint::Color::from_rgb_u8(0, 80, 255));
    let blue = slint::Image::from_rgba8(
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&[0u8, 80, 255, 255], 1, 1),
    );
    panel
        .global::<SkinRenderer>()
        .on_render(move |_, _, _, _| blue.clone());
    panel.set_items(ModelRc::from(Rc::new(VecModel::from(vec![
        TaskbarItemData {
            id: 1,
            is_running: true,
            ..Default::default()
        },
    ]))));
    panel.show().unwrap();
    surface.set_size(slint::PhysicalSize::new(140, 80));
    let render = || {
        slint::platform::update_timers_and_animations();
        surface.request_redraw();
        let mut pixels = vec![Rgb8Pixel::default(); 140 * 80];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 140);
        });
        pixels
    };
    let resting = render();
    assert!(
        resting[62 * 140 + 44].r > 100,
        "The unfocused running task keeps its marker"
    );
    panel.window().dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(44., 40.),
    });
    let hovered = render();
    let marker_pixel = hovered[62 * 140 + 44];
    assert!(
        marker_pixel.b > 200 && marker_pixel.r < 20,
        "Custom blue hover frame must not retain a grey running marker"
    );
    let resting_span: Vec<_> = (96..116).filter(|x| resting[40 * 140 + x].r > 80).collect();
    panel.window().dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(106., 40.),
    });
    let hovered = render();
    let hovered_span: Vec<_> = (96..116)
        .filter(|x| hovered[40 * 140 + x].b > 220 && hovered[40 * 140 + x].r < 20)
        .collect();
    assert_eq!(
        resting_span, hovered_span,
        "Show-desktop hover must not shift or widen the stem"
    );
    assert_eq!(resting_span, vec![105, 106, 107]);
    let clicks = Rc::new(Cell::new(0));
    let observed = clicks.clone();
    panel.on_show_desktop(move || observed.set(observed.get() + 1));
    for event in [
        WindowEvent::PointerPressed {
            position: LogicalPosition::new(106., 40.),
            button: PointerEventButton::Left,
        },
        WindowEvent::PointerReleased {
            position: LogicalPosition::new(106., 40.),
            button: PointerEventButton::Left,
        },
        WindowEvent::KeyPressed {
            text: slint::platform::Key::Return.into(),
        },
    ] {
        panel.window().dispatch_event(event);
    }
    assert_eq!(
        clicks.get(),
        2,
        "Show desktop must support pointer and keyboard activation"
    );
}
