use crate::model::{LayoutConfig, ThemeSettings, WidgetConfig, WidgetKind, WidgetSection};
use crate::parser::ConfigError;
use rix_core::Position;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug)]
pub struct PlasmaSnapshot {
    pub layout: LayoutConfig,
    pub theme: ThemeSettings,
    pub pinned_apps: Vec<String>,
}

#[derive(Default)]
struct Ini(HashMap<String, HashMap<String, String>>);

impl Ini {
    fn parse(input: &str) -> Self {
        let mut groups = HashMap::<String, HashMap<String, String>>::new();
        let mut section = String::new();
        for line in input.lines().map(str::trim) {
            if line.starts_with('[') && line.ends_with(']') {
                section = line.to_owned();
            } else if let Some((key, value)) = line.split_once('=') {
                groups
                    .entry(section.clone())
                    .or_default()
                    .insert(key.trim().to_owned(), value.trim().to_owned());
            }
        }
        Self(groups)
    }

    fn get<'a>(&'a self, section: &str, key: &str) -> Option<&'a str> {
        self.0.get(section)?.get(key).map(String::as_str)
    }
}

impl PlasmaSnapshot {
    pub fn read_from(config_dir: &Path, mut theme: ThemeSettings) -> Result<Self, ConfigError> {
        let applets = Ini::parse(&fs::read_to_string(
            config_dir.join("plasma-org.kde.plasma.desktop-appletsrc"),
        )?);
        let views = Ini::parse(&fs::read_to_string(config_dir.join("plasmashellrc"))?);
        let kdeglobals = fs::read_to_string(config_dir.join("kdeglobals"))
            .map(|text| Ini::parse(&text))
            .unwrap_or_default();
        let mut preferred = Ini::default();
        for name in ["mimeapps.list", "kde-mimeapps.list"] {
            if let Ok(text) = fs::read_to_string(config_dir.join(name)) {
                for (section, values) in Ini::parse(&text).0 {
                    preferred.0.entry(section).or_default().extend(values);
                }
            }
        }
        let stats = fs::read_to_string(config_dir.join("kactivitymanagerd-statsrc"))
            .map(|text| Ini::parse(&text))
            .unwrap_or_default();

        let mut panel_ids: Vec<&str> = applets
            .0
            .iter()
            .filter(|(section, values)| {
                section.starts_with("[Containments][")
                    && section.matches('[').count() == 2
                    && values
                        .get("plugin")
                        .is_some_and(|plugin| plugin == "org.kde.panel")
            })
            .filter_map(|(section, _)| section.strip_prefix("[Containments]["))
            .filter_map(|section| section.strip_suffix(']'))
            .collect();
        panel_ids.sort_by_key(|id| {
            let section = format!("[Containments][{id}]");
            (
                applets.get(&section, "location") != Some("4"),
                id.parse::<u32>().unwrap_or(u32::MAX),
            )
        });
        let panel_id = panel_ids
            .first()
            .copied()
            .ok_or_else(|| ConfigError::InvalidValue("No Plasma panel found".into()))?;

        let mut layout = LayoutConfig::default_layout();
        let panel = &mut layout.panels[0];
        let panel_section = format!("[Containments][{panel_id}]");
        let view_section = format!("[PlasmaViews][Panel {panel_id}]");
        let view_defaults = format!("{view_section}[Defaults]");
        panel.thickness = views
            .get(&view_defaults, "thickness")
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(panel.thickness)
            .clamp(24, 120);
        panel.floating = views.get(&view_section, "floating") == Some("1");

        panel.floating_margin = 0;
        panel.position = match applets.get(&panel_section, "location") {
            Some("3") => Position::Top,
            Some("5") => Position::Left,
            Some("6") => Position::Right,
            _ => Position::Bottom,
        };

        let order_section = format!("{panel_section}[General]");
        let ordered_ids = applets
            .get(&order_section, "AppletOrder")
            .unwrap_or("")
            .split(';');
        let mut section = WidgetSection::Start;
        let mut widgets = Vec::new();
        let mut kickoff_id = None;
        let mut taskbar_id = None;
        for id in ordered_ids {
            let applet_section = format!("{panel_section}[Applets][{id}]");
            let plugin = applets.get(&applet_section, "plugin").unwrap_or("");
            if plugin == "org.kde.plasma.panelspacer" {
                section = match section {
                    WidgetSection::Start => WidgetSection::Center,
                    _ => WidgetSection::End,
                };
                continue;
            }
            let kind = match plugin {
                "org.kde.plasma.kickoff" | "org.kde.plasma.kicker" => {
                    kickoff_id = Some(id.to_owned());
                    WidgetKind::Launcher
                }
                "org.kde.plasma.icontasks" | "org.kde.plasma.taskmanager" => {
                    taskbar_id = Some(id.to_owned());
                    WidgetKind::Taskbar
                }
                "org.kde.plasma.systemtray" => WidgetKind::Tray,
                "org.kde.plasma.digitalclock" => WidgetKind::Clock,
                "org.kde.plasma.showdesktop" => WidgetKind::ShowDesktop,
                _ => continue,
            };
            let mut widget = WidgetConfig::new(format!("plasma-{id}"), kind, section);
            if widget.kind == WidgetKind::Taskbar {
                widget = widget.with_property("show-labels", false);
            }
            widgets.push(widget);
        }
        if !widgets.is_empty() {
            panel.widgets = widgets;
        }

        if let Some(kickoff_id) = kickoff_id {
            let base = format!("{panel_section}[Applets][{kickoff_id}][Configuration]");
            layout.launcher.width = applets
                .get(&base, "popupWidth")
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(layout.launcher.width)
                .clamp(400, 1200);
            layout.launcher.height = applets
                .get(&base, "popupHeight")
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(layout.launcher.height)
                .clamp(400, 1000);
            if let Some(icon) = applets.get(&format!("{base}[General]"), "icon") {
                layout.launcher.icon = icon.to_owned();
            }
            let favorites_section = format!(
                "[Favorites-org.kde.plasma.kickoff.favorites.instance-{kickoff_id}-global]"
            );
            if let Some(order) = stats.get(&favorites_section, "ordering") {
                layout.launcher.favorites = order
                    .split(',')
                    .filter_map(|entry| resolve_desktop_id(entry, &kdeglobals, &preferred))
                    .collect();
            }
        }

        let pinned_apps = taskbar_id
            .and_then(|id| {
                let section = format!("{panel_section}[Applets][{id}][Configuration][General]");
                applets.get(&section, "launchers")
            })
            .map(|launchers| {
                launchers
                    .split(',')
                    .filter_map(|entry| resolve_desktop_id(entry, &kdeglobals, &preferred))
                    .collect()
            })
            .unwrap_or_default();

        let keyboard = read_ini(config_dir.join("kxkbrc"));
        if keyboard.get("[Layout]", "Use") == Some("true") {
            let variants: Vec<_> = keyboard
                .get("[Layout]", "VariantList")
                .unwrap_or("")
                .split(',')
                .collect();
            let names: Vec<_> = keyboard
                .get("[Layout]", "DisplayNames")
                .unwrap_or("")
                .split(',')
                .collect();
            let layouts: Vec<_> = keyboard
                .get("[Layout]", "LayoutList")
                .unwrap_or("")
                .split(',')
                .filter(|s| !s.is_empty())
                .take(4)
                .enumerate()
                .map(|(index, code)| crate::KeyboardLayout {
                    layout: code.into(),
                    variant: variants.get(index).unwrap_or(&"").to_string(),
                    label: names
                        .get(index)
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| code.to_uppercase()),
                })
                .collect();
            if !layouts.is_empty() {
                layout.keyboard.layouts = layouts;
            }
            if let Some(option) = keyboard
                .get("[Layout]", "Options")
                .and_then(|s| s.split(',').find(|s| s.starts_with("grp:")))
            {
                layout.keyboard.switch_option = option.into();
            }
        }
        let input = read_ini(config_dir.join("kcminputrc"));
        if let Some(delay) = input
            .get("[Keyboard]", "RepeatDelay")
            .and_then(|s| s.parse::<u32>().ok())
        {
            layout.keyboard.repeat_delay = delay.clamp(100, 2000);
        }
        if let Some(rate) = input
            .get("[Keyboard]", "RepeatRate")
            .and_then(|s| s.parse::<u32>().ok())
        {
            layout.keyboard.repeat_rate = rate.min(100);
        }
        layout.services.maximum_volume = if read_ini(config_dir.join("plasmaparc"))
            .get("[General]", "RaiseMaximumVolume")
            == Some("true")
        {
            150
        } else {
            100
        };
        import_appearance(config_dir, &kdeglobals, &mut theme);
        let input = read_ini(config_dir.join("kcminputrc"));
        if let Some(name) = input.get("[Mouse]", "cursorTheme") {
            theme.cursor_theme = name.to_owned();
        }
        theme.cursor_size = input
            .get("[Mouse]", "cursorSize")
            .and_then(|s| s.parse().ok())
            .unwrap_or(24);

