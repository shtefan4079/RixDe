use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter, WindowEvent};
use slint::ComponentHandle;
use std::{cell::RefCell, rc::Rc};
slint::slint! {
    import { NetworkView, NetworkRow, NetworkDeviceRow, NetworkProfileDraft, SystemState } from "../../../ui/common/system.slint";
    import { Theme } from "../../../ui/common/theme.slint";
    export { SkinFrame, SkinRenderer } from "../../../ui/common/frame.slint";
    export { SystemState, NetworkRow, NetworkDeviceRow, NetworkProfileDraft, Theme }
    export { Palette } from "std-widgets.slint";
    export component NetworkProbe inherits Window {
        in-out property <length> probe-width: 440px;
        in-out property <length> probe-height: 640px;
        width: root.probe-width; height: root.probe-height;
        background: Theme.menu-bg;
        in-out property <bool> active: true;
        in-out property <bool> full-mode;
        out property <bool> asking: view.password-prompt-open;
        public function prompt() { view.begin-password("fixture-row", "Fixture Wi-Fi", "wpa-psk"); }
        view := NetworkView { active: root.active; full-mode: root.full-mode; width: 100%; height: 100%; }
    }
}
#[path = "../../../ui/common/skins.rs"]
mod skins;
#[path = "../../../ui/common/bindings.rs"]
mod theme_bindings;

struct PrivatePlatform(Rc<MinimalSoftwareWindow>);
impl Platform for PrivatePlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}
fn render(surface: &MinimalSoftwareWindow) {
    slint::platform::update_timers_and_animations();
    let size = surface.window().size();
    let mut pixels = vec![slint::Rgb8Pixel::default(); size.width as usize * size.height as usize];
    surface.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, size.width as usize);
    });
}
fn snapshot(surface: &MinimalSoftwareWindow, name: &str) {
    if let Ok(directory) = std::env::var("RIX_NETWORK_PROBE_DIR") {

        std::thread::sleep(std::time::Duration::from_millis(200));
        slint::platform::update_timers_and_animations();
        surface.window().request_redraw();
        let size = surface.window().size();
        let mut pixels =
            vec![slint::Rgb8Pixel::default(); size.width as usize * size.height as usize];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, size.width as usize);
        });
        let bytes: Vec<u8> = pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
        std::fs::create_dir_all(&directory).unwrap();
        image::save_buffer(
            std::path::Path::new(&directory).join(name),
            &bytes,
            size.width,
            size.height,
            image::ColorType::Rgb8,
        )
        .unwrap();
    }
}
fn press(ui: &NetworkProbe, value: &str) {
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: value.into() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text: value.into() });
}

fn click(ui: &NetworkProbe, x: f32, y: f32) {
    let position = slint::LogicalPosition::new(x, y);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
}

#[test]
fn compact_passive_traffic_cards_render_in_themed_small_and_large_popups() {
    let surface = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(PrivatePlatform(surface.clone()))).unwrap();
    let ui = NetworkProbe::new().unwrap();
    skins::install(ui.global::<SkinRenderer>(), ui.as_weak());
    theme_bindings::apply(
        ui.global::<Theme>(),
        ui.global::<Palette>(),
        &rix_config::ThemeSettings::default(),
    );
    ui.global::<Theme>().set_font_family("DejaVu Sans".into());
    let state = ui.global::<SystemState>();
    state.set_network_active_name("Office Wi-Fi".into());
    state.set_connectivity("Connected to the internet".into());
    state.set_network_traffic_visible(true);
    state.set_network_download_rate("12.5 MiB/s".into());
    state.set_network_upload_rate("640 KiB/s".into());
    state.set_network_service_ready(true);
    state.set_networking_enabled(true);
    state.set_network_has_wifi(true);
    state.set_wifi_enabled(true);
    state.set_wifi_hardware(true);
    state.set_network_can_toggle(true);
    state.set_network_can_scan(true);
    ui.show().unwrap();
    for width in [360, 440] {
        ui.set_probe_width(width as f32);
        ui.set_probe_height(560.);
        surface.set_size(slint::PhysicalSize::new(width, 560));
        render(&surface);
        snapshot(&surface, &format!("traffic-{width}.png"));
        assert_eq!(state.get_network_download_rate(), "12.5 MiB/s");
        assert_eq!(state.get_network_upload_rate(), "640 KiB/s");
        assert!(!ui.get_asking());
    }
    state.set_network_download_rate("—".into());
    state.set_network_upload_rate("—".into());
    render(&surface);
    snapshot(&surface, "traffic-sampling.png");
}

