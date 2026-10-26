use crate::{ConfigError, LayoutConfig};
use serde_json::Value;

pub fn merge_layout(
    base: &LayoutConfig,
    updated: &LayoutConfig,
    current: &LayoutConfig,
) -> Result<LayoutConfig, ConfigError> {
    merge_document(base, updated, current)
}

pub fn merge_document<T: serde::Serialize + serde::de::DeserializeOwned>(
    base: &T,
    updated: &T,
    current: &T,
) -> Result<T, ConfigError> {
    let encode =
        |layout| serde_json::to_value(layout).map_err(|e| ConfigError::InvalidValue(e.to_string()));
    let mut conflicts = Vec::new();
    let result = merge(
        Some(&encode(base)?),
        Some(&encode(updated)?),
        Some(&encode(current)?),
        "",
        &mut conflicts,
    );
    if !conflicts.is_empty() {
        return Err(ConfigError::Conflict(conflicts.join(", ")));
    }
    serde_json::from_value(result.unwrap()).map_err(|e| ConfigError::InvalidValue(e.to_string()))
}

fn merge(
    base: Option<&Value>,
    updated: Option<&Value>,
    current: Option<&Value>,
    path: &str,
    conflicts: &mut Vec<String>,
) -> Option<Value> {
    if updated == base {
        return current.cloned();
    }
    if current == base || current == updated {
        return updated.cloned();
    }
    if let (Some(Value::Object(base)), Some(Value::Object(updated)), Some(Value::Object(current))) =
        (base, updated, current)
    {
        let keys: std::collections::BTreeSet<_> = base
            .keys()
            .chain(updated.keys())
            .chain(current.keys())
            .collect();
        let result = keys
            .into_iter()
            .filter_map(|key| {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                merge(
                    base.get(key),
                    updated.get(key),
                    current.get(key),
                    &format!("{path}/{escaped}"),
                    conflicts,
                )
                .map(|value| (key.clone(), value))
            })
            .collect();
        return Some(Value::Object(result));
    }
    if let (Some(Value::Array(base)), Some(Value::Array(updated)), Some(Value::Array(current))) =
        (base, updated, current)
    {

        let segments: Vec<_> = path.split('/').collect();
        let instances = path == "/panels"
            || (segments.len() == 4 && segments[1] == "panels" && segments[3] == "widgets");
        if instances {
            if let (Some(base_ids), Some(updated_ids), Some(current_ids)) =
                (ids(base), ids(updated), ids(current))
            {
                let order = if updated_ids == base_ids {
                    &current_ids
                } else if current_ids == base_ids || current_ids == updated_ids {
                    &updated_ids
                } else {
                    conflicts.push(format!("{path}/@order"));
                    return Some(Value::Array(current.clone()));
                };
                let all: std::collections::BTreeSet<_> = base_ids
                    .iter()
                    .chain(updated_ids.iter())
                    .chain(current_ids.iter())
                    .copied()
                    .collect();
                let merged: std::collections::BTreeMap<_, _> = all
                    .into_iter()
                    .map(|id| {
                        (
                            id,
                            merge(
                                item(base, id),
                                item(updated, id),
                                item(current, id),
                                &format!("{path}/{}", id.replace('~', "~0").replace('/', "~1")),
                                conflicts,
                            ),
                        )
                    })
                    .collect();
                let result = order
                    .iter()
                    .filter_map(|id| merged.get(id).cloned().flatten())
                    .collect();
                return Some(Value::Array(result));
            }
        }
    }
    conflicts.push(path.to_string());
    current.cloned()
}

fn item<'a>(values: &'a [Value], id: &str) -> Option<&'a Value> {
    values
        .iter()
        .find(|v| v.get("id").and_then(Value::as_str) == Some(id))
}

