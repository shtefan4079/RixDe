use slint::{
    platform::{
        software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType},
        Platform, PlatformError, WindowAdapter, WindowEvent,
    },
    ComponentHandle, LogicalPosition, Model, ModelRc, VecModel,
};
use std::rc::Rc;

slint::slint! {
    import { ActionsMenu, MenuItem } from "../../rix-files/ui/context_menu.slint";
    import { ContextMenu, ContextMenuItemData } from "../ui/widgets/context_menu.slint";
    import { Theme } from "../../../ui/common/theme.slint";
    import { SkinFrame, SkinRenderer } from "../../../ui/common/frame.slint";
    import { Palette } from "std-widgets.slint";
    export { Theme, SkinFrame, SkinRenderer, Palette, MenuItem, ContextMenuItemData }
    export component MenuParityProbe inherits Window {
        width: 640px; height: 200px; background: transparent;
        default-font-family: Theme.font-family;
        default-font-size: Theme.font-size-md;
        in property <[MenuItem]> files-items;
        in property <[ContextMenuItemData]> shell-items;
        in property <length> content-height;
        in property <bool> material-supported: true;
        in property <bool> opaque;
        in-out property <int> shell-selected: -1;
        out property <length> files-x: holder.x + files.x;
        out property <length> files-y: holder.y + files.y;
        out property <length> files-width: files.width;
        out property <length> files-height: files.height;
        out property <length> shell-x: shell.x;
        out property <length> shell-y: shell.y;
        out property <length> shell-width: shell.width;
        out property <length> shell-height: shell.height;
        pure callback rows-height([MenuItem], length, length) -> length;
        public function clear-selection() { files.focus-menu(); root.shell-selected = -1; }
        holder := Rectangle {
            x: 16px; y: 16px; width: 294px;
            height: root.content-height + Theme.context-menu-padding * 2;
            files := ActionsMenu {
                open: true; native-surface: true;
                window-width: parent.width; window-height: parent.height;
                material-supported: root.material-supported;
                opaque: root.opaque;
                accessible-name: "Files menu";
                items: root.files-items;
                rows-height(items, row-height, separator-height) => {
                    root.rows-height(items, row-height, separator-height)
                }
                items-width(items, font-size, min-width) => { min-width }
            }
        }
        shell := ContextMenu {
            menu-x: 336px; menu-bottom: root.height - 16px; prefer-below: true;
            window-width: root.width; window-height: root.height;
            material-supported: root.material-supported;
            opaque: root.opaque;
            items: root.shell-items;
            content-height: root.content-height;
            selected-index: root.shell-selected;
            hovered(index) => { root.shell-selected = index; }
        }
    }
}

#[path = "../../../ui/common/skins.rs"]
mod skins;
#[path = "../../../ui/common/bindings.rs"]
mod theme_bindings;

struct SoftwarePlatform(Rc<MinimalSoftwareWindow>);
impl Platform for SoftwarePlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}

fn populate(probe: &MenuParityProbe) {
    let theme = probe.global::<Theme>();
    let row = theme.get_menu_row_height();
    let separator = theme.get_default_spacing() + 1.;
    let files = vec![
        MenuItem {
            action: "open".into(),
            label: "Open folder".into(),
            enabled: true,
            ..Default::default()
        },
        MenuItem {
            separator: true,
            ..Default::default()
        },
        MenuItem {
            action: "paste".into(),
            label: "Paste".into(),
            ..Default::default()
        },
        MenuItem {
            action: "delete".into(),
            label: "Delete".into(),
            enabled: true,
            destructive: true,
            ..Default::default()
        },
        MenuItem {
            action: "hidden".into(),
            label: "Show hidden files".into(),
            enabled: true,
            checked: true,
            ..Default::default()
        },
    ];
    let mut row_y = 0.;
    let shell = files
        .iter()
        .map(|item| {
            let row_height = if item.separator { separator } else { row };
            let entry = ContextMenuItemData {
                id: item.action.clone(),
                label: item.label.clone(),
                disabled: !item.enabled,
                separator: item.separator,
                is_destructive: item.destructive,
                checked: item.checked,
                checkable: item.checked,
                row_y,
                row_height,
                ..Default::default()
            };
            row_y += row_height;
            entry
        })
        .collect::<Vec<_>>();
    probe.set_content_height(row_y);
    probe.set_files_items(ModelRc::from(Rc::new(VecModel::from(files))));
    probe.set_shell_items(ModelRc::from(Rc::new(VecModel::from(shell))));
    probe.invoke_clear_selection();
}

