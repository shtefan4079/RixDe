use super::*;

pub(crate) fn menu_rows_height(items: ModelRc<MenuItem>, row: f32, separator: f32) -> f32 {
    items
        .iter()
        .map(|item| if item.separator { separator } else { row })
        .sum()
}

pub(crate) fn menu_width(items: ModelRc<MenuItem>, font_size: f32, _base_width: f32) -> f32 {
    let font_size = if font_size > 0. { font_size } else { 13. };
    let mut max_item_width: f32 = 0.;
    for item in items.iter() {
        if item.separator {
            continue;
        }
        let text_w: f32 = item.label.chars().count() as f32 * font_size * 0.62;
        let shortcut_w: f32 = if item.shortcut.is_empty() {
            0.
        } else {
            item.shortcut.chars().count() as f32 * font_size * 0.55 + 16.
        };
        let icon_w = if item.checked { 28. } else { 0. };
        let total_w = 28. + icon_w + text_w + shortcut_w;
        if total_w > max_item_width {
            max_item_width = total_w;
        }
    }
    max_item_width.clamp(130., 280.)
}

fn place_at(items: ModelRc<PlaceItem>, y: f32, row: f32, group: f32) -> i32 {
    if ![y, row, group].iter().all(|value| value.is_finite()) || y < 0. || row <= 0. || group < 0. {
        return -1;
    }
    let mut top = 0.;
    for (index, place) in items.iter().enumerate() {
        let heading = if place.group_start { group } else { 0. };
        if y >= top + heading && y < top + heading + row {
            return i32::try_from(index).unwrap_or(-1);
        }
        top += heading + row;
    }
    -1
}

fn next_menu_index(items: ModelRc<MenuItem>, current: i32, direction: i32) -> i32 {
    let count = items.row_count();
    if count == 0 {
        return -1;
    }
    let backwards = direction < 0 || direction == 2;
    let previous = usize::try_from(current).ok().filter(|index| *index < count);
    let advance = |index: usize| {
        if backwards {
            if index == 0 {
                count - 1
            } else {
                index - 1
            }
        } else if index == count - 1 {
            0
        } else {
            index + 1
        }
    };
    let mut index = match direction {
        0 => 0,
        2 => count - 1,
        _ => previous
            .map(advance)
            .unwrap_or(if backwards { count - 1 } else { 0 }),
    };
    for _ in 0..count {
        if items
            .row_data(index)
            .is_some_and(|item| item.enabled && !item.separator)
        {
            return i32::try_from(index).unwrap_or(-1);
        }
        index = advance(index);
    }
    -1
}

impl App {
    fn begin_rectangle(&mut self, control: bool, shift: bool) {
        self.tabs[self.active].begin_rectangle(control, shift);
        self.render();
    }
    fn update_rectangle(
        &mut self,
        rect: rix_files::selection::SelectionRect,
        layout: rix_files::selection::EntryLayout,
    ) {
        if self.tabs[self.active].update_rectangle(rect, layout) {
            self.render();
        }
    }
    fn finish_rectangle(&mut self, cancelled: bool) {
        self.tabs[self.active].end_rectangle(cancelled);
        self.render();
    }
    fn refresh_application_choices(&mut self) {
        let (Some(w), Some(choices)) = (self.window.upgrade(), self.application_choices.clone())
        else {
            return;
        };
        let ui = w.global::<ApplicationChooser>();
        let query = ui.get_query().trim().to_lowercase();
        let show_all = ui.get_show_all();
        let selected = ui
            .get_applications()
            .row_data(ui.get_selected_index().max(0) as usize)
            .map(|app| app.id);
        let mut rows = Vec::new();
        for application in choices.applications.iter().filter(|application| {
            (show_all || choices.recommended.contains(&application.id))
                && (query.is_empty()
                    || application.name.to_lowercase().contains(&query)
                    || application.id.to_lowercase().contains(&query))
        }) {
            let recommended = choices.recommended.contains(&application.id);
            rows.push(ChooserApplication {
                id: application.id.clone().into(),
                label: application.name.clone().into(),
                detail: if recommended {
                    i18n::tr("Supports this file type")
                } else {
                    i18n::tr("Other application")
                },
                icon: self.icon(&application.icon),
                recommended,
                is_default: choices.default_id.as_ref() == Some(&application.id),
            });
        }
        let selected_index = selected
            .and_then(|id| rows.iter().position(|app| app.id == id))
            .or_else(|| (!rows.is_empty()).then_some(0))
            .map_or(-1, |index| index as i32);
        ui.set_applications(model(rows));
        ui.set_selected_index(selected_index);
    }

    fn choose_application(&mut self, id: String, always: bool) {
        if self.dialog_kind != 5 {
            return;
        }
        let (Some(path), Some(choices)) = (
            self.application_path.clone(),
            self.application_choices.as_ref(),
        ) else {
            return;
        };
        if !choices
            .applications
            .iter()
            .any(|application| application.id == id)
        {
            return;
        }
        let mime = choices.mime.clone();
        let sender = self.sender.clone();
        self.dismiss();
        tokio::spawn(async move {
            let result = async {
                if always {
                    let application_id = id.clone();
                    tokio::task::spawn_blocking(move || {
                        opening::set_default(&mime, &application_id)
                    })
                    .await
                    .map_err(|error| error.to_string())??;
                }
                opening::open_with(path, id).await
            }
            .await;
            if let Err(error) = result {
                let _ = sender.send(Message::Error(error));
            }
        });
    }

    fn refresh_appearance(&mut self) {
        let (mut theme, material) = self
            .window_appearance
            .effective(APPLICATION_ID, &self.desktop_theme);
        if !material {
            theme.blur_radius = 0;
            theme.opacity = 1.;
        }
        if theme == self.theme && material == self.desktop_material_enabled {
            return;
        }
        self.theme = theme;
        self.desktop_material_enabled = material;
        self.icons.clear();
        self.resolver = rix_assets::IconResolver::default();
        if let Some(window) = self.window.upgrade() {
            window.global::<DialogBackdrop>().invoke_clear();
            window
                .global::<DialogBackdrop>()
                .set_radius(self.theme.blur_radius as f32);
            window.set_desktop_material_enabled(material);
            theme_bindings::apply_for_app(
                window.global::<Theme>(),
                window.global::<Palette>(),
                &self.theme,
            );
        }
        if let Some(style) = &self.native_style {
            style.borrow_mut().update(&self.theme);
        }
        if let Some(popup) = &self.native_popup {
            popup.borrow_mut().update(&self.theme);
        }
        self.render();
        self.refresh_application_choices();
        self.refresh_archive_icons();
        self.render_properties(self.properties_summary.clone().as_ref());
    }
    fn context_entry(&mut self, index: usize) {
        let tab = &mut self.tabs[self.active];
        let Some(entry) = tab.entries.get(index) else {
            return;
        };
        if !tab.selected.contains(&entry.path) {
            tab.select(index, false, false);
        }

        tab.focus = Some(index);
        self.render();
    }
    fn unpin_place(&mut self, index: usize) {
        let Some(place) = self.places.get(index) else {
            return;
        };
        let path = place.path.clone();
        if place.builtin && !self.prefs.hidden_places.contains(&path) {
            self.prefs.hidden_places.push(path.clone());
        }
        self.prefs.bookmarks.retain(|bookmark| bookmark != &path);
        self.places = places(&self.prefs);
        self.save();
        self.render();
    }
    fn update_grid_columns(&mut self, columns: i32) {
        let columns = columns.clamp(1, 24) as usize;
        if self.grid_columns != columns {
            self.grid_columns = columns;
            self.render();
        }
    }
    fn trash_operation(&mut self, paths: Vec<PathBuf>, restore: bool) {
        if self.operation.is_some() {
            return;
        }
        let items: Vec<_> = paths
            .iter()
            .filter_map(|p| self.trash_items.get(p).cloned())
            .collect();
        let cancel = Arc::new(AtomicBool::new(false));
        self.operation = Some(cancel.clone());
        self.render();
        let sender = self.sender.clone();
        tokio::task::spawn_blocking(move || {
            if restore {
                let report = history::restore(items, &cancel, |_| {});
                let _ = sender.send(Message::Operation(report));
                return;
            }
            let mut report = operations::Report::default();
            for item in items {
                if cancel.load(Ordering::Relaxed) {
                    report.cancelled = true;
                    break;
                }
                let id = PathBuf::from(&item.id);
                let result = trash::os_limited::purge_all([item]);
                match result {
                    Ok(()) => report.completed.push(id),
                    Err(e) => report.errors.push(e.to_string()),
                }
            }
            let _ = sender.send(Message::Operation(report));
        });
    }

