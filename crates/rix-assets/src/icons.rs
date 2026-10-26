use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path, PathBuf},
};

#[derive(Default)]
struct Theme {
    inherits: Vec<String>,
    directories: Vec<(String, u32, u32)>,
}
pub struct IconResolver {
    roots: Vec<PathBuf>,
    pixmaps: Vec<PathBuf>,
    themes: HashMap<String, Theme>,
}
impl Default for IconResolver {
    fn default() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let user = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|p| p.join(".local/share")));
        let system = std::env::var_os("XDG_DATA_DIRS")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        let data: Vec<_> = user
            .into_iter()
            .chain(std::env::split_paths(&system))
            .collect();
        Self {
            roots: home
                .into_iter()
                .map(|p| p.join(".icons"))
                .chain(data.iter().map(|p| p.join("icons")))
                .collect(),
            pixmaps: data.iter().map(|p| p.join("pixmaps")).collect(),
            themes: HashMap::new(),
        }
    }
}
fn relative(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}
fn candidate(directory: &Path, name: &str) -> Option<PathBuf> {
    [
        name.to_owned(),
        format!("{name}.svg"),
        format!("{name}.svgz"),
        format!("{name}.png"),
    ]
    .into_iter()
    .map(|name| directory.join(name))
    .find(|p| p.is_file())
}
impl IconResolver {

    pub fn resolve_names(&mut self, names: &[String], theme: &str, size: u32) -> Option<PathBuf> {
        self.resolve_names_with(names, theme, size, |path| Some(path.to_path_buf()))
    }

    pub fn resolve_names_with<T>(
        &mut self,
        names: &[String],
        theme: &str,
        size: u32,
        mut load: impl FnMut(&Path) -> Option<T>,
    ) -> Option<T> {
        let names: Vec<_> = names.iter().filter(|name| relative(name)).collect();
        let mut pending = vec![theme.to_owned()];
        let mut visited = HashSet::new();
        let mut chain = Vec::new();
        while let Some(theme) = pending.pop() {
            if !relative(&theme) || visited.len() >= 64 || !visited.insert(theme.clone()) {
                continue;
            }
            let metadata = self.themes.entry(theme.clone()).or_insert_with(|| {
                self.roots
                    .iter()
                    .find_map(|root| {
                        std::fs::read_to_string(root.join(&theme).join("index.theme")).ok()
                    })
                    .map(|text| parse_index(&text))
                    .unwrap_or_default()
            });
            pending.extend(metadata.inherits.iter().rev().cloned());
            chain.push(theme);
        }
        if !visited.contains("hicolor") {
            chain.push("hicolor".into());
        }
        for theme in chain {
            let metadata = self.themes.entry(theme.clone()).or_insert_with(|| {
                self.roots
                    .iter()
                    .find_map(|root| {
                        std::fs::read_to_string(root.join(&theme).join("index.theme")).ok()
                    })
                    .map(|text| parse_index(&text))
                    .unwrap_or_default()
            });
            let mut directories = metadata.directories.clone();
            directories.sort_by_key(|(_, min, max)| (size > *max, distance(size, *min, *max)));
            for name in &names {
                for (directory, _, _) in &directories {
                    for root in &self.roots {
                        if let Some(path) = candidate(&root.join(&theme).join(directory), name) {
                            if let Some(image) = load(&path) {
                                return Some(image);
                            }
                        }
                    }
                }
            }
        }
        for name in names {
            for root in self.pixmaps.iter().chain(&self.roots) {
                if let Some(path) = candidate(root, name) {
                    if let Some(image) = load(&path) {
                        return Some(image);
                    }
                }
            }
        }
        None
    }