fn ids(values: &[Value]) -> Option<Vec<&str>> {
    let ids: Vec<_> = values
        .iter()
        .map(|value| value.get("id")?.as_str())
        .collect::<Option<_>>()?;
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    (unique.len() == ids.len()).then_some(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unrelated_clients_merge_fields_and_instances() {
        let mut base = LayoutConfig::default_layout();
        let mut second = base.panels[0].clone();
        second.id = "second".into();
        base.panels.push(second);
        let mut current = base.clone();
        current.panels[0].thickness = 54;
        current.panels[1].opacity = 0.5;
        current.launcher.favorites.push("example.desktop".into());
        let mut updated = base.clone();
        updated.panels[0].floating_margin = 12;
        updated.panels[1].thickness = 60;
        updated.keyboard.repeat_delay = 600;
        let merged = merge_layout(&base, &updated, &current).unwrap();
        assert_eq!(merged.panels[0].thickness, 54);
        assert_eq!(merged.panels[0].floating_margin, 12);
        assert_eq!(merged.panels[1].opacity, 0.5);
        assert_eq!(merged.panels[1].thickness, 60);
        assert_eq!(merged.keyboard.repeat_delay, 600);
        assert_eq!(merged.launcher.favorites, current.launcher.favorites);
    }

    #[test]
    fn conflicting_field_and_deleted_instance_are_rejected() {
        let base = LayoutConfig::default_layout();
        let mut updated = base.clone();
        updated.panels[0].thickness = 60;
        let mut current = base.clone();
        current.panels[0].thickness = 54;
        assert!(
            matches!(merge_layout(&base, &updated, &current), Err(ConfigError::Conflict(path)) if path.contains("thickness"))
        );
        current.panels.clear();
        assert!(matches!(
            merge_layout(&base, &updated, &current),
            Err(ConfigError::Conflict(_))
        ));
    }

    #[test]
    fn opaque_properties_and_reordering_preserve_concurrent_fields() {
        let base = LayoutConfig::default_layout();
        let mut current = base.clone();
        current.panels[0].widgets[0]
            .properties
            .insert("opaque".into(), serde_json::json!({"nested": [1, 2]}));
        let mut updated = base.clone();
        updated.panels[0].widgets.reverse();
        let merged = merge_layout(&base, &updated, &current).unwrap();
        assert_eq!(
            merged.panels[0].widgets.last().unwrap().properties["opaque"],
            serde_json::json!({"nested": [1, 2]})
        );
        assert_eq!(
            merged.panels[0].widgets[0].id,
            updated.panels[0].widgets[0].id
        );
    }

    #[test]
    fn theme_and_desktop_edits_preserve_other_domains() {
        let base = crate::ThemeSettings::default();
        let mut a = base.clone();
        a.opacity = 0.8;
        let mut b = base.clone();
        b.cursor_size = 40;
        let merged: crate::ThemeSettings = merge_document(&base, &a, &b).unwrap();
        assert_eq!(merged.opacity, 0.8);
        assert_eq!(merged.cursor_size, 40);
        let base = crate::DesktopSettings::default();
        let mut a = base.clone();
        a.language = "en".into();
        let mut b = base.clone();
        b.wallpaper.blur = 2.0;
        let merged: crate::DesktopSettings = merge_document(&base, &a, &b).unwrap();
        assert_eq!(merged.language, "en");
        assert_eq!(merged.wallpaper.blur, 2.0);
        a.wallpaper.blur = 1.0;
        assert!(matches!(
            merge_document(&base, &a, &b),
            Err(ConfigError::Conflict(_))
        ));
    }

    #[test]
    fn persisted_edits_validate_and_preserve_invalid_external_documents() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::ConfigManager::init_in(dir.path().to_path_buf()).unwrap();
        let base = config.get_desktop();
        let mut current = base.clone();
        current.language = "en".into();
        config.save_desktop(current).unwrap();
        let mut updated = base.clone();
        updated.wallpaper.blur = 2.0;
        let merged = config.edit_desktop(&base, &updated).unwrap();
        let file = dir.path().join("desktop.json");
        let saved: crate::DesktopSettings =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(saved, merged);
        assert_eq!(merged.language, "en");
        assert_eq!(merged.wallpaper.blur, 2.0);
        updated.wallpaper.max_fps = 0;
        assert!(config.edit_desktop(&base, &updated).is_err());
        assert_eq!(
            serde_json::from_slice::<crate::DesktopSettings>(&std::fs::read(&file).unwrap())
                .unwrap(),
            merged
        );
        std::fs::write(&file, "invalid external document").unwrap();
        assert!(config.edit_desktop(&base, &updated).is_err());
        assert_eq!(
            std::fs::read_to_string(file).unwrap(),
            "invalid external document"
        );
    }
}
