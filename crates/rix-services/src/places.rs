use std::{collections::HashSet, path::PathBuf};
pub struct Place {
    pub key: String,
    pub path: PathBuf,
}
pub fn list() -> Vec<Place> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return vec![];
    };
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let text = std::fs::read_to_string(config.join("user-dirs.dirs")).unwrap_or_default();
    let mut seen = HashSet::from([home.clone()]);
    let mut places = vec![Place {
        key: "HOME".into(),
        path: home.clone(),
    }];
    for line in text.lines().map(str::trim).filter(|s| !s.starts_with('#')) {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Some(key) = key
            .trim()
            .strip_prefix("XDG_")
            .and_then(|s| s.strip_suffix("_DIR"))
        else {
            continue;
        };
        let Some(path) = parse_path(value.trim(), &home) else {
            continue;
        };
        if path.is_dir() && seen.insert(path.clone()) {
            places.push(Place {
                key: key.into(),
                path,
            });
        }
    }
    places
}
fn parse_path(value: &str, home: &std::path::Path) -> Option<PathBuf> {
    let value = value.strip_prefix('"')?.strip_suffix('"')?;
    let mut text = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        text.push(if ch == '\\' { chars.next()? } else { ch });
    }
    if let Some(suffix) = text.strip_prefix("$HOME/") {
        Some(home.join(suffix))
    } else {
        let path = PathBuf::from(text);
        path.is_absolute().then_some(path)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_quoted_paths_without_running_commands() {
        assert_eq!(
            parse_path(
                r#""$HOME/My\ Documents""#,
                std::path::Path::new("/home/test")
            ),
            Some("/home/test/My Documents".into())
        );
        assert!(parse_path(r#""$(touch /tmp/no)""#, std::path::Path::new("/home/test")).is_none());
    }
}