    pub(super) fn show_paths_properties(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let request = self
            .properties_metadata_request
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1);
        let is_trash = self.tabs[self.active].path == Path::new("trash:");
        if is_trash {
            let sender = self.sender.clone();
            let deleted: Vec<_> = paths
                .iter()
                .filter_map(|path| {
                    self.trash_items.get(path).map(|item| {
                        (
                            path.clone(),
                            item.original_path(),
                            (item.time_deleted >= 0).then(|| {
                                SystemTime::UNIX_EPOCH
                                    + Duration::from_secs(item.time_deleted as u64)
                            }),
                        )
                    })
                })
                .collect();
            tokio::task::spawn_blocking(move || {
                let result = describe_properties(&paths).map(|mut properties| {
                    for item in &mut properties {
                        if let Some((_, original, date)) =
                            deleted.iter().find(|(path, _, _)| path == &item.path)
                        {
                            item.original_path = Some(original.clone());
                            item.deleted = *date;
                        }
                    }
                    properties
                });
                let _ = sender.send(Message::Properties {
                    request,
                    paths,
                    result,
                });
            });
        } else {
            let sender = self.sender.clone();
            tokio::task::spawn_blocking(move || {
                let result = describe_properties(&paths);
                let _ = sender.send(Message::Properties {
                    request,
                    paths,
                    result,
                });
            });
        }
    }

    pub(super) fn action_on_place(&mut self, action: &str, index: usize) {
        let Some(place) = self.places.get(index) else {
            return;
        };
        let path = place.path.clone();
        match action {
            "open" => {
                self.navigate(path);
            }
            "open-tab" => {
                self.add_tab(path);
            }
            "terminal" => {
                if path != Path::new("trash:") {
                    let sender = self.sender.clone();
                    tokio::spawn(async move {
                        if let Err(e) = opening::open_terminal(path).await {
                            let _ = sender.send(Message::Error(e));
                        }
                    });
                }
            }
            "copy-path" => {
                if path != Path::new("trash:") {
                    let sender = self.sender.clone();
                    tokio::task::spawn_blocking(move || {
                        if let Err(error) = rix_files::clipboard::path_text(&[path])
                            .and_then(rix_files::clipboard::publish_text)
                        {
                            let _ = sender.send(Message::Error(error));
                        }
                    });
                }
            }
            "properties" => {
                if path != Path::new("trash:") {
                    self.show_paths_properties(vec![path]);
                }
            }
            _ => {}
        }
    }

    pub(super) fn action_on_tab(&mut self, action: &str, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        match action {
            "duplicate" => {
                let path = self.tabs[index].path.clone();
                self.add_tab(path);
            }
            "copy-path" => {
                let path = self.tabs[index].path.clone();
                if path != Path::new("trash:") {
                    let sender = self.sender.clone();
                    tokio::task::spawn_blocking(move || {
                        if let Err(error) = rix_files::clipboard::path_text(&[path])
                            .and_then(rix_files::clipboard::publish_text)
                        {
                            let _ = sender.send(Message::Error(error));
                        }
                    });
                }
            }
            "close" => {
                self.close_tab(index);
            }
            "close-others" => {
                if self.tabs.len() <= 1 {
                    return;
                }
                for (i, tab) in self.tabs.iter().enumerate() {
                    if i != index {
                        tab.cancellation.store(true, Ordering::Relaxed);
                    }
                }
                let keep_tab = self.tabs.remove(index);
                self.tabs.clear();
                self.tabs.push(keep_tab);
                self.active = 0;
                self.render();
                self.watch();
            }
            "close-right" => {
                if index + 1 < self.tabs.len() {
                    for tab in &self.tabs[index + 1..] {
                        tab.cancellation.store(true, Ordering::Relaxed);
                    }
                    self.tabs.truncate(index + 1);
                    if self.active >= self.tabs.len() {
                        self.active = self.tabs.len() - 1;
                    }
                    self.render();
                    self.watch();
                }
            }
            _ => {}
        }
    }

    fn action(&mut self, action: &str) {
        if matches!(action, "open" | "preview" | "open-with") {
            self.invalidate_properties_metadata();
        }

        self.tabs[self.active].end_rectangle(false);
        if let Some(window) = self.window.upgrade() {
            window.invoke_cancel_drag_gesture();
        }
        let tab = &mut self.tabs[self.active];
        match action {
            "back" | "forward" => {
                if tab.history_step(action == "forward") {
                    self.invalidate_properties_metadata();
                    self.load();
                }
            }
            "up" => {
                if let Some(parent) = tab.path.parent().map(Path::to_path_buf) {
                    self.navigate(parent);
                }
            }
            "refresh" => self.load(),
            "undo" => self.history_operation(false),
            "redo" => self.history_operation(true),
            "restore-places" => {
                self.prefs.hidden_places.clear();
                self.places = places(&self.prefs);
                self.save();
                self.render();
            }
            "new-tab" => {
                let path = tab.path.clone();
                self.add_tab(path);
            }
            "close-tab" => self.close_tab(self.active),
            "hidden" => {
                tab.show_hidden = !tab.show_hidden;
                self.prefs.show_hidden = tab.show_hidden;
                self.save();
                self.load();
            }
            "view" => {
                self.prefs.grid_view = !self.prefs.grid_view;
                self.save();
                self.render();
            }
            "list" | "grid" => {
                let grid_view = action == "grid";
                if self.prefs.grid_view != grid_view {
                    self.prefs.grid_view = grid_view;
                    self.save();
                    self.render();
                }
            }
            "clear-selection" => {
                tab.selected.clear();
                tab.focus = None;
                tab.anchor = None;
                self.render();
            }
            "sort-name" | "sort-size" | "sort-modified" | "sort-kind" => {
                let sort = match action {
                    "sort-size" => SortBy::Size,
                    "sort-modified" => SortBy::Modified,
                    "sort-kind" => SortBy::Kind,
                    _ => SortBy::Name,
                };
                if tab.sort == sort {
                    tab.descending = !tab.descending;
                } else {
                    tab.sort = sort;
                    tab.descending = false;
                }
                self.prefs.sort = sort;
                fs::sort_entries(&mut tab.entries, tab.sort, tab.descending);
                tab.anchor = None;
                tab.focus = None;
                self.save();
                self.render();
            }
            "select-all" => {
                tab.selected = tab.entries.iter().map(|e| e.path.clone()).collect();
                self.render();
            }
            "copy" | "cut" => {
                if tab.path == Path::new("trash:") {
                    self.notice = Some("Restore trashed files before copying them".into());
                    self.render();
                    return;
                }
                let paths = tab.selected_paths();
                if paths.is_empty() {
                    return;
                }
                self.clipboard = paths.clone();
                self.cut = action == "cut";
                let cut = self.cut;
                let sender = self.sender.clone();
                self.notice = Some(translated(
                    if cut {
                        "{count} files cut · existing files will be kept when pasting"
                    } else {
                        "{count} files copied · existing files will be kept when pasting"
                    },
                    &[("count", paths.len().to_string())],
                ));
                self.render();
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = rix_files::clipboard::publish(&paths, cut) {
                        let _ =
                            sender.send(Message::ClipboardError(format!("System clipboard: {e}")));
                    }
                });
            }
            "copy-path" => {
                if tab.path == Path::new("trash:") {
                    return;
                }
                let paths = tab.selected_paths();
                self.clipboard.clear();
                self.cut = false;
                let sender = self.sender.clone();
                tokio::task::spawn_blocking(move || {
                    let result = rix_files::clipboard::path_text(&paths)
                        .and_then(rix_files::clipboard::publish_text);
                    if let Err(error) = result {
                        let _ = sender.send(Message::Error(error));
                    }
                });
                self.notice = Some("Path copied".into());
                self.render();
            }
            "paste" => {
                if self.operation.is_some() || tab.path == Path::new("trash:") {
                    return;
                }
                let sender = self.sender.clone();
                let destination = tab.path.clone();
                tokio::task::spawn_blocking(move || match rix_files::clipboard::read() {
                    Ok((mut paths, cut)) => {

                        if cut {
                            paths.retain(|path| path.exists());
                        }
                        let _ = sender.send(Message::Clipboard {
                            paths,
                            cut,
                            destination,
                        });
                    }
                    Err(e) => {

                        let _ = sender.send(Message::ClipboardError(e));
                    }
                });
            }
            "trash" => {
                if self.operation.is_some() {
                    return;
                }
                let paths = tab.selected_paths();
                if tab.path != Path::new("trash:") {
                    self.start_operation(Kind::Trash, paths, None);
                } else if !paths.is_empty() {
                    self.action("delete");
                }
            }
            "empty-trash" => {
                if self.operation.is_some() || tab.loading || tab.path != Path::new("trash:") {
                    return;
                }
                let paths: Vec<_> = self.trash_items.keys().cloned().collect();
                if paths.is_empty() {
                    self.notice = Some(i18n::tr("Trash is already empty").to_string());
                    self.render();
                    return;
                }
                self.dialog(
                    3,
                    "Empty Trash?",
                    translated(
                        "Permanently delete all {count} item(s) in the Trash? This action cannot be undone.",
                        &[("count", paths.len().to_string())],
                    ),
                    paths,
                    String::new(),
                );
            }
            "restore" => {
                let paths = tab.selected_paths();
                self.trash_operation(paths, true);
            }
            "delete" => {
                if self.operation.is_some() {
                    return;
                }
                let paths = tab.selected_paths();
                if !paths.is_empty() {
                    self.dialog(
                        3,
                        "Delete permanently?",
                        format!(
                            "{}\n\n{}",
                            translated(
                                "Delete {count} selected item(s)? This action cannot be undone.",
                                &[("count", paths.len().to_string())]
                            ),
                            paths
                                .iter()
                                .take(4)
                                .map(|p| p.file_name().unwrap_or_default().to_string_lossy())
                                .collect::<Vec<_>>()
                                .join("\n")
                        ),
                        paths,
                        String::new(),
                    );
                }
            }
            "rename" => {
                if self.operation.is_some() || tab.path == Path::new("trash:") {
                    return;
                }
                if let Some(path) = tab.selected_one().map(Path::to_path_buf) {
                    let name = path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into();
                    self.dialog(
                        2,
                        "Rename",
                        "Existing files will not be replaced.".into(),
                        vec![path],
                        name,
                    );
                }
            }
            "new-folder" | "new-file" => {
                if self.operation.is_some() || tab.path == Path::new("trash:") {
                    return;
                }
                let path = tab.path.clone();
                let file = action == "new-file";
                self.dialog(
                    if file { 6 } else { 1 },
                    if file { "New file" } else { "New folder" },
                    if file {
                        "Enter a file name."
                    } else {
                        "Enter a folder name."
                    }
                    .into(),
                    vec![path],
                    i18n::tr(if file { "New file" } else { "New folder" }).to_string(),
                );
            }
            "bookmark" => {
                let path = tab.path.clone();
                if path == Path::new("trash:") {
                    return;
                }
                if self.prefs.bookmarks.contains(&path) {
                    self.prefs.bookmarks.retain(|p| p != &path);
                    self.notice = Some("Bookmark removed".into());
                } else {
                    self.prefs.hidden_places.retain(|hidden| hidden != &path);
                    self.prefs.bookmarks.push(path);
                    self.notice = Some("Folder bookmarked".into());
                }
                self.places = places(&self.prefs);
                self.save();
                self.render();
            }
            "terminal" => {
                let path = tab
                    .selected_one()
                    .filter(|p| p.is_dir())
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| tab.path.clone());
                if path == Path::new("trash:") {
                    return;
                }
                let sender = self.sender.clone();
                tokio::spawn(async move {
                    if let Err(e) = opening::open_terminal(path).await {
                        let _ = sender.send(Message::Error(e));
                    }
                });
            }
            "preview" => {
                if let Some(path) = tab
                    .selected_one()
                    .filter(|path| path.is_file())
                    .map(Path::to_owned)
                {
                    if rix_files::archive::supported(&path) {
                        self.open_archive(path, String::new());
                    } else {
                        self.preview(path);
                    }
                }
            }
            "open" => {
                if let Some(index) = tab.focus.or_else(|| {
                    tab.entries
                        .iter()
                        .position(|e| tab.selected.contains(&e.path))
                }) {
                    self.open(index);
                }
            }
            "open-with" => {
                if let Some(path) = tab.selected_one().map(Path::to_path_buf) {
                    if tab.path == Path::new("trash:") {
                        return;
                    }
                    let sender = self.sender.clone();
                    tokio::task::spawn_blocking(move || {
                        match opening::application_choices(&path) {
                            Ok(apps) => {
                                let _ = sender.send(Message::Apps(path, apps));
                            }
                            Err(e) => {
                                let _ = sender.send(Message::Error(e));
                            }
                        }
                    });
                }
            }
            "properties" => {
                let paths = tab.selected_paths();
                self.show_paths_properties(paths);
            }
            "folder-properties" => {
                let path = tab.path.clone();
                if path != Path::new("trash:") {
                    self.show_paths_properties(vec![path]);
                }
            }
            "open-disk" => {
                self.open_disk();
            }
            "open-disk-tab" => {
                let path = self.disk_target_path();
                self.add_tab(path);
            }
            "disk-terminal" => {
                let path = self.disk_target_path();
                let sender = self.sender.clone();
                tokio::spawn(async move {
                    if let Err(e) = opening::open_terminal(path).await {
                        let _ = sender.send(Message::Error(e));
                    }
                });
            }
            "copy-disk-path" => {
                let path = self.disk_target_path();
                let sender = self.sender.clone();
                tokio::task::spawn_blocking(move || {
                    if let Err(error) = rix_files::clipboard::path_text(&[path])
                        .and_then(rix_files::clipboard::publish_text)
                    {
                        let _ = sender.send(Message::Error(error));
                    }
                });
            }
            "disk-properties" => {
                let path = self.disk_target_path();
                self.show_paths_properties(vec![path]);
            }
            "cancel-operation" => {
                if let Some(cancel) = &self.operation {
                    cancel.store(true, Ordering::Relaxed);
                    self.notice = Some("Cancelling after the current safe step…".into());
                    self.render();
                }
            }
            _ => {}
        }
    }
    pub(super) fn confirm(&mut self, value: String) {
        if self.operation.is_some() {
            return;
        }
        if self.dialog_kind == 8 {
            self.dismiss();
            self.accept_picker(true);
            return;
        }
        if self.dialog_kind == 9 {
            self.confirm_extraction(value);
            return;
        }
        let kind = self.dialog_kind;
        let paths = self.dialog_paths.clone();
        if paths.is_empty() {
            self.dismiss();
            return;
        }
        if kind == 3 {
            self.dismiss();
            if paths.iter().all(|p| self.trash_items.contains_key(p)) {
                self.trash_operation(paths, false);
            } else {
                self.start_operation(Kind::Delete, paths, None);
            }
            return;
        }
        if kind != 1 && kind != 2 && kind != 6 {
            return;
        }
        self.dismiss();
        let cancel = Arc::new(AtomicBool::new(false));
        self.operation = Some(cancel.clone());
        self.render();
        let sender = self.sender.clone();
        tokio::task::spawn_blocking(move || {

            if cancel.load(Ordering::Relaxed) {
                let _ = sender.send(Message::Operation(operations::Report {
                    cancelled: true,
                    ..Default::default()
                }));
                return;
            }
            let report = match kind {
                1 => history::create_directory(&paths[0], &value, &cancel),
                6 => history::create_file(&paths[0], &value, &cancel),
                _ => history::rename(&paths[0], &value, &cancel),
            };
            let _ = sender.send(Message::Operation(report));
        });
    }
    fn key(&mut self, text: &str, ctrl: bool, shift: bool, alt: bool) {
        use slint::platform::Key;
        if text == " " && !ctrl && !alt {
            self.action("preview");
            return;
        }
        let navigation = [
            Key::UpArrow,
            Key::DownArrow,
            Key::LeftArrow,
            Key::RightArrow,
            Key::Home,
            Key::End,
            Key::PageUp,
            Key::PageDown,
        ]
        .iter()
        .any(|key| text == char::from(*key).to_string());
        if ctrl && !alt && text == " " {
            let tab = &mut self.tabs[self.active];
            if !tab.entries.is_empty() {
                tab.select(tab.focus.unwrap_or(0), true, false);
                self.render();
            }
            return;
        }
        if ctrl && !navigation {
            if text == char::from(Key::Tab).to_string()
                || text == char::from(Key::Backtab).to_string()
            {
                self.invalidate_properties_metadata();
                self.active = if shift || text == char::from(Key::Backtab).to_string() {
                    (self.active + self.tabs.len() - 1) % self.tabs.len()
                } else {
                    (self.active + 1) % self.tabs.len()
                };
                if self.tabs[self.active].entries.is_empty() {
                    self.load();
                } else {
                    self.render();
                    self.watch();
                }
                return;
            }
            let normalized =
                rix_files::keyboard::control_letter(text).map(|letter| letter.to_string());
            let text = normalized.as_deref().unwrap_or(text);
            match text.to_lowercase().as_str() {
                "l" => {
                    if let Some(w) = self.window.upgrade() {
                        w.invoke_focus_location();
                    }
                }
                "f" => {
                    if let Some(w) = self.window.upgrade() {
                        w.invoke_focus_search();
                    }
                }
                "t" => self.action("new-tab"),
                "w" => self.action("close-tab"),
                "a" if shift => self.action("clear-selection"),
                "a" => self.action("select-all"),
                "c" if shift => self.action("copy-path"),
                "c" => self.action("copy"),
                "x" => self.action("cut"),
                "v" => self.action("paste"),
                "h" => self.action("hidden"),
                "r" => self.action("refresh"),
                "z" if shift => self.action("redo"),
                "z" => self.action("undo"),
                "y" => self.action("redo"),
                "n" if alt => self.action("new-file"),
                "n" if shift => self.action("new-folder"),
                _ => {}
            }
            return;
        }
        if alt {
            if text == char::from(Key::LeftArrow).to_string() {
                self.action("back");
            } else if text == char::from(Key::RightArrow).to_string() {
                self.action("forward");
            } else if text == char::from(Key::UpArrow).to_string() {
                self.action("up");
            } else if text == char::from(Key::Return).to_string() {
                self.action("properties");
            }
            return;
        }
        if text == char::from(Key::Return).to_string() {
            self.action("open");
        } else if text == char::from(Key::Delete).to_string() {
            let is_trash = self.tabs[self.active].path == Path::new("trash:");
            self.action(if shift || is_trash { "delete" } else { "trash" });
        } else if text == char::from(Key::F2).to_string() {
            self.action("rename");
        } else if text == char::from(Key::F5).to_string() {
            self.action("refresh");
        } else if text == char::from(Key::Escape).to_string() {
            self.invalidate_properties_metadata();
            self.tabs[self.active].selected.clear();
            self.render();
        } else {
            let tab = &mut self.tabs[self.active];
            if tab.entries.is_empty() {
                return;
            }
            let index = tab.focus.unwrap_or(0);
            let step = if self.prefs.grid_view {
                self.grid_columns
            } else {
                1
            };
            let page_step = self
                .window
                .upgrade()
                .map_or(8, |window| window.get_visible_rows().max(1) as usize)
                * step;
            let next = if text == char::from(Key::DownArrow).to_string() {
                if tab.focus.is_none() {
                    0
                } else {
                    index.saturating_add(step).min(tab.entries.len() - 1)
                }
            } else if text == char::from(Key::UpArrow).to_string() {
                index.saturating_sub(step)
            } else if text == char::from(Key::RightArrow).to_string() && self.prefs.grid_view {
                if tab.focus.is_none() {
                    0
                } else {
                    (index + 1).min(tab.entries.len() - 1)
                }
            } else if text == char::from(Key::LeftArrow).to_string() && self.prefs.grid_view {
                index.saturating_sub(1)
            } else if text == char::from(Key::Home).to_string() {
                0
            } else if text == char::from(Key::End).to_string() {
                tab.entries.len() - 1
            } else if text == char::from(Key::PageDown).to_string() {
                index.saturating_add(page_step).min(tab.entries.len() - 1)
            } else if text == char::from(Key::PageUp).to_string() {
                index.saturating_sub(page_step)
            } else {
                return;
            };
            if ctrl && !shift {
                tab.focus = Some(next);
            } else {
                tab.select(next, ctrl, shift);
            }
            self.render();
            if let Some(w) = self.window.upgrade() {
                w.invoke_reveal_entry(next as i32);
            }
        }
    }
    fn finish_operation(&mut self, report: operations::Report) {
        if let Some(id) = self.drop_operation.take() {
            if let Some(dnd) = &self.native_dnd {
                dnd.borrow_mut().complete(
                    id,
                    !report.cancelled && report.errors.is_empty() && report.skipped.is_empty(),
                );
            }
        }
        self.operation = None;
        OPERATION_TIMER.with(|t| *t.borrow_mut() = None);

        self.load();
        self.notice = Some(format!(
            "{}{}",
            translated(
                "{completed} completed · {skipped} skipped",
                &[
                    ("completed", report.completed.len().to_string()),
                    ("skipped", report.skipped.len().to_string())
                ]
            ),
            if report.cancelled {
                i18n::tr(" · cancelled").to_string()
            } else {
                String::new()
            }
        ));
        self.render();
        if !report.errors.is_empty() {
            self.error(report.errors.join("\n"));
        }
    }
    pub(super) fn message(&mut self, message: Message) {
        match message {
            Message::Content(event) => self.content_event(event),
            Message::Disk {
                request,
                path,
                result,
            } => {
                if self.disk_request == Some(request) {
                    self.disk_request = None;
                    if path == self.disk_location() {
                        self.disk_path = Some(path);
                        self.disk_usage = result.ok();
                        if let Some(window) = self.window.upgrade() {
                            self.render_disk(&window);
                        }
                    } else {
                        self.disk_refresh_due = Instant::now();
                    }
                }
            }
            Message::Loaded {
                id,
                generation,
                resolved_path,
                result,
                trash,
                search_limited,
            } => {
                if let Some(tab) = self
                    .tabs
                    .iter_mut()
                    .find(|t| t.id == id && t.generation == generation)
                {
                    tab.loading = false;
                    tab.search_limited = search_limited;
                    match result {
                        Ok(entries) => {
                            if let Some(path) = resolved_path {
                                tab.resolve_location(path);
                            }
                            tab.install_entries(entries);
                            tab.error = None;
                        }
                        Err(e) => {
                            tab.entries.clear();
                            tab.selected.clear();
                            tab.error = Some(e);
                        }
                    }
                    if tab.path == Path::new("trash:") {
                        self.trash_items.clear();
                    }
                    for item in trash {
                        self.trash_items.insert(PathBuf::from(&item.id), item);
                    }
                    self.render();
                    if self.tabs[self.active].id == id {
                        self.watch();
                    }
                }
            }
            Message::Theme(theme) => {
                self.desktop_theme = *theme;
                self.refresh_appearance();
            }
            Message::WindowAppearance(settings) => {
                self.window_appearance = *settings;
                self.refresh_appearance();
            }
            Message::History {
                history,
                report,
                redo,
            } => {
                let succeeded =
                    !report.completed.is_empty() && !report.cancelled && report.errors.is_empty();
                if !report.completed.is_empty() {
                    let tab = &mut self.tabs[self.active];
                    if tab.path != Path::new("trash:") && !tab.path.is_dir() {
                        if let Some(parent) = tab
                            .path
                            .ancestors()
                            .skip(1)
                            .find(|path| path.is_dir())
                            .map(Path::to_owned)
                        {
                            tab.navigate(parent);
                        }
                    }
                }
                self.history = history;
                self.finish_operation(report);
                if succeeded && self.dialog_kind == 0 {
                    self.notice = Some(
                        i18n::tr(if redo {
                            "Operation redone"
                        } else {
                            "Operation undone"
                        })
                        .to_string(),
                    );
                    self.render();
                }
            }
            Message::Operation(mut report) => {
                if !report.completed.is_empty() {
                    self.history.record(std::mem::take(&mut report.undo));
                }
                self.finish_operation(report);
            }
            Message::Error(e) => self.error(e),
            Message::Properties {
                request,
                paths,
                result,
            } => {
                if request == self.properties_metadata_request.load(Ordering::SeqCst) {
                    match result {
                        Ok(properties) => self.show_properties(paths, properties),
                        Err(error) => self.present_dialog(
                            4,
                            "Could not complete action",
                            error,
                            vec![],
                            String::new(),
                        ),
                    }
                }
            }
            Message::PropertiesSummary { request, summary } => {
                if self.dialog_kind == 10 && request == self.properties_request {
                    self.properties_scan = None;
                    self.properties_summary = Some(summary.clone());
                    self.render_properties(Some(&summary));
                }
            }
            Message::Apps(path, choices) => {
                let Some(w) = self.window.upgrade() else {
                    return;
                };
                self.applications = choices.applications.clone();
                self.application_path = Some(path.clone());
                let ui = w.global::<ApplicationChooser>();
                ui.set_title(i18n::tr("Open with"));
                ui.set_description(
                    format!(
                        "{} · {}",
                        path.file_name().unwrap_or_default().to_string_lossy(),
                        choices.mime
                    )
                    .into(),
                );
                ui.set_query("".into());
                ui.set_selected_index(-1);
                ui.set_always_use(false);
                ui.set_allow_always(true);
                ui.set_accept_label(i18n::tr("Open"));
                ui.set_busy(false);
                ui.set_show_all(choices.recommended.is_empty());
                self.application_choices = Some(choices);
                self.refresh_application_choices();
                self.dialog(5, "Open with", String::new(), vec![], String::new());
            }
            Message::Incoming(locations) => {
                for location in locations {
                    self.add_tab(location.folder);
                    if let Some(path) = location.selected {
                        self.tabs[self.active].selected.insert(path);
                    }
                }
            }
            Message::ClipboardError(error) => {
                self.clipboard.clear();
                self.cut = false;
                self.error(error);
            }
            Message::Clipboard {
                paths,
                cut,
                destination,
            } => {
                self.clipboard = paths;
                self.cut = cut && !self.clipboard.is_empty();
                if !self.clipboard.is_empty() && destination != Path::new("trash:") {
                    self.start_operation(
                        if self.cut { Kind::Move } else { Kind::Copy },
                        self.clipboard.clone(),
                        Some(destination),
                    );
                }
            }
        }
    }
}

