use crate::fs::SortBy;
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Write},
    path::PathBuf,
};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub show_hidden: bool,
    pub grid_view: bool,
    pub sort: SortBy,
    #[serde(with = "bookmark_paths")]
    pub bookmarks: Vec<PathBuf>,
    #[serde(with = "bookmark_paths")]
    pub hidden_places: Vec<PathBuf>,
    #[serde(default)]
    pub sidebar_width: Option<f32>,
    #[serde(default)]
    pub sidebar_collapsed: bool,
}

mod bookmark_paths {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::{
        ffi::OsString,
        os::unix::ffi::{OsStrExt, OsStringExt},
        path::PathBuf,
    };

    #[derive(Serialize, Deserialize)]
    #[serde(untagged)]
    enum SavedPath {
        Text(String),
        UnixBytes { unix_bytes: Vec<u8> },
    }

    pub fn serialize<S: Serializer>(paths: &[PathBuf], serializer: S) -> Result<S::Ok, S::Error> {
        paths
            .iter()
            .map(|path| match path.to_str() {
                Some(text) => SavedPath::Text(text.into()),
                None => SavedPath::UnixBytes {
                    unix_bytes: path.as_os_str().as_bytes().to_vec(),
                },
            })
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<PathBuf>, D::Error> {
        Vec::<SavedPath>::deserialize(deserializer).map(|paths| {
            paths
                .into_iter()
                .map(|path| match path {
                    SavedPath::Text(text) => PathBuf::from(text),
                    SavedPath::UnixBytes { unix_bytes } => {
                        PathBuf::from(OsString::from_vec(unix_bytes))
                    }
                })
                .collect()
        })
    }
}
pub fn config_directory() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".config")
        })
        .join("rix")
}
impl Preferences {
    pub fn load() -> Self {
        std::fs::read(config_directory().join("files.json"))
            .ok()
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or_default()
    }
    pub fn save(&self) -> io::Result<()> {
        self.save_to(config_directory())
    }
    pub fn save_to(&self, directory: PathBuf) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(&directory)?;
        let mut temp = tempfile::NamedTempFile::new_in(&directory)?;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        serde_json::to_writer_pretty(&mut temp, self)?;
        temp.write_all(b"\n")?;
        temp.as_file().sync_all()?;
        temp.persist(directory.join("files.json"))
            .map_err(|e| e.error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bookmarks_roundtrip_byte_exact_paths_and_existing_preferences() {
        use std::os::unix::ffi::OsStringExt;
        let root = tempfile::tempdir().unwrap();
        let raw = PathBuf::from(std::ffi::OsString::from_vec(
            b"/tmp/\xff-directory".to_vec(),
        ));
        let preferences = Preferences {
            bookmarks: vec!["/tmp/readable".into(), raw.clone()],
            hidden_places: vec![raw.clone(), "trash:".into()],
            ..Default::default()
        };
        preferences.save_to(root.path().into()).unwrap();
        let data = std::fs::read(root.path().join("files.json")).unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(saved["bookmarks"][0], "/tmp/readable");
        assert!(saved["bookmarks"][1]["unix_bytes"].is_array());
        let loaded: Preferences = serde_json::from_slice(&data).unwrap();
        assert_eq!(loaded.bookmarks, preferences.bookmarks);
        assert_eq!(loaded.hidden_places, preferences.hidden_places);
        assert_eq!(loaded.bookmarks[1], raw);

        let old: Preferences =
            serde_json::from_slice(br#"{"bookmarks":["/home/user/Documents"]}"#).unwrap();
        assert_eq!(old.bookmarks, vec![PathBuf::from("/home/user/Documents")]);
        let defaults: Preferences = serde_json::from_slice(b"{}").unwrap();
        assert!(defaults.bookmarks.is_empty());
        assert!(defaults.hidden_places.is_empty());
        assert_eq!(defaults.sidebar_width, None);
        assert!(!defaults.sidebar_collapsed);
    }

    #[test]
    fn bookmarks_survive_atomic_save_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let p = Preferences {
            bookmarks: vec!["/tmp/a".into()],
            grid_view: true,
            ..Default::default()
        };
        p.save_to(root.path().into()).unwrap();
        p.save_to(root.path().into()).unwrap();
        let file = root.path().join("files.json");
        let loaded: Preferences = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert!(loaded.grid_view);
        assert_eq!(loaded.bookmarks, p.bookmarks);
        assert_eq!(
            std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
