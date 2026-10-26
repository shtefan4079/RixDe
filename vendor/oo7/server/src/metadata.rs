//! Nonsecret Secret Service collection labels/aliases survive restart.
use crate::collection::Collection;
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Serialize, Deserialize)]
pub(crate) struct Metadata {
    pub label: String,
    pub alias: String,
}
impl Metadata {
    pub fn path(data_dir: &Path, name: &str) -> PathBuf {
        data_dir
            .join("keyrings/v1")
            .join(format!("{name}.metadata.json"))
    }
    pub async fn load(data_dir: &Path, name: &str) -> Option<Self> {
        let path = Self::path(data_dir, name);
        let stat = tokio::fs::symlink_metadata(&path).await.ok()?;
        if !stat.is_file() || stat.len() > 16_384 {
            return None;
        }
        let bytes = tokio::fs::read(path).await.ok()?;
        let result: Self = serde_json::from_slice(&bytes).ok()?;
        if result.label.trim().is_empty() || result.label.len() > 256 || result.alias.len() > 256 {
            return None;
        }
        Some(result)
    }
    pub async fn save(data_dir: &Path, collection: &Collection) -> io::Result<()> {
        if collection.name() == "session" {
            return Ok(());
        }
        let guard = collection.keyring.read().await;
        if guard
            .as_ref()
            .is_none_or(|keyring| keyring.path().is_none())
        {
            return Ok(());
        }
        drop(guard);
        let value = Self {
            label: collection.label().await,
            alias: collection.alias().await,
        };
        let bytes = serde_json::to_vec_pretty(&value).map_err(io::Error::other)?;
        let path = Self::path(data_dir, collection.name());
        tokio::task::spawn_blocking(move || atomic_write(&path, &bytes))
            .await
            .map_err(io::Error::other)?
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Missing metadata directory"))?;
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".metadata-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        std::fs::File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}