struct FileManager {
    sender: mpsc::Sender<Message>,
    properties_metadata_request: Arc<AtomicU64>,
}
#[zbus::interface(name = "org.freedesktop.FileManager1")]
impl FileManager {
    async fn show_items(&self, uris: Vec<String>, _startup_id: String) -> zbus::fdo::Result<()> {
        self.send(uris, true).await
    }
    async fn show_folders(&self, uris: Vec<String>, _startup_id: String) -> zbus::fdo::Result<()> {
        self.send(uris, false).await
    }
    async fn show_item_properties(
        &self,
        uris: Vec<String>,
        _startup_id: String,
    ) -> zbus::fdo::Result<()> {
        let paths = Self::paths(uris)?;

        let request = self
            .properties_metadata_request
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1);
        let frozen = paths.clone();
        let result = tokio::task::spawn_blocking(move || describe_properties(&frozen))
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        self.sender
            .send(Message::Properties {
                request,
                paths,
                result,
            })
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }
}
impl FileManager {
    fn paths(uris: Vec<String>) -> zbus::fdo::Result<Vec<PathBuf>> {
        if uris.len() > 12 {
            return Err(zbus::fdo::Error::LimitsExceeded(
                "At most 12 locations can be opened".into(),
            ));
        }
        if uris.is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs(
                "At least one local file URI is required".into(),
            ));
        }
        uris.into_iter()
            .map(|uri| {
                use std::os::unix::ffi::OsStrExt;
                let uri = url::Url::parse(&uri)
                    .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))?;
                if uri.query().is_some() || uri.fragment().is_some() {
                    return Err(zbus::fdo::Error::InvalidArgs(
                        "File URI contains a query or fragment".into(),
                    ));
                }
                let path = uri.to_file_path().map_err(|_| {
                    zbus::fdo::Error::InvalidArgs("Only local file URIs are supported".into())
                })?;
                if path.as_os_str().as_bytes().contains(&0) {
                    return Err(zbus::fdo::Error::InvalidArgs(
                        "File URI contains a NUL byte".into(),
                    ));
                }

                Ok(path.components().collect())
            })
            .collect()
    }
    async fn send(&self, uris: Vec<String>, select: bool) -> zbus::fdo::Result<()> {
        let paths = Self::paths(uris)?;

        let locations = tokio::task::spawn_blocking(move || {
            paths
                .into_iter()
                .map(|path| {
                    let folder = if select || !path.is_dir() {
                        path.parent().unwrap_or(Path::new("/")).to_owned()
                    } else {
                        path.clone()
                    };
                    IncomingLocation {
                        folder,
                        selected: select.then_some(path),
                    }
                })
                .collect()
        })
        .await
        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        self.sender
            .send(Message::Incoming(locations))
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }
}