#[test]
fn full_settings_profiles_keep_themed_tabs_and_actions_inside_the_viewport() {
    let surface = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(PrivatePlatform(surface.clone()))).unwrap();
    let ui = NetworkProbe::new().unwrap();
    skins::install(ui.global::<SkinRenderer>(), ui.as_weak());
    theme_bindings::apply(
        ui.global::<Theme>(),
        ui.global::<Palette>(),
        &rix_config::ThemeSettings::default(),
    );
    ui.global::<Theme>().set_menu_action_size(36.);
    ui.global::<Theme>().set_font_family("DejaVu Sans".into());
    ui.set_full_mode(true);
    let state = ui.global::<SystemState>();
    state.set_network_has_wifi(true);
    state.set_wifi_hardware(true);
    state.set_wifi_enabled(true);
    state.set_networking_enabled(true);
    state.set_network_service_ready(true);
    state.set_network_can_networking(true);
    state.set_network_can_toggle(true);
    state.set_network_can_connect(true);
    state.set_network_can_scan(true);
    state.set_network_can_save_own(true);
    state.set_network_can_save_shared(true);
    state.set_connectivity("Connected to the internet".into());
    state.set_network_devices(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        NetworkDeviceRow {
            path: "/fixture/wlan0".into(),
            name: "wlan0".into(),
            kind: 2,
            managed: true,
            hardware: true,
            available: true,
            ..Default::default()
        },
    ]))));
    state.set_networks(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        NetworkRow {
            path: "fixture-row".into(),
            name: "Office Wi-Fi with a complete long SSID and several devices".into(),
            detail: "WPA3 · 31% · Saved · wlan0 · 192.168.1.20".into(),
            active: true,
            saved: true,
            supported: true,
            available: true,
            editable: true,
            security: "sae".into(),
            icon: slint::Image::load_from_svg_data(include_bytes!(
                "../../../ui/common/icons/network-weak.svg"
            ))
            .unwrap(),
            ..Default::default()
        },
    ]))));
    ui.show().unwrap();
    ui.set_probe_width(640.);
    ui.set_probe_height(620.);
    surface.set_size(slint::PhysicalSize::new(640, 620));
    render(&surface);
    snapshot(&surface, "settings-networks-640.png");
    state.set_network_profile_draft(NetworkProfileDraft {
        path: "/fixture/profile".into(),
        uuid: "a84a29f7-ec2a-4d94-a9f7-71312d7c5828".into(),
        name: "Office Wi-Fi".into(),
        ssid: "Office Wi-Fi with a long descriptive name".into(),
        kind: "802-11-wireless".into(),
        security: "wpa-eap".into(),
        eap: "peap".into(),
        phase2_auth: "mschapv2".into(),
        username: "fixture-user".into(),
        anonymous_identity: "anonymous@example.org".into(),
        domain_suffix_match: "radius.example.org".into(),
        ipv4_method: "manual".into(),
        ipv4_addresses: "192.168.1.20/24".into(),
        ipv4_gateway: "192.168.1.1".into(),
        ipv4_dns: "1.1.1.1, 8.8.8.8".into(),
        ipv4_routes: "10.0.0.0/8 192.168.1.1 100".into(),
        ipv6_method: "auto".into(),
        proxy_method: "none".into(),
        autoconnect: true,
        remember: true,
        ..Default::default()
    });
    state.set_network_profile_editing(true);
    let submitted = Rc::new(RefCell::new(Vec::new()));
    let received = submitted.clone();
    state.on_network_save_profile(move |draft, activate, _, _| {
        received.borrow_mut().push((draft, activate));
    });
    render(&surface);
    snapshot(&surface, "settings-profile-general-640.png");

    click(&ui, 196., 122.);
    render(&surface);
    snapshot(&surface, "settings-profile-security-640.png");
    click(&ui, 320., 122.);
    render(&surface);
    snapshot(&surface, "settings-profile-ipv4-640.png");

    state.set_network_profile_busy(true);
    render(&surface);
    click(&ui, 468., 588.);
    assert!(
        submitted.borrow().is_empty(),
        "Saving is disabled while busy"
    );
    state.set_network_profile_busy(false);
    state.set_network_can_save_own(false);
    render(&surface);
    click(&ui, 468., 588.);
    assert!(
        submitted.borrow().is_empty(),
        "Policy must prevent profile writes"
    );
    state.set_network_can_save_own(true);
    render(&surface);
    click(&ui, 468., 588.);
    assert_eq!(
        submitted.borrow().len(),
        1,
        "Save must be reachable in a 640px viewport"
    );
    assert!(
        !submitted.borrow()[0].1,
        "Save must not activate the profile"
    );
    assert_eq!(submitted.borrow()[0].0.ipv4_addresses, "192.168.1.20/24");
    assert_eq!(
        submitted.borrow()[0].0.ipv4_routes,
        "10.0.0.0/8 192.168.1.1 100"
    );
    state.set_network_error("Fixture validation error: check the configured route".into());
    render(&surface);
    assert!(
        state.get_network_profile_editing(),
        "Failure must retain the draft"
    );
    assert_eq!(
        state.get_network_profile_draft().ssid,
        "Office Wi-Fi with a long descriptive name"
    );
    state.set_network_error("".into());
    ui.set_probe_width(760.);
    ui.set_probe_height(660.);
    surface.set_size(slint::PhysicalSize::new(760, 660));
    render(&surface);
    snapshot(&surface, "settings-profile-ipv4-760.png");
    click(&ui, 687., 628.);
    assert_eq!(
        submitted.borrow().len(),
        2,
        "Save and connect must remain reachable after resize"
    );
    assert!(submitted.borrow()[1].1);
}