fn crop(
    pixels: &[PremultipliedRgbaColor],
    stride: u32,
    scale: f32,
    bounds: (f32, f32, f32, f32),
) -> (u32, u32, Vec<u8>) {
    let (x, y, width, height) = bounds;
    let x = (x * scale).round() as u32;
    let y = (y * scale).round() as u32;
    let width = (width * scale).round() as u32;
    let height = (height * scale).round() as u32;
    let rgba = (y..y + height)
        .flat_map(|row| {
            (x..x + width).flat_map(move |column| {
                let pixel = pixels[(row * stride + column) as usize];
                [pixel.red, pixel.green, pixel.blue, pixel.alpha]
            })
        })
        .collect();
    (width, height, rgba)
}

#[test]
fn files_and_shell_context_menus_have_identical_rgba_at_all_scales_and_live_themes() {
    let surface = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(SoftwarePlatform(surface.clone()))).unwrap();
    let probe = MenuParityProbe::new().unwrap();
    skins::install(probe.global::<SkinRenderer>(), probe.as_weak());
    probe.on_rows_height(|items, row, separator| {
        items
            .iter()
            .map(|item| if item.separator { separator } else { row })
            .sum()
    });
    let output = std::env::temp_dir().join("rix-common-menu-parity");
    std::fs::create_dir_all(&output).unwrap();
    let fixtures = tempfile::tempdir().unwrap();
    let frame = |name: &str, color: &str| {
        let parts = rix_assets::FRAME_PARTS.iter().enumerate().map(|(index, part)| {
            format!("<rect id='{part}' x='{}' y='{}' width='8' height='8' fill='{color}' fill-opacity='0.7'/>", index % 3 * 8, index / 3 * 8)
        }).collect::<String>();
        let path = fixtures.path().join(name);
        std::fs::write(
            &path,
            format!("<svg xmlns='http://www.w3.org/2000/svg' width='24' height='24'>{parts}</svg>"),
        )
        .unwrap();
        path.to_string_lossy().into_owned()
    };
    let mut glass = rix_config::ThemeSettings::aero_glass();
    glass.appearance.menu_frame.clear();
    glass.opacity = 0.65;
    let mut custom = glass.clone();
    custom.appearance.menu_frame = frame("first-menu.svg", "#18b040");
    let mut changed = custom.clone();
    changed.appearance.menu_frame = frame("changed-menu.svg", "#a020c0");
    changed
        .appearance
        .metrics
        .insert("context_menu_radius".into(), 3.);
    changed
        .appearance
        .metrics
        .insert("context_menu_padding".into(), 6.);
    changed
        .appearance
        .metrics
        .insert("menu_row_height".into(), 32.);
    changed
        .appearance
        .metrics
        .insert("default_spacing".into(), 10.);
    let mut flat = rix_config::ThemeSettings {
        blur_radius: 0,
        ..Default::default()
    };
    flat.appearance.menu_frame.clear();
    let cases = [
        ("flat", flat, true, false),
        ("glass", glass, true, false),
        ("custom", custom.clone(), true, false),
        ("live-changed", changed, true, false),
        ("unsupported", custom.clone(), false, false),
        ("opaque", custom, true, true),
        (
            "live-light",
            rix_config::ThemeSettings::crisp_light(),
            true,
            false,
        ),
    ];
    probe.show().unwrap();
    let mut proof = Vec::new();
    for scale in [1., 1.5, 2.] {
        probe
            .window()
            .dispatch_event(WindowEvent::ScaleFactorChanged {
                scale_factor: scale,
            });
        surface.set_size(slint::PhysicalSize::new(
            (640. * scale) as u32,
            (200. * scale) as u32,
        ));
        for (name, theme, supported, opaque) in &cases {
            theme_bindings::apply(probe.global::<Theme>(), probe.global::<Palette>(), theme);

            probe.global::<Theme>().set_font_size_md(14.);
            probe.global::<Theme>().set_context_menu_width(294.);
            probe.set_material_supported(*supported);
            probe.set_opaque(*opaque);
            populate(&probe);
            let mut idle_pixels = None;
            for selection in [-1, 0, 3] {
                probe.invoke_clear_selection();
                if selection >= 0 {
                    let theme = probe.global::<Theme>();
                    let row_y = if selection == 0 {
                        0.
                    } else {
                        theme.get_menu_row_height() * 2. + theme.get_default_spacing() + 1.
                    };
                    let y = 16.
                        + theme.get_context_menu_padding()
                        + row_y
                        + theme.get_menu_row_height() / 2.;
                    for x in [116., 436.] {
                        probe.window().dispatch_event(WindowEvent::PointerMoved {
                            position: LogicalPosition::new(x, y),
                        });
                    }
                } else {
                    probe.window().dispatch_event(WindowEvent::PointerMoved {
                        position: LogicalPosition::new(635., 195.),
                    });
                }
                slint::platform::update_timers_and_animations();
                surface.request_redraw();
                let size = surface.size();
                let mut pixels =
                    vec![PremultipliedRgbaColor::default(); (size.width * size.height) as usize];
                surface.draw_if_needed(|renderer| {
                    renderer.render(&mut pixels, size.width as usize);
                });
                let files_bounds = (
                    probe.get_files_x(),
                    probe.get_files_y(),
                    probe.get_files_width(),
                    probe.get_files_height(),
                );
                let shell_bounds = (
                    probe.get_shell_x(),
                    probe.get_shell_y(),
                    probe.get_shell_width(),
                    probe.get_shell_height(),
                );
                assert_eq!(
                    files_bounds.1, shell_bounds.1,
                    "Vertical origins differ for {name}"
                );
                assert_eq!(files_bounds.2, shell_bounds.2, "Widths differ for {name}");
                assert_eq!(
                    files_bounds.3, shell_bounds.3,
                    "Row heights/padding differ for {name}"
                );
                let files = crop(&pixels, size.width, scale, files_bounds);
                let shell = crop(&pixels, size.width, scale, shell_bounds);
                let case = format!("{name}-scale{scale}-selected{selection}");
                for (app, (width, height, rgba)) in [("files", &files), ("shell", &shell)] {

                    let png = rgba
                        .chunks_exact(4)
                        .flat_map(|pixel| {
                            let alpha = pixel[3] as u32;
                            let straight = |channel: u8| {
                                ((channel as u32 * 255 + alpha / 2)
                                    .checked_div(alpha)
                                    .unwrap_or(0)
                                    .min(255)) as u8
                            };
                            [
                                straight(pixel[0]),
                                straight(pixel[1]),
                                straight(pixel[2]),
                                pixel[3],
                            ]
                        })
                        .collect::<Vec<_>>();
                    image::save_buffer(
                        output.join(format!("{case}-{app}.png")),
                        &png,
                        *width,
                        *height,
                        image::ColorType::Rgba8,
                    )
                    .unwrap();
                }
                let mismatched = files
                    .2
                    .chunks_exact(4)
                    .zip(shell.2.chunks_exact(4))
                    .filter(|(a, b)| a != b)
                    .count();
                assert_eq!(mismatched, 0, "Actual Files and Shell menus differ in {mismatched} RGBA pixels for {case}; inspect {}", output.display());
                assert!(
                    files.2.chunks_exact(4).any(|pixel| pixel[3] != 0),
                    "A blank menu must not pass parity"
                );
                if selection == -1 {
                    let theme = probe.global::<Theme>();
                    let sample_x = files.0 - (2. * scale).round() as u32;
                    let sample_y = (files.1 / 2).max(1);
                    let alpha = files.2[((sample_y * files.0 + sample_x) * 4 + 3) as usize];
                    if *name == "flat" || !supported || *opaque {
                        assert_eq!(alpha, 255, "Opaque fallback must cover the menu for {case}");
                    } else {
                        assert!(
                            alpha > 0 && alpha < 255,
                            "The menu material must preserve alpha for {case}: {alpha}"
                        );
                    }
                    if *name == "custom" || *name == "live-changed" {
                        let offset = ((sample_y * files.0 + sample_x) * 4) as usize;
                        let expected = if *name == "custom" {
                            [24i32, 176, 64]
                        } else {
                            [160i32, 32, 192]
                        };
                        for (channel, expected) in files.2[offset..offset + 3].iter().zip(expected)
                        {
                            let straight = *channel as i32 * 255 / alpha as i32;
                            assert!((straight - expected).abs() <= 3, "The actual Theme.menu-frame must render and redraw live for {case}: channel {straight} vs {expected}");
                        }
                    }
                    for row_y in [
                        0.,
                        theme.get_menu_row_height() + theme.get_default_spacing() + 1.,
                        theme.get_menu_row_height() * 2. + theme.get_default_spacing() + 1.,
                        theme.get_menu_row_height() * 3. + theme.get_default_spacing() + 1.,
                    ] {
                        let y = ((theme.get_context_menu_padding()
                            + row_y
                            + theme.get_menu_row_height() / 2.)
                            * scale)
                            .round() as u32;
                        let left = (15. * scale).round() as u32;
                        let right = (210. * scale).round() as u32;
                        let line = &files.2[((y * files.0 + left) * 4) as usize
                            ..((y * files.0 + right) * 4) as usize];
                        assert!(line.chunks_exact(4).any(|pixel| pixel != &line[..4]), "Normal, disabled, destructive and checked labels must actually render for {case}, row {row_y}");
                    }
                    idle_pixels = Some(files.2.clone());
                } else {
                    assert_ne!(
                        idle_pixels.as_ref().unwrap(),
                        &files.2,
                        "Hover highlights must actually render for {case}"
                    );
                }
                proof.push(serde_json::json!({"case": case, "width": files.0, "height": files.1, "mismatched_pixels": mismatched}));
            }
        }
    }
    std::fs::write(
        output.join("proof.json"),
        serde_json::to_vec_pretty(&proof).unwrap(),
    )
    .unwrap();
}