fn describe_properties(paths: &[PathBuf]) -> Result<Vec<FileProperties>, String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    paths
        .iter()
        .map(|path| {
            let metadata = std::fs::symlink_metadata(path)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            let file_type = metadata.file_type();
            let kind = if file_type.is_symlink() {
                "Symbolic link"
            } else if file_type.is_dir() {
                "Folder"
            } else if file_type.is_file() {
                "File"
            } else if file_type.is_socket() {
                "Socket"
            } else if file_type.is_fifo() {
                "FIFO"
            } else if file_type.is_block_device() {
                "Block device"
            } else if file_type.is_char_device() {
                "Character device"
            } else {
                "Unknown"
            };
            let link_target = if file_type.is_symlink() {
                Some(
                    std::fs::read_link(path)
                        .map_err(|error| format!("{}: {error}", path.display()))?,
                )
            } else {
                None
            };
            Ok(FileProperties {
                path: path.clone(),
                kind,
                size: metadata.len(),
                modified: metadata.modified().ok(),
                accessed: metadata.accessed().ok(),
                created: metadata.created().ok(),
                allocated: metadata.blocks().saturating_mul(512),
                original_path: None,
                deleted: None,
                permissions: metadata.mode() & 0o7777,
                owner: metadata.uid(),
                group: metadata.gid(),
                link_target,
            })
        })
        .collect()
}

