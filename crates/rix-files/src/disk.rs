use super::*;

impl App {
    pub(super) fn disk_location(&self) -> PathBuf {
        let path = &self.tabs[self.active].path;
        if path == Path::new("trash:") {
            home()
        } else {
            path.clone()
        }
    }

    pub(super) fn refresh_disk(&mut self) {
        let path = self.disk_location();
        if self.disk_request.is_some()
            || (self.disk_path.as_ref() == Some(&path) && Instant::now() < self.disk_refresh_due)
        {
            return;
        }
        self.disk_next_request = self.disk_next_request.wrapping_add(1);
        let request = self.disk_next_request;
        self.disk_request = Some(request);
        self.disk_refresh_due = Instant::now() + Duration::from_secs(15);
        let sender = self.sender.clone();

        tokio::task::spawn_blocking(move || {
            let result = storage::disk_usage(&path).map_err(|error| {
                tracing::debug!(path = %path.display(), %error, "Cannot read disk capacity");
                error.to_string()
            });
            let _ = sender.send(Message::Disk {
                request,
                path,
                result,
            });
        });
    }

    pub(super) fn render_disk(&mut self, window: &FilesWindow) {
        window.set_disk_icon(self.icon("drive-harddisk"));
        let current = self.disk_path.as_ref() == Some(&self.disk_location());
        if let Some(disk) = self.disk_usage.as_ref().filter(|_| current) {
            window.set_disk_name(if disk.mount_path == Path::new("/") {
                i18n::tr("System disk")
            } else {
                translated(
                    "Disk · {name}",
                    &[(
                        "name",
                        disk.mount_path
                            .file_name()
                            .unwrap_or(disk.mount_path.as_os_str())
                            .to_string_lossy()
                            .into_owned(),
                    )],
                )
                .into()
            });
            window.set_disk_mount(disk.mount_path.to_string_lossy().into_owned().into());
            window.set_disk_detail(
                translated(
                    "{free} free of {total}",
                    &[
                        ("free", size(disk.available_bytes).replace(' ', "\u{00a0}")),
                        ("total", size(disk.total_bytes).replace(' ', "\u{00a0}")),
                    ],
                )
                .into(),
            );
            window.set_disk_ratio(disk.usage_ratio as f32);
            window.set_disk_usage(format!("{:.0}%", disk.usage_ratio * 100.).into());
            window.set_disk_ready(true);
        } else {
            window.set_disk_name(i18n::tr("Disk"));
            window.set_disk_mount("".into());
            window.set_disk_detail(i18n::tr(if current {
                "Disk usage unavailable"
            } else {
                "Reading disk usage…"
            }));
            window.set_disk_ratio(0.);
            window.set_disk_usage("".into());
            window.set_disk_ready(false);
        }
    }

    pub(super) fn disk_target_path(&self) -> PathBuf {
        if self.disk_path.as_ref() == Some(&self.disk_location()) {
            if let Some(disk) = &self.disk_usage {
                return disk.mount_path.clone();
            }
        }
        self.disk_location()
    }

    pub(super) fn open_disk(&mut self) {
        let path = self.disk_target_path();
        self.navigate(path);
    }
}
