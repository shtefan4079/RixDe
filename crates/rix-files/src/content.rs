use super::*;
use rix_files::{archive, preview};
#[derive(Default)]
pub(super) struct State {
    archive: Option<Arc<archive::Archive>>,
    archive_path: Option<PathBuf>,
    archive_warnings: Vec<String>,
    folder: PathBuf,
    children: Vec<archive::Child>,
    password: String,
    archive_generation: u64,
    archive_cancel: Option<Arc<AtomicBool>>,
    preview_generation: u64,
    preview_path: Option<PathBuf>,
    preview_original: Option<PathBuf>,
    preview_temporary: Option<tempfile::TempDir>,
    preview_worker: bool,
    preview_loader: Arc<std::sync::Mutex<preview::Loader>>,
    pending_preview: Option<preview::Request>,
    pub preview_visible: bool,
}
pub(super) enum Event {
    Archive {
        generation: u64,
        result: Result<(archive::Archive, Vec<String>), String>,
    },
    Member {
        generation: u64,
        result: Result<(tempfile::TempDir, PathBuf), String>,
    },
    Preview {
        generation: u64,
        result: Result<preview::Preview, String>,
    },
    Extracted {
        result: Result<(PathBuf, operations::Report), String>,
    },
    Applications {
        generation: u64,
        path: PathBuf,
        result: Result<opening::ApplicationChoices, String>,
    },
}
impl App {
    pub(super) fn open_archive(&mut self, path: PathBuf, password: String) {
        if let Some(cancel) = self.content.archive_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.content.archive_generation += 1;
        let generation = self.content.archive_generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.content.archive_cancel = Some(cancel.clone());
        self.content.password = password.clone();
        self.content.archive_path = Some(path.clone());
        self.content.archive = None;
        self.content.archive_warnings.clear();
        self.content.children.clear();
        self.content.folder = PathBuf::new();
        if let Some(window) = self.window.upgrade() {
            let ui = window.global::<ArchiveBrowser>();
            ui.set_visible(true);
            ui.set_busy(true);
            ui.set_title(
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
                    .into(),
            );
            ui.set_message("".into());
            ui.set_summary("".into());
            ui.set_password("".into());
            ui.set_members(model(Vec::<ArchiveMember>::new()));
            ui.set_selected_index(-1);
        }
        let sender = self.sender.clone();
        tokio::task::spawn_blocking(move || {
            let result = archive::inspect_with_warnings(&path, &password, &cancel)
                .map_err(|error| error.to_string());
            let _ = sender.send(Message::Content(Event::Archive { generation, result }));
        });
    }
    fn render_archive(&mut self) {
        let Some(archive) = &self.content.archive else {
            return;
        };
        self.content.children = archive.children(&self.content.folder, "");
        let children = self.content.children.clone();
        let members = children
            .iter()
            .map(|entry| ArchiveMember {
                name: entry.name.clone().into(),
                directory: entry.directory,
                detail: if entry.directory {
                    i18n::tr("Folder")
                } else {
                    size(entry.size).into()
                },
                icon: self.icon_candidates(&rix_files::file_icons::names(
                    &entry.path,
                    entry.directory,
                    false,
                )),
            })
            .collect();
        if let Some(window) = self.window.upgrade() {
            let ui = window.global::<ArchiveBrowser>();
            ui.set_members(model(members));
            ui.set_selected_index(-1);
            ui.set_folder(self.content.folder.to_string_lossy().into_owned().into());
            ui.set_can_up(!self.content.folder.as_os_str().is_empty());
            let mut summary = format!(
                "{} · {} · {}",
                self.content.archive.as_ref().unwrap().format,
                self.content.archive.as_ref().unwrap().members.len(),
                size(self.content.archive.as_ref().unwrap().total_size)
            );
            if !self.content.archive_warnings.is_empty() {
                summary.push_str(&format!(
                    " · {}",
                    translated(
                        "{count} skipped",
                        &[("count", self.content.archive_warnings.len().to_string())]
                    )
                ));
            }
            ui.set_summary(summary.into());
            ui.set_busy(false);
        }
    }
    pub(super) fn refresh_archive_icons(&mut self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let ui = window.global::<ArchiveBrowser>();
        let selected = ui.get_selected_index();
        if !ui.get_visible() || self.content.archive.is_none() {
            return;
        }
        self.render_archive();
        ui.set_selected_index(selected);
    }
    fn archive_open(&mut self, index: i32) {
        if index < 0
            || self
                .window
                .upgrade()
                .is_some_and(|window| window.global::<ArchiveBrowser>().get_busy())
        {
            return;
        }
        let Some(entry) = self.content.children.get(index.max(0) as usize).cloned() else {
            return;
        };
        if entry.directory {
            self.content.folder = entry.path;
            self.render_archive();
            return;
        }
        let Some(archive) = self.content.archive.clone() else {
            return;
        };
        self.content.archive_generation += 1;
        let generation = self.content.archive_generation;
        let password = self.content.password.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.content.archive_cancel = Some(cancel.clone());
        if let Some(window) = self.window.upgrade() {
            window.global::<ArchiveBrowser>().set_busy(true);
        }
        let sender = self.sender.clone();
        tokio::task::spawn_blocking(move || {
            let result = archive::read_member(&archive, &entry.path, &password, &cancel)
                .map_err(|error| error.to_string());
            let _ = sender.send(Message::Content(Event::Member { generation, result }));
        });
    }
    fn extract_archive(&mut self) {
        if self.operation.is_some() {
            return;
        }
        let Some(path) = self.content.archive_path.clone() else {
            return;
        };
        let destination = path.parent().map(Path::to_owned).unwrap_or_else(home);
        self.dialog(
            9,
            "Extract archive",
            i18n::tr("A new folder will be created. Existing files will be preserved.").to_string(),
            vec![path],
            destination.to_string_lossy().into_owned(),
        );
    }
    pub(super) fn confirm_extraction(&mut self, value: String) {
        let Some(path) = self.dialog_paths.first().cloned() else {
            return;
        };
        let destination = PathBuf::from(value);

        if !destination.is_absolute() {
            self.error("Choose an existing absolute extraction folder".into());
            return;
        }
        self.dismiss();
        let cancel = Arc::new(AtomicBool::new(false));
        self.operation = Some(cancel.clone());
        self.render();
        let password = self.content.password.clone();
        let sender = self.sender.clone();
        let progress = Arc::new(std::sync::Mutex::new(None::<(u64, String)>));
        let worker = progress.clone();
        let window = self.window.clone();
        let total = self
            .content
            .archive
            .as_ref()
            .map_or(0, |archive| archive.total_size);
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(100),
            move || {
                if let Some((bytes, name)) = progress.lock().unwrap().take() {
                    if let Some(window) = window.upgrade() {
                        window.set_operation_progress(if total > 0 {
                            (bytes as f64 / total as f64).min(1.) as f32
                        } else {
                            0.
                        });
                        window.set_operation_message(name.into());
                    }
                }
            },
        );
        OPERATION_TIMER.with(|slot| *slot.borrow_mut() = Some(timer));
        if let Some(window) = self.window.upgrade() {
            window.global::<ArchiveBrowser>().set_visible(false);
        }
        tokio::task::spawn_blocking(move || {
            let result = if destination.is_dir() {
                archive::extract_with_warnings(
                    &path,
                    &password,
                    &destination,
                    &cancel,
                    |bytes, name| {
                        *worker.lock().unwrap() =
                            Some((bytes, name.to_string_lossy().into_owned()));
                    },
                )
                .map(|(path, skipped)| {
                    let mut report = history::created(path.clone());
                    report.warnings = skipped;
                    (path, report)
                })
                .map_err(|error| error.to_string())
            } else {
                Err("Choose an existing absolute extraction folder".into())
            };
            let _ = sender.send(Message::Content(Event::Extracted { result }));
        });
    }
    pub(super) fn preview(&mut self, path: PathBuf) {
        self.content.preview_original = Some(path.clone());
        self.content.preview_temporary = None;
        self.show_preview(path);
    }
    fn show_preview(&mut self, path: PathBuf) {
        self.content.preview_path = Some(path.clone());
        self.content.preview_visible = true;
        if let Some(window) = self.window.upgrade() {
            let ui = window.global::<FilePreview>();
            ui.set_visible(true);
            ui.set_external_allowed(self.content.preview_original.is_some());
            ui.set_body("".into());
            ui.set_page_image(Image::default());
            ui.set_image_width(0);
            ui.set_image_height(0);
            ui.set_title(
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
                    .into(),
            );
            ui.set_message("".into());
            ui.set_note(if self.content.preview_original.is_some() {
                "".into()
            } else {
                i18n::tr("Extract the archive to open this file in another application")
            });
            ui.set_query("".into());
            ui.set_match_start(-1);
            ui.set_match_end(-1);
            ui.set_search_status("".into());
            ui.set_page(0);
            ui.set_zoom(1.);
        }
        self.request_preview(0, 1.);
    }
    fn request_preview(&mut self, page: usize, zoom: f32) {
        let Some(path) = self.content.preview_path.clone() else {
            return;
        };
        let width = self
            .window
            .upgrade()
            .map_or(1000, |window| window.window().size().width.min(1600));
        let request = preview::Request {
            path,
            page,
            zoom,
            target_width: width,
        };
        self.content.preview_generation += 1;
        self.content.pending_preview = Some(request);
        if !self.content.preview_worker {
            self.start_preview_worker();
        }
    }
    fn start_preview_worker(&mut self) {
        let Some(request) = self.content.pending_preview.take() else {
            return;
        };
        let generation = self.content.preview_generation;
        self.content.preview_worker = true;
        if let Some(window) = self.window.upgrade() {
            window.global::<FilePreview>().set_busy(true);
        }
        let sender = self.sender.clone();
        let loader = self.content.preview_loader.clone();
        tokio::task::spawn_blocking(move || {
            let result = loader
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .load(request)
                .map_err(|error| error.to_string());
            let _ = sender.send(Message::Content(Event::Preview { generation, result }));
        });
    }
    pub(super) fn content_event(&mut self, event: Event) {
        match event {
            Event::Applications {
                generation,
                path,
                result,
            } => {
                if generation != self.content.preview_generation
                    || !self.content.preview_visible
                    || self.content.preview_original.as_ref() != Some(&path)
                {
                    return;
                }
                match result {
                    Ok(choices) => self.message(Message::Apps(path, choices)),
                    Err(error) => self.error(error),
                }
            }
            Event::Archive { generation, result } => {
                if generation != self.content.archive_generation {
                    return;
                }
                if let Some(window) = self.window.upgrade() {
                    window.global::<ArchiveBrowser>().set_busy(false);
                }
                match result {
                    Ok((archive, warnings)) => {
                        self.content.archive_warnings = warnings;
                        self.content.archive = Some(Arc::new(archive));
                        self.render_archive();
                    }
                    Err(error) => {
                        if let Some(window) = self.window.upgrade() {
                            window.global::<ArchiveBrowser>().set_message(error.into());
                        }
                    }
                }
            }
            Event::Member { generation, result } => {
                if generation != self.content.archive_generation {
                    return;
                }
                if let Some(window) = self.window.upgrade() {
                    window.global::<ArchiveBrowser>().set_busy(false);
                }
                match result {
                    Ok((temporary, path)) => {
                        self.content.preview_original = None;
                        self.content.preview_temporary = Some(temporary);
                        self.show_preview(path);
                    }
                    Err(error) => {
                        if let Some(window) = self.window.upgrade() {
                            window.global::<ArchiveBrowser>().set_message(error.into());
                        }
                    }
                }
            }
            Event::Extracted { result } => {
                self.operation = None;
                OPERATION_TIMER.with(|slot| *slot.borrow_mut() = None);
                match result {
                    Ok((path, mut report)) => {
                        self.history.record(std::mem::take(&mut report.undo));
                        self.navigate(path);
                        if !report.errors.is_empty() {
                            self.error(report.errors.join("\n"));
                        } else if !report.warnings.is_empty() {

                            self.notice = Some(translated(
                                "{count} skipped",
                                &[("count", report.warnings.len().to_string())],
                            ));
                            self.render();
                        }
                    }
                    Err(error) => {
                        self.load();
                        self.error(error);
                    }
                }
            }
            Event::Preview { generation, result } => {
                self.content.preview_worker = false;
                if generation != self.content.preview_generation {
                    self.start_preview_worker();
                    return;
                }
                let Some(window) = self.window.upgrade() else {
                    return;
                };
                let ui = window.global::<FilePreview>();
                ui.set_busy(false);
                match result {
                    Ok(preview) => {
                        ui.set_title(preview.title.into());
                        ui.set_kind(match preview.kind {
                            preview::Kind::Pdf => 0,
                            preview::Kind::Image => 1,
                            preview::Kind::Text => 2,
                            preview::Kind::Document => 3,
                            preview::Kind::Binary => 4,
                        });
                        let kind = i18n::tr(match preview.kind {
                            preview::Kind::Pdf => "PDF document",
                            preview::Kind::Image => "Image",
                            preview::Kind::Text => "Text file",
                            preview::Kind::Document => "Document",
                            preview::Kind::Binary => "Binary file",
                        });
                        ui.set_subtitle(format!("{kind} · {}", size(preview.file_size)).into());
                        ui.set_page(preview.page as i32);
                        ui.set_page_count(preview.page_count as i32);
                        ui.set_zoom(preview.zoom);
                        ui.set_message("".into());
                        let mut notes = preview.notes.iter().map(|note|i18n::tr(match note{preview::Note::Truncated=>"Preview shows the beginning of this file",preview::Note::DocumentTextOnly=>"Document text preview; original page layout is not reproduced",preview::Note::UnsupportedFormat=>"No built-in viewer for this format; a binary preview is shown",preview::Note::LossyText=>"Some text could not be decoded",preview::Note::RenderingLimited=>"Preview resolution was limited for this large page"}).to_string()).collect::<Vec<_>>();
                        if !ui.get_external_allowed() {
                            notes.push(
                                i18n::tr(
                                    "Extract the archive to open this file in another application",
                                )
                                .to_string(),
                            );
                        }
                        ui.set_note(notes.join(" · ").into());
                        match preview.content {
                            preview::Content::Raster {
                                width,
                                height,
                                rgba,
                            } => {
                                ui.set_page_image(Image::from_rgba8(slint::SharedPixelBuffer::<
                                    slint::Rgba8Pixel,
                                >::clone_from_slice(
                                    &rgba, width, height
                                )));
                                ui.set_image_width(width as i32);
                                ui.set_image_height(height as i32);
                                ui.set_body("".into());
                            }
                            preview::Content::Text { body } => ui.set_body(body.into()),
                            preview::Content::Binary { hex } => ui.set_body(hex.into()),
                        }
                    }
                    Err(error) => ui.set_message(error.into()),
                }
            }
        }
    }
    fn preview_open_external(&mut self) {
        let Some(path) = self.content.preview_original.clone() else {
            return;
        };
        let sender = self.sender.clone();
        let generation = self.content.preview_generation;
        tokio::spawn(async move {
            let chosen_path = path.clone();
            let choices =
                tokio::task::spawn_blocking(move || opening::application_choices(&chosen_path))
                    .await;
            match choices {
                Ok(Ok(mut choices))
                    if choices
                        .default_id
                        .as_deref()
                        .or_else(|| {
                            choices
                                .applications
                                .iter()
                                .find(|app| choices.recommended.contains(&app.id))
                                .map(|app| app.id.as_str())
                        })
                        .is_some_and(|id| {
                            matches!(
                                id,
                                "org.rixde.Files.Viewer.desktop"
                                    | "org.rixde.Files.Archives.desktop"
                            )
                        }) =>
                {
                    choices.applications.retain(|app| {
                        !matches!(
                            app.id.as_str(),
                            "org.rixde.Files.Viewer.desktop" | "org.rixde.Files.Archives.desktop"
                        )
                    });
                    choices.recommended.remove("org.rixde.Files.Viewer.desktop");
                    choices
                        .recommended
                        .remove("org.rixde.Files.Archives.desktop");
                    choices.default_id = None;
                    let _ = sender.send(Message::Content(Event::Applications {
                        generation,
                        path,
                        result: Ok(choices),
                    }));
                }
                _ => {
                    let chosen_path_for_error = path.clone();
                    if let Err(error) = opening::open_default(path).await {
                        let _ = sender.send(Message::Content(Event::Applications {
                            generation,
                            path: chosen_path_for_error,
                            result: Err(error),
                        }));
                    }
                }
            }
        });
    }
    fn preview_open_with(&mut self) {
        let Some(path) = self.content.preview_original.clone() else {
            return;
        };
        let sender = self.sender.clone();
        let generation = self.content.preview_generation;
        tokio::task::spawn_blocking(move || {
            let result = opening::application_choices(&path);
            let _ = sender.send(Message::Content(Event::Applications {
                generation,
                path,
                result,
            }));
        });
    }
}
pub(super) fn install(window: &FilesWindow, app: &Rc<RefCell<App>>) {
    let state = app.clone();
    window
        .global::<ArchiveBrowser>()
        .on_open(move |index| state.borrow_mut().archive_open(index));
    let state = app.clone();
    window.global::<ArchiveBrowser>().on_up(move || {
        let mut app = state.borrow_mut();
        app.content.folder.pop();
        app.render_archive();
    });
    let state = app.clone();
    window.global::<ArchiveBrowser>().on_close(move || {
        let mut app = state.borrow_mut();
        app.content.archive_generation += 1;
        if let Some(cancel) = app.content.archive_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        app.content.password.clear();
        app.content.archive = None;
        app.content.archive_path = None;
        app.content.children.clear();
        if let Some(window) = app.window.upgrade() {
            window.global::<ArchiveBrowser>().set_password("".into());
            window.global::<ArchiveBrowser>().set_visible(false);
            window.invoke_focus_files();
        }
    });
    let state = app.clone();
    window
        .global::<ArchiveBrowser>()
        .on_extract(move || state.borrow_mut().extract_archive());
    let state = app.clone();
    window
        .global::<ArchiveBrowser>()
        .on_unlock(move |password| {
            let mut app = state.borrow_mut();
            if let Some(path) = app.content.archive_path.clone() {
                app.open_archive(path, password.to_string());
            }
        });
    let state = app.clone();
    window.global::<FilePreview>().on_close(move || {
        let mut app = state.borrow_mut();
        app.content.preview_generation += 1;
        app.content.pending_preview = None;
        app.content.preview_visible = false;
        let loader = app.content.preview_loader.clone();
        tokio::task::spawn_blocking(move || {
            loader
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clear()
        });
        if let Some(window) = app.window.upgrade() {
            window.global::<FilePreview>().set_visible(false);
            window.invoke_focus_files();
        }
    });
    let state = app.clone();
    window
        .global::<FilePreview>()
        .on_page_requested(move |page| {
            let mut app = state.borrow_mut();
            let zoom = app
                .window
                .upgrade()
                .map_or(1., |window| window.global::<FilePreview>().get_zoom());
            app.request_preview(page.max(0) as usize, zoom);
        });
    let state = app.clone();
    window
        .global::<FilePreview>()
        .on_zoom_requested(move |zoom| {
            let mut app = state.borrow_mut();
            let page = app
                .window
                .upgrade()
                .map_or(0, |window| window.global::<FilePreview>().get_page().max(0));
            app.request_preview(page as usize, zoom);
        });
    let state = app.clone();
    window
        .global::<FilePreview>()
        .on_open_with(move || state.borrow_mut().preview_open_with());
    let state = app.clone();
    window
        .global::<FilePreview>()
        .on_open_externally(move || state.borrow_mut().preview_open_external());
    let weak = window.as_weak();
    window
        .global::<FilePreview>()
        .on_find_requested(move |query, backwards| {
            if let Some(window) = weak.upgrade() {
                let ui = window.global::<FilePreview>();
                let body = ui.get_body();
                let query = query.to_string();
                if query.is_empty() {
                    ui.set_search_status("".into());
                    return;
                }
                let start = if backwards {
                    ui.get_match_start().max(0) as usize
                } else {
                    ui.get_match_end().max(0) as usize
                }
                .min(body.len());
                let matches = preview::find_matches(&body, &query);
                let result = if backwards {
                    matches
                        .iter()
                        .rev()
                        .find(|item| item.start < start)
                        .or_else(|| matches.last())
                } else {
                    matches
                        .iter()
                        .find(|item| item.start >= start)
                        .or_else(|| matches.first())
                };
                if let Some(index) = result {
                    ui.set_match_start(index.start as i32);
                    ui.set_match_end(index.end as i32);
                    ui.set_match_generation(ui.get_match_generation() + 1);
                    ui.set_search_status("".into());
                } else {
                    ui.set_search_status(i18n::tr("No matches"));
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_theme_refresh_replaces_cached_icons_in_open_archive_and_application_chooser() {
        let root = tempfile::tempdir().unwrap();
        let (window, mut app, _receiver) = controller::tests::app(root.path().into());
        app.content.archive = Some(Arc::new(archive::Archive {
            path: root.path().join("fixture.zip"),
            format: "ZIP".into(),
            total_size: 1,
            members: vec![archive::Member {
                path: "Setup.EXE".into(),
                directory: false,
                size: 1,
                link: false,
                mode: None,
                modified: None,
            }],
        }));
        window.global::<ArchiveBrowser>().set_visible(true);
        let icon = slint::Image::load_from_svg_data(b"<svg xmlns='http://www.w3.org/2000/svg' width='8' height='8'><rect width='8' height='8' fill='#ff00ff'/></svg>").unwrap();
        let family = rix_files::file_icons::names(Path::new("Setup.EXE"), false, false);
        app.icons.insert(family.join("\u{1f}"), icon.clone());
        app.icons
            .insert("rix-fixture-missing-app-icon".into(), icon.clone());
        app.application_choices = Some(opening::ApplicationChoices {
            mime: "application/x-ms-dos-executable".into(),
            applications: vec![opening::Application {
                id: "fixture.desktop".into(),
                name: "Fixture app".into(),
                icon: "rix-fixture-missing-app-icon".into(),
            }],
            recommended: ["fixture.desktop".into()].into_iter().collect(),
            default_id: None,
        });
        app.render_archive();
        window.global::<ArchiveBrowser>().set_selected_index(0);

        assert_eq!(
            window
                .global::<ArchiveBrowser>()
                .get_members()
                .row_data(0)
                .unwrap()
                .icon
                .size()
                .width,
            8
        );
        app.message(Message::Theme(Box::new(ThemeSettings::crisp_light())));
        let archive = window.global::<ArchiveBrowser>();
        let chooser = window.global::<ApplicationChooser>();
        assert_eq!(
            archive.get_selected_index(),
            0,
            "Appearance reload must preserve archive selection"
        );
        let icons = [
            archive.get_members().row_data(0).unwrap().icon,
            chooser.get_applications().row_data(0).unwrap().icon,
        ];
        for image in icons {
            let pixels = image
                .to_rgba8()
                .expect("The new icon must contain visible pixels");
            assert!(pixels.as_slice().iter().any(|pixel| pixel.a != 0));
            assert!(
                pixels.as_slice().iter().any(|pixel| pixel.r != 255
                    || pixel.g != 0
                    || pixel.b != 255
                    || pixel.a != 255),
                "Open panels must not retain the old cached sentinel"
            );
        }
    }

    #[test]
    fn archive_summary_reports_skipped_members() {
        let root = tempfile::tempdir().unwrap();
        let (window, mut app, _receiver) = controller::tests::app(root.path().into());
        app.content.archive = Some(Arc::new(archive::Archive {
            path: root.path().join("fixture.zip"),
            format: "ZIP".into(),
            total_size: 1,
            members: vec![],
        }));
        app.content.archive_warnings = vec![
            "Skipped duplicate path: dup".into(),
            "Skipped link or special file: link".into(),
        ];
        app.render_archive();
        let summary = window.global::<ArchiveBrowser>().get_summary().to_string();
        assert!(
            summary.contains("2 skipped"),
            "the summary must count skipped members without blocking extraction: {summary}"
        );
    }

    #[tokio::test]
    async fn extraction_warnings_reach_the_status_notice() {
        let root = tempfile::tempdir().unwrap();
        let (_window, mut app, _receiver) = controller::tests::app(root.path().into());
        let mut report = operations::Report::default();
        report
            .warnings
            .push("Skipped link or special file: link".into());
        app.content_event(Event::Extracted {
            result: Ok((root.path().to_path_buf(), report)),
        });
        assert_eq!(app.notice.as_deref(), Some("1 skipped"));
    }

    fn applications(generation: u64, path: &Path) -> Event {
        Event::Applications {
            generation,
            path: path.to_owned(),
            result: Ok(opening::ApplicationChoices {
                mime: "text/plain".into(),
                applications: Vec::new(),
                recommended: Default::default(),
                default_id: None,
            }),
        }
    }

    #[test]
    fn delayed_application_choices_cannot_reopen_a_closed_or_replaced_preview() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first.txt");
        let second = root.path().join("second.txt");
        let (window, mut app, _receiver) = controller::tests::app(root.path().into());
        app.content.preview_generation = 3;
        app.content.preview_original = Some(first.clone());
        app.content.preview_visible = true;
        app.content_event(applications(2, &first));
        assert_eq!(app.dialog_kind, 0);
        app.content.preview_visible = false;
        app.content_event(applications(3, &first));
        assert_eq!(app.dialog_kind, 0);
        app.content.preview_visible = true;
        app.content.preview_original = Some(second.clone());
        app.content_event(applications(3, &first));
        assert_eq!(app.dialog_kind, 0);
        app.content.preview_original = None;
        app.content_event(applications(3, &second));
        assert_eq!(app.dialog_kind, 0);
        app.content.preview_original = Some(second.clone());
        app.content_event(applications(3, &second));
        assert_eq!(app.dialog_kind, 5);
        assert_eq!(app.application_path.as_ref(), Some(&second));
        assert_eq!(window.get_dialog_kind(), 5);
    }

    #[test]
    fn archived_preview_never_launches_an_application_with_a_temporary_path() {
        let root = tempfile::tempdir().unwrap();
        let (_window, mut app, receiver) = controller::tests::app(root.path().into());
        app.content.preview_visible = true;
        app.content.preview_path = Some(root.path().join("temporary-member.txt"));
        app.preview_open_external();
        app.preview_open_with();
        assert!(receiver.try_recv().is_err());
        assert_eq!(app.dialog_kind, 0);
    }
}