fn theme_service(sender: mpsc::Sender<Message>) {
    let _task = theme_service_at(sender, rix_ipc::default_socket_path());
}
fn theme_service_at(sender: mpsc::Sender<Message>, socket: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let result = async {
                let mut events = rix_ipc::IpcClient::subscribe_to(socket.clone()).await?;
                let mut client = rix_ipc::IpcClient::connect_to(socket.clone()).await?;
                if let rix_ipc::IpcResponse::Desktop(desktop) =
                    client.send(rix_ipc::IpcRequest::GetDesktop).await?
                {
                    let _ = sender.send(Message::WindowAppearance(Box::new(
                        desktop.window_appearance,
                    )));
                }
                if let rix_ipc::IpcResponse::Theme(theme) =
                    client.send(rix_ipc::IpcRequest::GetTheme).await?
                {
                    let _ = sender.send(Message::Theme(Box::new(theme)));
                }
                loop {
                    match events.next_event().await? {
                        rix_ipc::IpcEvent::ThemeChanged(theme) => {
                            let _ = sender.send(Message::Theme(Box::new(theme)));
                        }
                        rix_ipc::IpcEvent::DesktopChanged(desktop) => {
                            let _ = sender.send(Message::WindowAppearance(Box::new(
                                desktop.window_appearance,
                            )));
                        }
                        rix_ipc::IpcEvent::IconThemeChanged(_) => {
                            if let rix_ipc::IpcResponse::Theme(theme) =
                                client.send(rix_ipc::IpcRequest::GetTheme).await?
                            {
                                let _ = sender.send(Message::Theme(Box::new(theme)));
                            }
                        }
                        _ => {}
                    }
                }
                #[allow(unreachable_code)]
                Ok::<(), rix_ipc::ClientError>(())
            }
            .await;
            if sender.send(Message::Theme(Box::new(load_theme()))).is_err() {
                break;
            }
            if sender
                .send(Message::WindowAppearance(Box::new(
                    load_desktop().window_appearance,
                )))
                .is_err()
            {
                break;
            }
            if let Err(e) = result {
                tracing::debug!(%e,"Theme IPC reconnecting");
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    })
}