        if let Some(margins) = rix_assets::resolve(&theme.appearance.menu_frame)
            .and_then(|path| rix_assets::frame_margins(&path).ok())
        {
            layout.launcher.width =
                (layout.launcher.width + (margins[0] + margins[2]).round() as u32).min(1200);
            layout.launcher.height =
                (layout.launcher.height + (margins[1] + margins[3]).round() as u32).min(1000);
        }
        if let Some(rgb) = kdeglobals.get("[Colors:Selection]", "BackgroundNormal") {
            let components: Vec<_> = rgb
                .split(',')
                .take(3)
                .filter_map(|value| value.parse::<u8>().ok())
                .collect();
            if let [red, green, blue] = components.as_slice() {
                theme.accent_color = format!("#{red:02x}{green:02x}{blue:02x}");
            }
        }

        Ok(Self {
            layout,
            theme,
            pinned_apps,
        })
    }
}

fn read_ini(path: impl AsRef<Path>) -> Ini {
    fs::read_to_string(path)
        .map(|text| Ini::parse(&text))
        .unwrap_or_default()
}
fn data_file(relative: &str) -> Option<std::path::PathBuf> {
    let user = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/share"))
        });
    let system =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    user.into_iter()
        .chain(system.split(':').map(std::path::PathBuf::from))
        .map(|root| root.join(relative))
        .find(|path| path.is_file())
}
fn import_appearance(config_dir: &Path, globals: &Ini, theme: &mut ThemeSettings) {
    let defaults = globals
        .get("[KDE]", "LookAndFeelPackage")
        .and_then(|name| data_file(&format!("plasma/look-and-feel/{name}/contents/defaults")))
        .map(read_ini)
        .unwrap_or_default();
    let plasma = read_ini(config_dir.join("plasmarc"));
    let kwin = read_ini(config_dir.join("kwinrc"));

    let mut remaining = kwin
        .get("[Effect-blur]", "BlurStrength")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(15)
        .clamp(1, 15);
    theme.effects = Default::default();
    for (iterations, steps, start, end) in [
        (1, 2, 1., 2.),
        (2, 2, 2., 3.),
        (3, 5, 2., 5.),
        (4, 6, 3., 8.),
    ] {
        if remaining <= steps {
            theme.effects.iterations = iterations;
            theme.effects.offset = start + (end - start) * remaining as f32 / steps as f32;
            break;
        }
        remaining -= steps;
    }
    theme.blur_radius = if kwin.get("[Plugins]", "blurEnabled") == Some("false") {
        0
    } else {
        ThemeSettings::plasma_glass().blur_radius
    };
    theme.noise_opacity = kwin
        .get("[Effect-blur]", "NoiseStrength")
        .and_then(|v| v.parse::<u32>().ok())
        .map(|v| v.min(127) as f32 / 255.)
        .unwrap_or_else(|| ThemeSettings::plasma_glass().noise_opacity);

    if let Some(name) = plasma
        .get("[Theme]", "name")
        .or_else(|| defaults.get("[plasmarc][Theme]", "name"))
    {
        theme.name = name.into();
        theme.appearance.panel_frame = format!("plasma:{name}/widgets/panel-background.svgz");
        theme.appearance.menu_frame = format!("plasma:{name}/dialogs/background.svgz");
        theme.appearance.task_active_frame =
            format!("plasma:{name}/widgets/tasks.svgz#south-focus-");
        theme.appearance.task_hover_frame =
            format!("plasma:{name}/widgets/tasks.svgz#south-hover-");

        theme.opacity = 1.;
    }
    if let Some(name) = globals
        .get("[Icons]", "Theme")
        .or_else(|| defaults.get("[kdeglobals][Icons]", "Theme"))
    {
        theme.icon_theme = name.into();
    }
    if let Some(name) = kwin
        .get("[org.kde.kdecoration2]", "theme")
        .or_else(|| defaults.get("[kwinrc][org.kde.kdecoration2]", "theme"))
        .and_then(|v| v.strip_prefix("__aurorae__svg__"))
    {
        theme.appearance.decoration_theme = format!("aurorae:{name}");
        if let Some(file) = data_file(&format!("aurorae/themes/{name}/{name}rc")) {
            let rc = read_ini(file);
            for (key, token) in [
                ("BorderLeft", "decoration_border_left"),
                ("BorderRight", "decoration_border_right"),
                ("BorderBottom", "decoration_border_bottom"),
                ("ButtonWidth", "decoration_button_size"),
                ("ButtonSpacing", "decoration_button_spacing"),
                ("TitleBorderLeft", "decoration_title_padding"),
            ] {
                if let Some(value) = rc.get("[Layout]", key).and_then(|v| v.parse::<f32>().ok()) {
                    theme.appearance.metrics.insert(token.into(), value);
                }
            }
        }
    }
    if let Some(font) = globals.get("[General]", "font") {
        let mut fields = font.split(',');
        if let Some(family) = fields.next() {
            theme.appearance.font_family = family.into();
        }
        if let Some(points) = fields.next().and_then(|v| v.parse::<f32>().ok()) {
            theme
                .appearance
                .metrics
                .insert("font_size_md".into(), points * 96. / 72.);
        }
    }
    for (token, section, key) in [
        ("background", "[Colors:Window]", "BackgroundNormal"),
        ("menu_bg", "[Colors:Window]", "BackgroundNormal"),
        ("text_primary", "[Colors:Window]", "ForegroundNormal"),
        ("text_secondary", "[Colors:Window]", "ForegroundInactive"),
    ] {
        if let Some(value) = globals.get(section, key) {
            let values: Vec<_> = value
                .split(',')
                .take(3)
                .filter_map(|v| v.parse::<u8>().ok())
                .collect();
            if let [r, g, b] = values.as_slice() {
                theme
                    .appearance
                    .palette
                    .insert(token.into(), format!("#{r:02x}{g:02x}{b:02x}"));
            }
        }
    }
}

