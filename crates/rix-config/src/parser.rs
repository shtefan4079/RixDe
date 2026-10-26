use crate::model::*;
use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};
use rix_core::{Alignment, AutoHide, PanelLength, Position};
use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("KDL parse error: {0}")]
    Kdl(#[from] kdl::KdlError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid configuration value: {0}")]
    InvalidValue(String),
    #[error("Settings changed in another client; reopen this page and retry: {0}")]
    Conflict(String),
}

pub fn layout_schema_version(input: &str) -> Result<u32, ConfigError> {
    let doc: KdlDocument = input.parse()?;
    Ok(read_schema_version(&doc))
}

fn read_schema_version(doc: &KdlDocument) -> u32 {
    doc.nodes()
        .iter()
        .find(|node| node.name().value() == "schema-version")
        .and_then(|node| node.get(0))
        .and_then(|entry| entry.value().as_i64())
        .filter(|value| *value >= 0)
        .map_or(0, |value| value as u32)
}

pub fn parse_layout_kdl(input: &str) -> Result<LayoutConfig, ConfigError> {
    let doc: KdlDocument = input.parse()?;
    let declared = read_schema_version(&doc);
    if SchemaVersion::resolve(declared).is_future() {
        return Err(ConfigError::InvalidValue(format!(
            "layout.kdl uses schema {declared}, newer than this build (schema {CURRENT_SCHEMA_VERSION})"
        )));
    }
    let schema_version = migrate_schema_version(declared).ok_or_else(|| {
        ConfigError::InvalidValue(format!(
            "layout.kdl uses schema {declared} without a migration path"
        ))
    })?;
    let mut panels = Vec::new();
    let mut launcher = LauncherConfig::default();
    let mut keyboard = KeyboardConfig::default();
    let mut services = ServiceSettings::default();

    for node in doc.nodes() {
        match node.name().value() {
            "panel" => panels.push(parse_panel_node(node)?),
            "launcher" => launcher = parse_launcher_node(node),
            "keyboard" => {
                keyboard.switch_option = node
                    .get("switch")
                    .and_then(|e| e.value().as_string())
                    .unwrap_or("grp:alt_shift_toggle")
                    .into();
                keyboard.repeat_rate = node
                    .get("repeat-rate")
                    .and_then(|e| e.value().as_i64())
                    .unwrap_or(30) as u32;
                keyboard.repeat_delay = node
                    .get("repeat-delay")
                    .and_then(|e| e.value().as_i64())
                    .unwrap_or(500) as u32;
                if let Some(children) = node.children() {
                    keyboard.layouts = children
                        .nodes()
                        .iter()
                        .filter(|n| n.name().value() == "layout")
                        .map(|n| KeyboardLayout {
                            layout: n
                                .get(0)
                                .and_then(|e| e.value().as_string())
                                .unwrap_or("")
                                .into(),
                            variant: n
                                .get("variant")
                                .and_then(|e| e.value().as_string())
                                .unwrap_or("")
                                .into(),
                            label: n
                                .get("label")
                                .and_then(|e| e.value().as_string())
                                .unwrap_or("")
                                .into(),
                        })
                        .collect();
                }
                keyboard.validate()?;
            }
            "services" => {
                services.maximum_volume = node
                    .get("maximum-volume")
                    .and_then(|e| e.value().as_i64())
                    .unwrap_or(100) as u32;
                services.previews = node
                    .get("previews")
                    .and_then(|e| e.value().as_bool())
                    .unwrap_or(true);
                services.week_start = node
                    .get("week-start")
                    .and_then(|e| e.value().as_i64())
                    .unwrap_or(0) as u32;
                if let Some(children) = node.children() {
                    services.tray_modules = children
                        .nodes()
                        .iter()
                        .filter(|n| n.name().value() == "tray")
                        .filter_map(|n| {
                            n.get(0)
                                .and_then(|e| e.value().as_string())
                                .map(str::to_owned)
                        })
                        .collect();
                }
                services.validate()?;
            }
            _ => {}
        }
    }

    if panels.is_empty() {
        panels = LayoutConfig::default_layout().panels;
    }
    let layout = LayoutConfig {
        schema_version,
        panels,
        launcher,
        keyboard,
        services,
    };
    crate::panels::validate_panels(&layout)?;
    Ok(layout)
}