pub(super) fn run() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run_until_closed());

    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}

async fn run_until_closed() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();
    let picker_request = if args.portal_picker {
        Some(rix_portal::picker::read_request(std::io::stdin())?)
    } else {
        None
    };
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("femtovg".into())
        .require_opengl_es()
        .select()?;
    slint::set_xdg_app_id(APPLICATION_ID)?;
    let window = FilesWindow::new()?;
    rix_files::keyboard::install(window.window());
    let desktop = load_desktop();
    let _ = i18n::select_language(&desktop.language);
    let prefs = Preferences::load();
    let theme = load_theme();
    theme_bindings::apply_for_app(window.global::<Theme>(), window.global::<Palette>(), &theme);
    window
        .global::<DialogBackdrop>()
        .set_radius(theme.blur_radius as f32);
    skins::install(window.global::<SkinRenderer>(), window.as_weak());
    let native_style = native_style::install(&window, &theme);
    let (sender, receiver) = mpsc::channel();
    let dirty = Arc::new(AtomicBool::new(false));
    let mut tabs = Vec::new();
    if args.trash {
        tabs.push(Tab::new(1, "trash:".into(), false, SortBy::Name));
    }
    for arg in args.paths.iter().take(12) {
        let path = local_path(arg).map_err(anyhow::Error::msg)?;
        let folder = if path.is_dir() && !args.select {
            path.clone()
        } else {
            path.parent().unwrap_or(Path::new("/")).into()
        };
        let mut tab = Tab::new(tabs.len() as u64 + 1, folder, prefs.show_hidden, prefs.sort);
        if args.select || !path.is_dir() {
            tab.selected.insert(path);
        }
        tabs.push(tab);
    }
    if let Some(request) = &picker_request {
        let picker = picker::Picker::new(request.clone());
        tabs.clear();
        let mut tab = Tab::new(1, picker.folder(), prefs.show_hidden, prefs.sort);
        if let Some(bytes) = &request.current_file {
            use std::os::unix::ffi::OsStringExt;
            tab.selected
                .insert(PathBuf::from(OsString::from_vec(bytes.clone())));
        }
        tabs.push(tab);
    }
    if tabs.is_empty() {
        tabs.push(Tab::new(1, home(), prefs.show_hidden, prefs.sort));
    }
    let app = Rc::new(RefCell::new(App::new(
        &window,
        tabs,
        prefs,
        theme,
        sender.clone(),
        dirty.clone(),
    )));
    app.borrow_mut().native_style = Some(native_style);
    if let Some(request) = picker_request {
        let picker = picker::Picker::new(request);
        picker.install(&window);
        app.borrow_mut().picker = Some(picker);
    }
    content::install(&window, &app);
    picker::install(&window, &app);
    app.borrow_mut().message(Message::WindowAppearance(Box::new(
        desktop.window_appearance,
    )));
    let weak = window.as_weak();
    let native_popup =
        native_popup::install(&window, &app.borrow().theme, move |event| {

            if let Some(window) = weak.upgrade() {
                match event {
                    native_popup::NativeEvent::Shown => {
                        if window.global::<MenuHost>().get_active_owner() == 0 {
                            window.set_native_popup_active(true);
                        }
                    }
                    native_popup::NativeEvent::Dismissed => {
                        if !menu_host::deliver(&window, String::new(), false) {
                            window.invoke_dismiss_menu();
                        }
                    }
                    native_popup::NativeEvent::Activated(action) => {
                        if !menu_host::deliver(&window, action.clone(), false) {
                            window.invoke_activate_menu(action.into());
                        }
                    }
                    native_popup::NativeEvent::Unavailable => {
                        if !menu_host::deliver(&window, String::new(), true) {
                            window.set_native_popup_active(false);
                            window.invoke_focus_inline_menu();
                        }
                    }
                    native_popup::NativeEvent::ReleaseParentPointer(position, button) => {
                        window.window().dispatch_event(
                            slint::platform::WindowEvent::PointerReleased { position, button },
                        );
                    }
                }
            }
        });
    app.borrow_mut().native_popup = Some(native_popup.clone());
    menu_host::install(&window, native_popup.clone());
    dialog_backdrop::install(&window);
    let popup = native_popup.clone();
    let weak = window.as_weak();
    window.on_show_native_menu(move |items, title, x, y, width, height| {
        let Some(window) = weak.upgrade() else {
            return false;
        };
        menu_host::deliver(&window, String::new(), false);

        popup.borrow_mut().close();
        popup.borrow_mut().open(native_popup::MenuRequest {
            items: items
                .iter()
                .map(native_popup::OwnedMenuItem::from)
                .collect(),
            title: title.to_string(),
            x: x.round() as i32,
            y: y.round() as i32,
            width: width.round().max(1.) as u32,
            height: height.round().max(1.) as u32,
            scale: window.window().scale_factor(),
            radius: window.global::<Theme>().get_context_menu_radius(),
        })
    });
    let weak = window.as_weak();
    window.on_close_native_menu(move || {
        if weak
            .upgrade()
            .is_some_and(|window| window.global::<MenuHost>().get_active_owner() == 0)
        {
            native_popup.borrow_mut().close();
        }
    });
    file_drag::install(&window, &app);
    {
        let app = app.clone();
        window.on_action(move |action| app.borrow_mut().action(&action));
    }
    {
        let app = app.clone();
        window.on_navigate(move |value| {
            let path = if value == "trash:" {
                Ok(PathBuf::from("trash:"))
            } else if value == "~" {
                Ok(home())
            } else if let Some(s) = value.strip_prefix("~/") {
                Ok(home().join(s))
            } else if !Path::new(value.as_str()).is_absolute() && !value.contains("://") {
                let current = app.borrow().tabs[app.borrow().active].path.clone();
                let current = if current == Path::new("trash:") {
                    home()
                } else {
                    current
                };
                local_path(current.join(value.as_str()).as_os_str())
            } else {
                local_path(value.as_str().as_ref())
            };
            match path {
                Ok(p) => app.borrow_mut().navigate(p),
                Err(e) => app.borrow_mut().error(e),
            }
        });
    }
    {
        let app = app.clone();
        window.on_select_entry(move |i, ctrl, shift| {
            let mut a = app.borrow_mut();
            let active = a.active;
            if i >= 0 {
                let multiple = a
                    .picker
                    .as_ref()
                    .is_none_or(|picker| picker.request.multiple);
                a.tabs[active].select(i as usize, ctrl && multiple, shift && multiple);
                a.render();
            }
        });
    }
    {
        let app = app.clone();
        window.on_context_entry(move |i| {
            if let Ok(index) = usize::try_from(i) {
                app.borrow_mut().context_entry(index);
            }
        });
    }
    {
        let app = app.clone();
        window.on_activate_entry(move |i| app.borrow_mut().open(i as usize));
    }
    {
        let app = app.clone();
        window.on_switch_tab(move |i| {
            let mut a = app.borrow_mut();
            if i >= 0 && (i as usize) < a.tabs.len() {
                a.invalidate_properties_metadata();
                a.active = i as usize;
                if a.tabs[a.active].entries.is_empty() {
                    a.load();
                } else {
                    a.render();
                    a.watch();
                }
            }
        });
    }
    {
        let app = app.clone();
        window.on_close_tab(move |i| app.borrow_mut().close_tab(i as usize));
    }
    {
        let app = app.clone();
        window.on_open_place(move |i| {
            let mut a = app.borrow_mut();
            if let Some(p) = a.places.get(i as usize) {
                let path = p.path.clone();
                a.navigate(path);
            }
        });
    }
    {
        let app = app.clone();
        window.on_open_disk(move || app.borrow_mut().open_disk());
    }
    {
        let app = app.clone();
        window.on_unpin_place(move |index| {
            if let Ok(index) = usize::try_from(index) {
                app.borrow_mut().unpin_place(index);
            }
        });
    }
    {
        let app = app.clone();
        window.on_action_on_place(move |action, index| {
            if let Ok(index) = usize::try_from(index) {
                app.borrow_mut().action_on_place(&action, index);
            }
        });
    }
    {
        let app = app.clone();
        window.on_action_on_tab(move |action, index| {
            if let Ok(index) = usize::try_from(index) {
                app.borrow_mut().action_on_tab(&action, index);
            }
        });
    }
    {
        let state = app.clone();
        window.on_grid_columns_changed(move |columns| {
            state.borrow_mut().update_grid_columns(columns)
        });
        let columns = window.get_grid_columns();
        app.borrow_mut().update_grid_columns(columns);
    }
    window.on_next_menu_index(next_menu_index);
    window.on_menu_rows_height(menu_rows_height);
    window.on_menu_items_width(menu_width);
    window.on_place_at(place_at);
    {
        let app = app.clone();
        window.on_begin_rectangle(move |control, shift| {
            app.borrow_mut().begin_rectangle(control, shift)
        });
    }
    {
        let app = app.clone();
        window.on_update_rectangle(move |x1, y1, x2, y2, width, height, gap, columns| {
            app.borrow_mut().update_rectangle(
                rix_files::selection::SelectionRect { x1, y1, x2, y2 },
                rix_files::selection::EntryLayout {
                    width,
                    height,
                    gap,
                    columns: usize::try_from(columns).unwrap_or(0),
                },
            );
        });
    }
    {
        let app = app.clone();
        window.on_finish_rectangle(move |cancelled| app.borrow_mut().finish_rectangle(cancelled));
    }
    {
        let app = app.clone();
        window.on_search_changed(move |query| {
            let mut a = app.borrow_mut();
            let active = a.active;
            a.tabs[active].query = query.into();
            a.tabs[active].cancellation.store(true, Ordering::Relaxed);
            a.search_due = Some(Instant::now() + Duration::from_millis(250));
        });
    }
    {
        let app = app.clone();
        window.on_confirm_dialog(move |value| app.borrow_mut().confirm(value.into()));
    }
    {
        let app = app.clone();
        window.on_dismiss_dialog(move || app.borrow_mut().dismiss());
    }
    {
        let app = app.clone();
        window
            .global::<PropertiesView>()
            .on_copy_paths(move || app.borrow().copy_property_paths());
    }
    {
        let app = app.clone();
        window.on_keyboard(move |text, ctrl, shift, alt| {
            app.borrow_mut().key(&text, ctrl, shift, alt)
        });
    }
    {
        let app = app.clone();
        window.on_choose_application(move |i| {
            let id = app
                .borrow()
                .applications
                .get(i as usize)
                .map(|application| application.id.clone());
            if let Some(id) = id {
                app.borrow_mut().choose_application(id, false);
            }
        });
    }
    {
        let app = app.clone();
        window
            .global::<ApplicationChooser>()
            .on_search(move |_| app.borrow_mut().refresh_application_choices());
    }
    {
        let app = app.clone();
        window
            .global::<ApplicationChooser>()
            .on_show_all_changed(move |_| app.borrow_mut().refresh_application_choices());
    }
    {
        let app = app.clone();
        window
            .global::<ApplicationChooser>()
            .on_confirm(move |id, always| app.borrow_mut().choose_application(id.into(), always));
    }
    {
        let app = app.clone();
        window
            .global::<ApplicationChooser>()
            .on_cancelled(move || app.borrow_mut().dismiss());
    }
    let service_sender = sender.clone();
    let service_properties_request = app.borrow().properties_metadata_request.clone();
    if !args.portal_picker {
        tokio::spawn(async move {
            match zbus::connection::Builder::session()
                .and_then(|b| b.name("org.freedesktop.FileManager1"))
                .and_then(|b| {
                    b.serve_at(
                        "/org/freedesktop/FileManager1",
                        FileManager {
                            sender: service_sender,
                            properties_metadata_request: service_properties_request,
                        },
                    )
                }) {
                Ok(builder) => {
                    if let Ok(_connection) = builder.build().await {
                        std::future::pending::<()>().await;
                    }
                }
                Err(e) => tracing::debug!(%e,"FileManager1 is provided by another application"),
            }
        });
    }
    theme_service(sender);
    let timer = slint::Timer::default();
    let state = app.clone();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(30),
        move || {
            let mut app = state.borrow_mut();
            for message in receiver.try_iter().take(32) {
                app.message(message);
            }
            if dirty.swap(false, Ordering::Relaxed) {
                app.watch_due = Some(Instant::now() + Duration::from_millis(160));
            }
            let active_loading = app.tabs.get(app.active).is_some_and(|t| t.loading);
            if app.search_due.is_some_and(|t| Instant::now() >= t)
                || (app.watch_due.is_some_and(|t| Instant::now() >= t) && !active_loading)
            {
                app.watch_due = None;
                app.load();
            }
            app.refresh_disk();
        },
    );
    app.borrow_mut().load();
    window.invoke_focus_files();
    if args.view {
        if let Some(path) = args.paths.first().and_then(|path| local_path(path).ok()) {
            if rix_files::archive::supported(&path) {
                app.borrow_mut().open_archive(path, String::new());
            } else {
                app.borrow_mut().preview(path);
            }
        }
    }
    let picker_parent = app
        .borrow()
        .picker
        .as_ref()
        .map(|picker| (picker.request.parent_window.clone(), picker.request.modal));
    let parent_state = app.clone();
    let _picker_parent = picker_parent.and_then(|(parent, modal)| {
        picker_parent::install(&window, &parent, modal, move || {
            if let Some(picker) = &mut parent_state.borrow_mut().picker {
                picker.response = Some(rix_portal::picker::PickerResponse::cancelled());
            }
            let _ = slint::quit_event_loop();
        })
    });
    window.show()?;
    if let Some(style) = &app.borrow().native_style {
        style.borrow_mut().prepare();
    }
    window.run()?;
    if args.portal_picker {
        let response = app
            .borrow_mut()
            .picker
            .as_mut()
            .and_then(|picker| picker.response.take())
            .unwrap_or_else(rix_portal::picker::PickerResponse::cancelled);
        rix_portal::picker::write_response(std::io::stdout(), &response)?;
    }
    for tab in &app.borrow().tabs {
        tab.cancellation.store(true, Ordering::Relaxed);
    }
    if let Some(cancel) = &app.borrow().operation {
        cancel.store(true, Ordering::Relaxed);
    }
    app.borrow_mut().cancel_properties_scan();
    Ok(())
}

#[cfg(test)]
#[path = "controller_tests.rs"]
pub(super) mod tests;

#[cfg(test)]
#[path = "undo_tests.rs"]
mod undo_tests;

#[cfg(test)]
#[path = "properties_tests.rs"]
mod properties_tests;

#[cfg(test)]
#[path = "sidebar_tests.rs"]
mod sidebar_tests;

#[cfg(test)]
#[path = "menu_tests.rs"]
mod menu_tests;

#[cfg(test)]
#[path = "creation_tests.rs"]
mod creation_tests;