fn resolve_desktop_id(entry: &str, kdeglobals: &Ini, preferred: &Ini) -> Option<String> {
    let entry = entry.trim();
    let raw = if entry == "preferred://browser" {
        kdeglobals
            .get("[General]", "BrowserApplication")
            .or_else(|| preferred.get("[Default Applications]", "x-scheme-handler/https"))?
    } else if entry == "preferred://filemanager" {
        preferred.get("[Default Applications]", "inode/directory")?
    } else if entry.starts_with("preferred://") {
        return None;
    } else {
        entry.strip_prefix("applications:").unwrap_or(entry)
    };
    let raw = raw.split(';').find(|id| !id.is_empty())?;
    let id = Path::new(raw)
        .file_name()
        .and_then(|name| name.to_str())?
        .strip_suffix(".desktop")
        .unwrap_or(raw);
    (!id.is_empty()).then(|| id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_current_panel_shape_and_favorites() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("plasmashellrc"), "[PlasmaViews][Panel 54]\nfloating=1\n[PlasmaViews][Panel 54][Defaults]\nthickness=46\n").unwrap();
        fs::write(dir.path().join("plasma-org.kde.plasma.desktop-appletsrc"), "[Containments][54]\nplugin=org.kde.panel\nlocation=4\n[Containments][54][General]\nAppletOrder=79;55;57;78;59;73;74\n[Containments][54][Applets][79]\nplugin=org.kde.plasma.panelspacer\n[Containments][54][Applets][55]\nplugin=org.kde.plasma.kickoff\n[Containments][54][Applets][55][Configuration]\npopupWidth=643\npopupHeight=529\n[Containments][54][Applets][55][Configuration][General]\nicon=start-here\n[Containments][54][Applets][57]\nplugin=org.kde.plasma.icontasks\n[Containments][54][Applets][57][Configuration][General]\nlaunchers=applications:systemsettings.desktop,preferred://filemanager\n[Containments][54][Applets][78]\nplugin=org.kde.plasma.panelspacer\n[Containments][54][Applets][59]\nplugin=org.kde.plasma.systemtray\n[Containments][54][Applets][73]\nplugin=org.kde.plasma.digitalclock\n[Containments][54][Applets][74]\nplugin=org.kde.plasma.showdesktop\n").unwrap();
        fs::write(
            dir.path().join("kde-mimeapps.list"),
            "[Default Applications]\ninode/directory=org.example.Files.desktop;\n",
        )
        .unwrap();
        fs::write(dir.path().join("kactivitymanagerd-statsrc"), "[Favorites-org.kde.plasma.kickoff.favorites.instance-55-global]\nordering=applications:org.kde.discover.desktop,preferred://browser\n").unwrap();
        fs::write(dir.path().join("kdeglobals"), "[General]\nBrowserApplication=org.example.Browser.desktop\n[Colors:Selection]\nBackgroundNormal=0,156,255,140\n").unwrap();

        let frame = dir.path().join("frame.svg");
        fs::write(&frame, "<svg xmlns='http://www.w3.org/2000/svg' width='100' height='100' viewBox='0 0 50 50'><rect id='hint-left-margin' width='5' height='1'/><rect id='hint-right-margin' width='5' height='1'/><rect id='hint-top-margin' width='1' height='7'/><rect id='hint-bottom-margin' width='1' height='7'/></svg>").unwrap();
        let mut theme = ThemeSettings::default();
        theme.appearance.menu_frame = frame.to_string_lossy().into();
        let snapshot = PlasmaSnapshot::read_from(dir.path(), theme).unwrap();
        assert_eq!(snapshot.layout.panels[0].thickness, 46);
        assert_eq!(
            snapshot.layout.panels[0].widgets[0].section,
            WidgetSection::Center
        );
        assert_eq!(
            snapshot.layout.panels[0].widgets[2].section,
            WidgetSection::End
        );
        assert_eq!(snapshot.layout.launcher.width, 663);
        assert_eq!(snapshot.layout.launcher.height, 557);
        assert_eq!(
            snapshot.layout.launcher.favorites,
            ["org.kde.discover", "org.example.Browser"]
        );
        assert_eq!(
            snapshot.pinned_apps,
            ["systemsettings", "org.example.Files"]
        );
        assert_eq!(snapshot.theme.accent_color, "#009cff");
    }
}