fn parse_launcher_node(node: &KdlNode) -> LauncherConfig {
    let mut launcher = LauncherConfig::default();
    for entry in node.entries() {
        let Some(key) = entry.name() else { continue };
        match key.value() {
            "width" => {
                if let Some(value) = entry.value().as_i64() {
                    launcher.width = value.clamp(360, 1200) as u32;
                }
            }
            "height" => {
                if let Some(value) = entry.value().as_i64() {
                    launcher.height = value.clamp(360, 1000) as u32;
                }
            }
            "icon" => {
                if let Some(value) = entry.value().as_string() {
                    launcher.icon = value.to_owned();
                }
            }
            "alignment" => {
                launcher.alignment = match entry.value().as_string() {
                    Some("start") => Alignment::Start,
                    Some("end") => Alignment::End,
                    _ => Alignment::Center,
                };
            }
            _ => {}
        }
    }
    if let Some(children) = node.children() {
        if let Some(footer) = children
            .nodes()
            .iter()
            .find(|node| node.name().value() == "footer")
        {
            launcher.footer = footer
                .children()
                .map(|children| {
                    children
                        .nodes()
                        .iter()
                        .filter_map(|node| {
                            if node.name().value() != "action" {
                                return None;
                            }
                            let action = node.get(0)?.value().as_string()?.to_owned();
                            let string = |key, default: &str| {
                                node.get(key)
                                    .and_then(|v| v.value().as_string())
                                    .unwrap_or(default)
                                    .to_owned()
                            };
                            let boolean = |key| {
                                node.get(key)
                                    .and_then(|v| v.value().as_bool())
                                    .unwrap_or(false)
                            };
                            Some(crate::model::LauncherAction {
                                label: string("label", &action),
                                icon: string("icon", ""),
                                end: boolean("end"),
                                icon_only: boolean("icon-only"),
                                visible: node
                                    .get("visible")
                                    .and_then(|v| v.value().as_bool())
                                    .unwrap_or(true),
                                action,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
        }
        launcher.favorites = children
            .nodes()
            .iter()
            .filter(|child| child.name().value() == "favorite")
            .filter_map(|child| child.entries().first()?.value().as_string())
            .map(ToOwned::to_owned)
            .take(24)
            .collect();
    }
    launcher
}

fn parse_panel_node(node: &KdlNode) -> Result<PanelConfig, ConfigError> {
    let mut panel = PanelConfig::default();

    for entry in node.entries() {
        if let Some(key) = entry.name() {
            match key.value() {
                "id" => {
                    if let Some(s) = entry.value().as_string() {
                        panel.id = s.to_string();
                    }
                }
                "screen" => {
                    if let Some(s) = entry.value().as_string() {
                        panel.screen = s.to_string();
                    }
                }
                "position" => {
                    if let Some(s) = entry.value().as_string() {
                        panel.position = match s.to_lowercase().as_str() {
                            "top" => Position::Top,
                            "bottom" => Position::Bottom,
                            "left" => Position::Left,
                            "right" => Position::Right,
                            _ => Position::Bottom,
                        };
                    }
                }
                "thickness" => {
                    if let Some(v) = entry.value().as_i64() {
                        panel.thickness = v.clamp(16, 200) as u32;
                    }
                }
                "floating" => {
                    if let Some(b) = entry.value().as_bool() {
                        panel.floating = b;
                    }
                }
                "floating-margin" | "floating_margin" => {
                    if let Some(v) = entry.value().as_i64() {
                        panel.floating_margin = v.max(0) as u32;
                    }
                }
                "opacity" => {
                    if let Some(value) = entry
                        .value()
                        .as_f64()
                        .or_else(|| entry.value().as_i64().map(|v| v as f64))
                    {
                        panel.opacity = value.clamp(0., 1.) as f32;
                    }
                }
                "use-theme-frame" => {
                    if let Some(value) = entry.value().as_bool() {
                        panel.use_theme_frame = value;
                    }
                }
                "corner-radius" | "corner_radius" => {
                    if let Some(v) = entry.value().as_i64() {
                        panel.corner_radius = v.max(0) as u32;
                    }
                }
                _ => {}
            }
        }
    }

    if let Some(children) = node.children() {
        panel.widgets.clear();
        for child in children.nodes() {
            match child.name().value() {
                "alignment" => {
                    if let Some(entry) = child.entries().first() {
                        if let Some(s) = entry.value().as_string() {
                            panel.alignment = match s.to_lowercase().as_str() {
                                "start" | "left" => Alignment::Start,
                                "center" => Alignment::Center,
                                "end" | "right" => Alignment::End,
                                "justify" => Alignment::Justify,
                                _ => Alignment::Center,
                            };
                        }
                    }
                }
                "exclusive-zone" | "exclusive_zone" => {
                    if let Some(entry) = child.entries().first() {
                        if let Some(b) = entry.value().as_bool() {
                            panel.exclusive_zone = b;
                        }
                    }
                }
                "autohide" => {
                    if let Some(entry) = child.entries().first() {
                        if let Some(s) = entry.value().as_string() {
                            panel.autohide = match s.to_lowercase().as_str() {
                                "always" => AutoHide::Always,
                                "dodge-windows" | "dodge" => AutoHide::DodgeWindows,
                                _ => AutoHide::None,
                            };
                        }
                    }
                }
                "length" => {
                    if let Some(entry) = child.entries().first() {
                        if let Some(s) = entry.value().as_string() {
                            if s == "fill" {
                                panel.length = PanelLength::Fill;
                            } else if s == "fit-content" {
                                panel.length = PanelLength::FitContent;
                            }
                        } else if let Some(v) = entry.value().as_i64() {
                            panel.length = PanelLength::Fixed(v.max(100) as u32);
                        }
                    }
                }
                "section" => {
                    let section = if let Some(entry) = child.entries().first() {
                        if let Some(s) = entry.value().as_string() {
                            WidgetSection::from_str_loose(s)
                        } else {
                            WidgetSection::Start
                        }
                    } else {
                        WidgetSection::Start
                    };

                    if let Some(widget_children) = child.children() {
                        for w_node in widget_children.nodes() {
                            if w_node.name().value() == "widget" {
                                panel.widgets.push(parse_widget_node(w_node, section)?);
                            }
                        }
                    }
                }
                "widget" => {

                    panel
                        .widgets
                        .push(parse_widget_node(child, WidgetSection::Start)?);
                }
                _ => {}
            }
        }
    }

    Ok(panel)
}

fn parse_widget_node(node: &KdlNode, section: WidgetSection) -> Result<WidgetConfig, ConfigError> {
    let mut kind = WidgetKind::Spacer;
    let mut id = String::new();
    let mut properties = HashMap::new();

    let entries = node.entries();
    if let Some(first) = entries.first() {
        if first.name().is_none() {
            if let Some(s) = first.value().as_string() {
                kind = WidgetKind::from_name(s);
            }
        }
    }

    for entry in entries {
        if let Some(key) = entry.name() {
            match key.value() {
                "id" => {
                    if let Some(s) = entry.value().as_string() {
                        id = s.to_string();
                    }
                }
                "kind" | "type" => {
                    if let Some(s) = entry.value().as_string() {
                        kind = WidgetKind::from_name(s);
                    }
                }
                prop_key => {
                    let json_val = if let Some(s) = entry.value().as_string() {
                        serde_json::Value::String(s.to_string())
                    } else if let Some(i) = entry.value().as_i64() {
                        serde_json::Value::Number(i.into())
                    } else if let Some(f) = entry.value().as_f64() {
                        serde_json::Number::from_f64(f)
                            .map(serde_json::Value::Number)
                            .unwrap_or(serde_json::Value::Null)
                    } else if let Some(b) = entry.value().as_bool() {
                        serde_json::Value::Bool(b)
                    } else {
                        serde_json::Value::Null
                    };
                    properties.insert(prop_key.to_string(), json_val);
                }
            }
        }
    }

    if id.is_empty() {
        id = format!("w-{}-{}", kind.as_str(), fastrand_suffix());
    }

    Ok(WidgetConfig {
        id,
        kind,
        section,
        properties,
    })
}

fn fastrand_suffix() -> u16 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(42);
    (nanos % 10000) as u16
}

pub fn format_layout_kdl(config: &LayoutConfig) -> String {
    fn quoted(value: &str) -> String {
        KdlValue::String(value.to_owned()).to_string()
    }
    let mut output = String::new();
    output.push_str("// RixDe Desktop Environment Layout Configuration\n");
    output.push_str("// Auto-generated & live-synced with KDE-style Edit Mode\n\n");

    output.push_str(&format!("schema-version {}\n\n", config.schema_version));

    output.push_str(&format!(
        "keyboard switch={} repeat-rate={} repeat-delay={} {{\n",
        quoted(&config.keyboard.switch_option),
        config.keyboard.repeat_rate,
        config.keyboard.repeat_delay
    ));
    for layout in &config.keyboard.layouts {
        output.push_str(&format!(
            "    layout {} variant={} label={}\n",
            quoted(&layout.layout),
            quoted(&layout.variant),
            quoted(&layout.label)
        ));
    }
    output.push_str("}\n\n");
    output.push_str(&format!(
        "services maximum-volume={} previews={} week-start={} {{\n",
        config.services.maximum_volume, config.services.previews, config.services.week_start
    ));
    for module in &config.services.tray_modules {
        output.push_str(&format!("    tray {}\n", quoted(module)));
    }
    output.push_str("}\n\n");
    for panel in &config.panels {
        let pos_str = match panel.position {
            Position::Bottom => "bottom",
            Position::Top => "top",
            Position::Left => "left",
            Position::Right => "right",
        };

        output.push_str(&format!(
            "panel id={} screen={} position=\"{}\" thickness={} floating={} floating-margin={} corner-radius={} use-theme-frame={} opacity={} {{\n",
            quoted(&panel.id),
            quoted(&panel.screen),
            pos_str,
            panel.thickness,
            panel.floating,
            panel.floating_margin,
            panel.corner_radius,
            panel.use_theme_frame,
            panel.opacity
        ));

        let align_str = match panel.alignment {
            Alignment::Start => "start",
            Alignment::Center => "center",
            Alignment::End => "end",
            Alignment::Justify => "justify",
        };
        output.push_str(&format!("    alignment \"{}\"\n", align_str));
        output.push_str(&format!("    exclusive-zone {}\n", panel.exclusive_zone));

        let autohide_str = match panel.autohide {
            AutoHide::None => "none",
            AutoHide::Always => "always",
            AutoHide::DodgeWindows => "dodge-windows",
        };
        output.push_str(&format!("    autohide \"{}\"\n", autohide_str));

        match panel.length {
            PanelLength::Fill => output.push_str("    length \"fill\"\n"),
            PanelLength::FitContent => output.push_str("    length \"fit-content\"\n"),
            PanelLength::Fixed(px) => output.push_str(&format!("    length {}\n", px)),
        }

        output.push('\n');

        for section in [
            WidgetSection::Start,
            WidgetSection::Center,
            WidgetSection::End,
        ] {
            let section_widgets: Vec<_> = panel
                .widgets
                .iter()
                .filter(|w| w.section == section)
                .collect();
            if !section_widgets.is_empty() {
                output.push_str(&format!("    section \"{}\" {{\n", section.as_str()));
                for w in section_widgets {
                    output.push_str(&format!(
                        "        widget {} id={}",
                        quoted(w.kind.as_str()),
                        quoted(&w.id)
                    ));
                    let mut properties: Vec<_> = w.properties.iter().collect();
                    properties.sort_by_key(|(key, _)| *key);
                    for (k, v) in properties {
                        match v {
                            serde_json::Value::String(s) => output.push_str(&format!(
                                " {}",
                                KdlEntry::new_prop(k.as_str(), s.clone())
                            )),
                            serde_json::Value::Bool(b) => {
                                output.push_str(&format!(" {}", KdlEntry::new_prop(k.as_str(), *b)))
                            }
                            serde_json::Value::Number(n) => {
                                if let Some(i) = n.as_i64() {
                                    output.push_str(&format!(
                                        " {}",
                                        KdlEntry::new_prop(k.as_str(), i)
                                    ));
                                } else if let Some(u) = n.as_u64() {
                                    output.push_str(&format!(
                                        " {}",
                                        KdlEntry::new_prop(k.as_str(), u as f64)
                                    ));
                                } else if let Some(f) = n.as_f64() {
                                    output.push_str(&format!(
                                        " {}",
                                        KdlEntry::new_prop(k.as_str(), f)
                                    ));
                                }
                            }
                            _ => {}
                        }
                    }
                    output.push('\n');
                }
                output.push_str("    }\n");
            }
        }

        output.push_str("}\n\n");
    }

    output.push_str(&format!(
        "launcher width={} height={} icon={} alignment={} {{\n",
        config.launcher.width,
        config.launcher.height,
        quoted(&config.launcher.icon),
        quoted(match config.launcher.alignment {
            Alignment::Start => "start",
            Alignment::End => "end",
            _ => "center",
        })
    ));
    for favorite in &config.launcher.favorites {
        output.push_str(&format!("    favorite {}\n", quoted(favorite)));
    }
    output.push_str("    footer {\n");
    for action in &config.launcher.footer {
        output.push_str(&format!(
            "        action {} label={} icon={} end={} icon-only={} visible={}\n",
            quoted(&action.action),
            quoted(&action.label),
            quoted(&action.icon),
            action.end,
            action.icon_only,
            action.visible
        ));
    }
    output.push_str("    }\n");
    output.push_str("}\n");

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_roundtrip_escapes_user_values_and_is_stable() {
        let mut layout = LayoutConfig::default_layout();
        let panel = &mut layout.panels[0];
        panel.id = "panel \"one\"\\north".into();
        panel.screen = "display\nprimary".into();
        let widget = &mut panel.widgets[0];
        widget.id = "clock \"special\"".into();
        widget.properties.insert(
            "label with space".into(),
            serde_json::Value::String("line 1\nline \"2\"\\".into()),
        );
        layout.launcher.footer[0].label = "Menu \"custom\"\\title".into();
        layout.launcher.footer[1].visible = false;
        layout.launcher.footer[2].icon_only = true;
        let formatted = format_layout_kdl(&layout);
        assert_eq!(parse_layout_kdl(&formatted).unwrap(), layout);
        assert_eq!(format_layout_kdl(&layout), formatted);
        layout.launcher.footer.clear();
        assert_eq!(
            parse_layout_kdl(&format_layout_kdl(&layout)).unwrap(),
            layout
        );
    }

    #[test]
    fn layout_schema_header_roundtrips_and_future_revisions_are_rejected() {
        let layout = LayoutConfig::default_layout();
        let formatted = format_layout_kdl(&layout);
        assert!(formatted.contains("schema-version 1"));
        assert_eq!(
            layout_schema_version(&formatted).unwrap(),
            CURRENT_SCHEMA_VERSION
        );

        let legacy = formatted.replace("schema-version 1\n\n", "");
        assert_eq!(layout_schema_version(&legacy).unwrap(), 0);
        assert_eq!(
            parse_layout_kdl(&legacy).unwrap().schema_version,
            CURRENT_SCHEMA_VERSION
        );

        let future = formatted.replace("schema-version 1", "schema-version 99");
        assert_eq!(layout_schema_version(&future).unwrap(), 99);
        assert!(parse_layout_kdl(&future).is_err());
    }
}