#[test]
fn enter_validation_failed_retry_success_and_close_clear_the_prompt() {
    let surface = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(PrivatePlatform(surface.clone()))).unwrap();
    let ui = NetworkProbe::new().unwrap();
    skins::install(ui.global::<SkinRenderer>(), ui.as_weak());
    theme_bindings::apply(
        ui.global::<Theme>(),
        ui.global::<Palette>(),
        &rix_config::ThemeSettings::default(),
    );
    ui.global::<Theme>().set_menu_action_size(36.);
    ui.global::<Theme>().set_font_family("DejaVu Sans".into());
    let state = ui.global::<SystemState>();
    state.set_network_has_wifi(true);
    state.set_wifi_hardware(true);
    state.set_wifi_enabled(true);
    state.set_network_service_ready(true);
    state.on_network_validate_password(|_, pw| {
        if pw.len() >= 8 {
            "".into()
        } else {
            "Fixture validation error".into()
        }
    });
    let calls = Rc::new(RefCell::new(Vec::<String>::new()));
    let received = calls.clone();
    state.on_network_connect(move |_, pw, _| received.borrow_mut().push(pw.into()));
    ui.show().unwrap();
    surface.set_size(slint::PhysicalSize::new(440, 640));
    ui.invoke_prompt();
    render(&surface);
    snapshot(&surface, "password.png");
    press(&ui, "\n");
    assert!(calls.borrow().is_empty(), "Empty Enter must not submit");
    press(&ui, "short");
    press(&ui, "\n");
    assert!(calls.borrow().is_empty(), "Short Enter must not submit");
    press(&ui, "-valid");
    press(&ui, "\n");
    assert_eq!(calls.borrow().len(), 1);
    assert!(
        ui.get_asking(),
        "The form must remain visible during activation"
    );
    state.set_network_form_error("Fixture rejected password".into());
    state.set_network_form_state(3);
    render(&surface);
    assert!(
        ui.get_asking(),
        "Failure must retain the form for correction"
    );
    press(&ui, "\n");
    assert_eq!(calls.borrow().len(), 2, "A failed request must allow retry");
    state.set_network_form_state(2);
    render(&surface);
    assert!(!ui.get_asking());
    ui.invoke_prompt();
    render(&surface);
    press(&ui, "\n");
    assert_eq!(calls.borrow().len(), 2, "A new prompt must start empty");
    press(&ui, "fixture-password");
    ui.set_active(false);
    render(&surface);
    assert!(!ui.get_asking());
    ui.set_active(true);
    ui.invoke_prompt();
    render(&surface);
    press(&ui, "\n");
    assert_eq!(calls.borrow().len(), 2, "Closing must clear the secret");
    let canceled = Rc::new(RefCell::new(Vec::new()));
    let received = canceled.clone();
    state.on_network_reply_secrets(move |id, _, _, _, _, cancel| {
        if cancel {
            received.borrow_mut().push(id.to_string());
        }
        true
    });
    state.set_network_credential_id("123".into());
    state.set_network_credential_name("Fixture automatic reconnect".into());
    state.set_network_credential_password(true);
    render(&surface);
    ui.set_active(false);
    render(&surface);
    assert_eq!(*canceled.borrow(), vec!["123"]);
    state.set_network_credential_id("".into());
    state.set_network_form_state(2);
    ui.set_active(true);
    render(&surface);
    state.set_connectivity("Connected to the internet".into());
    state.set_network_can_toggle(true);
    state.set_network_can_connect(true);
    state.set_network_can_scan(true);
    state.set_networking_enabled(true);
    state.set_networks(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        NetworkRow {
            path: "fixture-1".into(),
            name: "Fixture Office Wi-Fi with a complete long SSID".into(),
            detail: "WPA3 · 31% · Saved · wlan0".into(),
            active: true,
            saved: true,
            supported: true,
            available: true,
            editable: true,
            security: "sae".into(),
            icon: slint::Image::load_from_svg_data(include_bytes!(
                "../../../ui/common/icons/network-weak.svg"
            ))
            .unwrap(),
            ..Default::default()
        },
        NetworkRow {
            path: "fixture-2".into(),
            name: "Guest network".into(),
            detail: "Open network · 82% · wlan0".into(),
            supported: true,
            available: true,
            editable: true,
            security: "open".into(),
            icon: slint::Image::load_from_svg_data(include_bytes!(
                "../../../ui/common/icons/network-wireless.svg"
            ))
            .unwrap(),
            ..Default::default()
        },
    ]))));
    render(&surface);
    snapshot(&surface, "networks.png");
}
