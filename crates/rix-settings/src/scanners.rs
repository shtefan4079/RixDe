use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

pub fn get_icon_search_paths() -> Vec<PathBuf> {
    let user = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let system = std::env::var_os("XDG_DATA_DIRS")
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".icons"))
        .into_iter()
        .chain(user.map(|p| p.join("icons")))
        .chain(std::env::split_paths(&system).map(|p| p.join("icons")))
        .collect()
}

pub fn scan_icon_themes() -> Vec<String> {
    let mut themes = BTreeSet::new();
    let search_paths = get_icon_search_paths();

    for dir in search_paths {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {

                    let index = path.join("index.theme");
                    let apps = path.join("apps");
                    let scalable = path.join("scalable");
                    if index.exists() || apps.exists() || scalable.exists() {
                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {

                            if name != "default" && name != "hicolor" && !name.starts_with('.') {
                                themes.insert(name.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    themes.insert("hicolor".into());
    themes.into_iter().collect()
}

pub fn scan_cursor_themes() -> Vec<String> {
    let mut cursors = BTreeSet::new();
    let search_paths = get_icon_search_paths();

    for dir in search_paths {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && path.join("cursors").is_dir() {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if name != "default" && !name.starts_with('.') {
                            cursors.insert(name.to_string());
                        }
                    }
                }
            }
        }
    }

    cursors.insert("default".into());
    cursors.into_iter().collect()
}
