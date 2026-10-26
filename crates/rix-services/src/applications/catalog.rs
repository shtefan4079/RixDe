use super::{application_dirs, scan_applications_in, AppInfo};
use notify::{EventKind, RecursiveMode, Watcher};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{mpsc, Arc},
    thread,
    time::{Duration, Instant},
};
use tokio::sync::watch;

#[derive(Clone)]
pub struct ApplicationCatalog {
    updates: watch::Receiver<Arc<Vec<AppInfo>>>,
}
impl ApplicationCatalog {
    pub fn watch() -> Self {
        watch_in(
            application_dirs(),
            Duration::from_secs(3),
            Duration::from_millis(150),
        )
    }
    pub fn snapshot(&self) -> Arc<Vec<AppInfo>> {
        self.updates.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<Arc<Vec<AppInfo>>> {
        self.updates.clone()
    }
}
impl From<Arc<Vec<AppInfo>>> for ApplicationCatalog {
    fn from(apps: Arc<Vec<AppInfo>>) -> Self {
        let (_, updates) = watch::channel(apps);
        Self { updates }
    }
}
fn watch_in(dirs: Vec<PathBuf>, reconcile: Duration, debounce: Duration) -> ApplicationCatalog {
    let (sender, updates) = watch::channel(Arc::new(Vec::new()));
    thread::Builder::new()
        .name("rix-app-catalog".into())
        .spawn(move || {

            let (dirty, events) = mpsc::sync_channel(1);
            let mut watcher =
                notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                    if !matches!(
                        event.as_ref().map(|event| &event.kind),
                        Ok(EventKind::Access(_))
                    ) {
                        let _ = dirty.try_send(());
                    }
                })
                .ok();
            let mut watches = HashMap::new();
            let mut next_scan = Instant::now();
            while !sender.is_closed() {
                if let Some(watcher) = watcher.as_mut() {
                    reconcile_watches(watcher, &dirs, &mut watches);
                }
                if Instant::now() >= next_scan {
                    let started = Instant::now();
                    let apps = scan_applications_in(&dirs);
                    if sender.borrow().as_ref() != &apps {
                        tracing::info!(
                            count = apps.len(),
                            elapsed_ms = started.elapsed().as_millis(),
                            "Application catalog updated"
                        );
                        sender.send_replace(Arc::new(apps));
                    }
                    next_scan = Instant::now() + reconcile;
                }
                let wait = next_scan
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(250));
                match events.recv_timeout(wait) {
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        thread::sleep(wait);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Ok(()) => {

                        let start = Instant::now();
                        while start.elapsed() < Duration::from_secs(1)
                            && events.recv_timeout(debounce).is_ok()
                        {}
                        next_scan = Instant::now();
                    }
                }
            }
        })
        .expect("Cannot start the application catalog worker");
    ApplicationCatalog { updates }
}
fn reconcile_watches(
    watcher: &mut notify::RecommendedWatcher,
    dirs: &[PathBuf],
    watched: &mut HashMap<PathBuf, RecursiveMode>,
) {
    let mut desired = HashMap::new();
    for dir in dirs {
        if dir.is_dir() {
            desired.insert(dir.clone(), RecursiveMode::Recursive);
        } else if let Some(parent) = dir.ancestors().skip(1).find(|path| path.is_dir()) {
            desired
                .entry(parent.to_path_buf())
                .or_insert(RecursiveMode::NonRecursive);
        }
    }
    let obsolete: Vec<_> = watched
        .iter()
        .filter(|(path, mode)| desired.get(*path) != Some(*mode))
        .map(|(path, _)| path.clone())
        .collect();
    for path in obsolete {
        let _ = watcher.unwatch(&path);
        watched.remove(&path);
    }
    for (path, mode) in desired {
        if !watched.contains_key(&path) && watcher.watch(&path, mode).is_ok() {
            watched.insert(path, mode);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wait(catalog: &ApplicationCatalog, expected: &[&str]) {
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            let names: Vec<_> = catalog
                .snapshot()
                .iter()
                .map(|app| app.name.clone())
                .collect();
            if names == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Expected {expected:?}, received {names:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn refreshes_install_rename_remove_and_initially_missing_dirs() {
        let root = tempfile::tempdir().unwrap();
        let applications = root.path().join("exports/share/applications");
        let catalog = watch_in(
            vec![applications.clone()],
            Duration::from_millis(150),
            Duration::from_millis(20),
        );
        std::fs::create_dir_all(&applications).unwrap();
        let entry = applications.join("demo.desktop");
        std::fs::write(
            &entry,
            "[Desktop Entry]\nType=Application\nName=First\nExec=true\n",
        )
        .unwrap();
        wait(&catalog, &["First"]);
        let replacement = applications.join("replacement.tmp");
        std::fs::write(
            &replacement,
            "[Desktop Entry]\nType=Application\nName=Updated\nExec=true\n",
        )
        .unwrap();
        std::fs::rename(replacement, &entry).unwrap();
        wait(&catalog, &["Updated"]);
        std::fs::remove_dir_all(root.path().join("exports")).unwrap();
        wait(&catalog, &[]);
        std::fs::create_dir_all(&applications).unwrap();
        std::fs::write(
            &entry,
            "[Desktop Entry]\nType=Application\nName=Reinstalled\nExec=true\n",
        )
        .unwrap();
        wait(&catalog, &["Reinstalled"]);
    }
    #[test]
    fn directory_events_refresh_before_the_periodic_reconcile() {
        let root = tempfile::tempdir().unwrap();
        let entry = root.path().join("demo.desktop");
        std::fs::write(
            &entry,
            "[Desktop Entry]\nType=Application\nName=First\nExec=true\n",
        )
        .unwrap();
        let catalog = watch_in(
            vec![root.path().to_path_buf()],
            Duration::from_secs(60),
            Duration::from_millis(20),
        );
        wait(&catalog, &["First"]);
        std::fs::write(
            &entry,
            "[Desktop Entry]\nType=Application\nName=Changed by event\nExec=true\n",
        )
        .unwrap();
        wait(&catalog, &["Changed by event"]);
        std::fs::remove_file(entry).unwrap();
        wait(&catalog, &[]);
    }
    #[test]
    fn reconcile_recovers_try_exec_changes_outside_watched_application_dirs() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let applications = root.path().join("applications");
        std::fs::create_dir_all(&applications).unwrap();
        let binary = root.path().join("installed-later");
        std::fs::write(applications.join("demo.desktop"), format!("[Desktop Entry]\nType=Application\nName=After package install\nExec=true\nTryExec={}\n", binary.display())).unwrap();
        let catalog = watch_in(
            vec![applications],
            Duration::from_millis(100),
            Duration::from_millis(20),
        );
        assert!(catalog.snapshot().is_empty());
        std::fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        wait(&catalog, &["After package install"]);
        std::fs::remove_file(binary).unwrap();
        wait(&catalog, &[]);
    }
}