    pub fn resolve(
        &mut self,
        name: &str,
        theme: &str,
        size: u32,
        extra: Option<&Path>,
    ) -> Option<PathBuf> {
        let path = Path::new(name);
        if path.is_absolute() {
            return path.is_file().then(|| path.to_path_buf());
        }
        if !relative(name) {
            return None;
        }
        if let Some(extra) = extra {
            if let Some(path) = candidate(extra, name) {
                return Some(path);
            }

            if let Ok(index) = std::fs::read_to_string(extra.join("index.theme")) {
                let mut directories = parse_index(&index).directories;
                directories.sort_by_key(|(_, min, max)| (size > *max, distance(size, *min, *max)));
                for (dir, _, _) in directories {
                    if let Some(path) = candidate(&extra.join(dir), name) {
                        return Some(path);
                    }
                }
            }
        }
        let mut visited = HashSet::new();
        self.in_theme(name, theme, size, &mut visited)
            .or_else(|| self.in_theme(name, "hicolor", size, &mut visited))
            .or_else(|| self.pixmaps.iter().find_map(|root| candidate(root, name)))
            .or_else(|| self.roots.iter().find_map(|root| candidate(root, name)))
    }
    fn in_theme(
        &mut self,
        name: &str,
        theme: &str,
        size: u32,
        visited: &mut HashSet<String>,
    ) -> Option<PathBuf> {
        if !relative(theme) || visited.len() >= 64 || !visited.insert(theme.to_owned()) {
            return None;
        }
        let metadata = self.themes.entry(theme.to_owned()).or_insert_with(|| {
            self.roots
                .iter()
                .find_map(|root| std::fs::read_to_string(root.join(theme).join("index.theme")).ok())
                .map(|text| parse_index(&text))
                .unwrap_or_default()
        });
        let mut directories = metadata.directories.clone();
        directories.sort_by_key(|(_, min, max)| (size > *max, distance(size, *min, *max)));
        for (dir, _, _) in directories {
            for root in &self.roots {
                if let Some(path) = candidate(&root.join(theme).join(&dir), name) {
                    return Some(path);
                }
            }
        }
        for parent in metadata.inherits.clone() {
            if let Some(path) = self.in_theme(name, &parent, size, visited) {
                return Some(path);
            }
        }
        None
    }
}
fn distance(size: u32, min: u32, max: u32) -> u32 {
    min.saturating_sub(size).max(size.saturating_sub(max))
}
fn parse_index(text: &str) -> Theme {
    let mut groups: HashMap<&str, HashMap<&str, &str>> = HashMap::new();
    let mut section = "";
    for line in text.lines().map(str::trim).filter(|s| !s.starts_with('#')) {
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name;
        } else if let Some((key, value)) = line.split_once('=') {
            groups
                .entry(section)
                .or_default()
                .insert(key.trim(), value.trim());
        }
    }
    let Some(header) = groups.get("Icon Theme") else {
        return Theme::default();
    };
    let inherits = header
        .get("Inherits")
        .into_iter()
        .flat_map(|s| s.split(','))
        .map(str::trim)
        .filter(|s| relative(s))
        .map(str::to_owned)
        .collect();
    let directories = ["Directories", "ScaledDirectories"]
        .iter()
        .filter_map(|key| header.get(key))
        .flat_map(|s| s.split(','))
        .map(str::trim)
        .filter(|s| relative(s))
        .filter_map(|dir| {
            let group = groups.get(dir)?;
            let value = |key, default| {
                group
                    .get(key)
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(default)
            };
            let size = value("Size", 48);
            let scale = value("Scale", 1).max(1);
            let (min, max) = match group.get("Type").copied().unwrap_or("Threshold") {
                "Scalable" => (value("MinSize", size), value("MaxSize", size)),
                "Fixed" => (size, size),
                _ => (
                    size.saturating_sub(value("Threshold", 2)),
                    size.saturating_add(value("Threshold", 2)),
                ),
            };
            Some((
                dir.to_owned(),
                min.saturating_mul(scale),
                max.max(min).saturating_mul(scale),
            ))
        })
        .collect();
    Theme {
        inherits,
        directories,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_fallback_keeps_selected_theme_before_unrelated_specific_icon() {
        let root = tempfile::tempdir().unwrap();
        for theme in ["custom", "hicolor"] {
            let path = root.path().join(theme).join("mimetypes");
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.parent().unwrap().join("index.theme"),
                "[Icon Theme]\nInherits=hicolor\nDirectories=mimetypes\n[mimetypes]\nSize=48\nType=Fixed\n",
            )
            .unwrap();
        }
        let generic = root
            .path()
            .join("custom/mimetypes/application-x-executable.svg");
        std::fs::write(&generic, "custom generic").unwrap();
        std::fs::write(
            root.path()
                .join("hicolor/mimetypes/application-x-ms-dos-executable.svg"),
            "hicolor exact",
        )
        .unwrap();
        let mut resolver = IconResolver {
            roots: vec![root.path().into()],
            pixmaps: vec![],
            themes: HashMap::new(),
        };
        let names = [
            "application-x-ms-dos-executable".into(),
            "application-x-executable".into(),
            "text-x-generic".into(),
        ];
        assert_eq!(
            resolver.resolve_names(&names, "custom", 48),
            Some(generic.clone())
        );
        let bad = root
            .path()
            .join("custom/mimetypes/application-x-ms-dos-executable.svg");
        std::fs::write(&bad, "damaged").unwrap();
        assert_eq!(
            resolver.resolve_names_with(&names, "custom", 48, |path| (path != bad)
                .then(|| path.to_path_buf())),
            Some(generic)
        );
        assert!(resolver
            .resolve_names(&["../secret".into()], "custom", 48)
            .is_none());
    }
    #[test]
    fn avoids_upscaling_when_larger_bitmap_exists() {
        let root = tempfile::tempdir().unwrap();
        let theme = root.path().join("fixture");
        for size in [16, 128] {
            std::fs::create_dir_all(theme.join(size.to_string())).unwrap();
            std::fs::write(theme.join(size.to_string()).join("player.png"), b"fixture").unwrap();
        }
        std::fs::write(theme.join("index.theme"), "[Icon Theme]\nDirectories=16,128\n[16]\nSize=16\nType=Fixed\n[128]\nSize=128\nType=Fixed\n").unwrap();
        let mut resolver = IconResolver {
            roots: vec![root.path().into()],
            pixmaps: vec![],
            themes: HashMap::new(),
        };
        assert_eq!(
            resolver.resolve("player", "fixture", 32, None),
            Some(theme.join("128/player.png"))
        );
    }
    #[test]
    fn inheritance_respects_theme_precedence_and_terminates_cycles() {
        let root = tempfile::tempdir().unwrap();
        for (theme, parent) in [("custom", "base"), ("base", "custom")] {
            std::fs::create_dir_all(root.path().join(theme).join("symbolic/status")).unwrap();
            std::fs::write(root.path().join(theme).join("index.theme"), format!("[Icon Theme]\nInherits={parent}\nDirectories=symbolic/status\n[symbolic/status]\nSize=22\nType=Fixed")).unwrap();
        }
        let icon = root.path().join("base/symbolic/status/battery.svg");
        std::fs::write(&icon, "base").unwrap();
        let mut resolver = IconResolver {
            roots: vec![root.path().into()],
            pixmaps: vec![],
            themes: HashMap::new(),
        };
        assert_eq!(resolver.resolve("battery", "custom", 22, None), Some(icon));
        let custom = root.path().join("custom/symbolic/status/battery.svg");
        std::fs::write(&custom, "custom").unwrap();
        assert_eq!(
            resolver.resolve("battery", "custom", 22, None),
            Some(custom)
        );
        assert!(resolver.resolve("missing", "custom", 22, None).is_none());
        assert!(resolver
            .resolve("../index.theme", "custom", 22, None)
            .is_none());
    }
}
