use super::*;
use rix_portal::picker::{PickerMode, PickerRequest, PickerResponse};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
pub(super) struct Picker {
    pub request: PickerRequest,
    pub response: Option<PickerResponse>,
    pub filter: Option<rix_portal::picker::Filter>,
    pub choices: Vec<(String, String)>,
    pub pending_paths: Option<Vec<PathBuf>>,
}
impl Picker {
    pub fn new(request: PickerRequest) -> Self {
        let filter = request
            .current_filter
            .clone()
            .or_else(|| request.filters.first().cloned());
        let choices = request
            .choices
            .iter()
            .map(|choice| {
                (
                    choice.id.clone(),
                    if !choice.selected.is_empty() {
                        choice.selected.clone()
                    } else if choice.options.is_empty() {
                        "false".into()
                    } else {
                        choice.options[0].0.clone()
                    },
                )
            })
            .collect();
        Self {
            request,
            response: None,
            filter,
            choices,
            pending_paths: None,
        }
    }
    pub fn folder(&self) -> PathBuf {
        self.request
            .current_file
            .as_ref()
            .map(|bytes| PathBuf::from(OsString::from_vec(bytes.clone())))
            .and_then(|path| path.parent().map(Path::to_owned))
            .or_else(|| {
                self.request
                    .current_folder
                    .as_ref()
                    .map(|bytes| PathBuf::from(OsString::from_vec(bytes.clone())))
            })
            .filter(|path| path.is_dir())
            .unwrap_or_else(home)
    }
    pub fn install(&self, window: &FilesWindow) {
        let ui = window.global::<FilePicker>();
        ui.set_active(true);
        ui.set_saving(self.request.mode != PickerMode::Open);
        ui.set_directory(self.request.directory || self.request.mode == PickerMode::SaveFiles);
        ui.set_title(if self.request.title.is_empty() {
            i18n::tr(if self.request.mode == PickerMode::Open {
                "Open file"
            } else {
                "Save file"
            })
        } else {
            self.request.title.clone().into()
        });
        ui.set_accept_label(if self.request.accept_label.is_empty() {
            i18n::tr(if self.request.mode == PickerMode::Open {
                "Open"
            } else {
                "Save"
            })
        } else {
            self.request.accept_label.replace('_', "").into()
        });
        ui.set_name(
            self.request
                .current_file
                .as_ref()
                .and_then(|bytes| Path::new(std::ffi::OsStr::from_bytes(bytes)).file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.request.current_name.clone())
                .into(),
        );
        ui.set_summary(if self.request.mode == PickerMode::SaveFiles {
            format!("{} files", self.request.files.len()).into()
        } else if self.request.mode == PickerMode::Save {
            i18n::tr("Choose where to save the file")
        } else {
            i18n::tr(if self.request.directory {
                "Choose a folder"
            } else if self.request.multiple {
                "Choose one or more files"
            } else {
                "Choose a file"
            })
        });
        ui.set_filters(model(
            self.request
                .filters
                .iter()
                .map(|filter| PickerOption {
                    label: filter.name.clone().into(),
                    value: filter.name.clone().into(),
                })
                .collect(),
        ));
        ui.set_selected_filter(
            self.filter
                .as_ref()
                .and_then(|filter| self.request.filters.iter().position(|item| item == filter))
                .map_or(-1, |index| index as i32),
        );
        ui.set_choices(model(
            self.request
                .choices
                .iter()
                .map(|choice| {
                    let options: Vec<_> = if choice.options.is_empty() {
                        vec![
                            PickerOption {
                                label: i18n::tr("No"),
                                value: "false".into(),
                            },
                            PickerOption {
                                label: i18n::tr("Yes"),
                                value: "true".into(),
                            },
                        ]
                    } else {
                        choice
                            .options
                            .iter()
                            .map(|(id, label)| PickerOption {
                                label: label.clone().into(),
                                value: id.clone().into(),
                            })
                            .collect()
                    };
                    let selected = self
                        .choices
                        .iter()
                        .find(|(id, _)| id == &choice.id)
                        .and_then(|(_, value)| {
                            options.iter().position(|item| item.value == value.as_str())
                        })
                        .unwrap_or(0);
                    PickerChoice {
                        label: choice.label.clone().into(),
                        options: model(options),
                        selected: selected as i32,
                    }
                })
                .collect(),
        ));
    }
}
pub(super) struct Matcher {
    directory: bool,
    unrestricted: bool,
    globs: Vec<globset::GlobMatcher>,
    mime_patterns: Vec<String>,
    detector: Option<opening::MimeDetector>,
}
impl Matcher {
    pub fn new(request: &PickerRequest, filter: Option<&rix_portal::picker::Filter>) -> Self {
        let mut globs = Vec::new();
        let mut mime_patterns = Vec::new();
        if let Some(filter) = filter {
            for (kind, pattern) in &filter.rules {
                if *kind == 0 {
                    if let Ok(glob) = globset::GlobBuilder::new(pattern)
                        .literal_separator(false)
                        .backslash_escape(false)
                        .build()
                    {
                        globs.push(glob.compile_matcher());
                    }
                } else if *kind == 1 {
                    mime_patterns.push(pattern.clone());
                }
            }
        }
        let detector =
            (!request.directory && !mime_patterns.is_empty()).then(opening::MimeDetector::new);
        Self {
            directory: request.directory,
            unrestricted: filter.is_none(),
            globs,
            mime_patterns,
            detector,
        }
    }
    pub fn matches(&self, entry: &Entry) -> bool {
        self.matches_using(entry, || self.detector.as_ref()?.mime(&entry.path).ok())
    }
    fn matches_using(&self, entry: &Entry, detect: impl FnOnce() -> Option<String>) -> bool {
        if entry.is_dir {
            return true;
        }
        if self.directory {
            return false;
        }
        if self.unrestricted || self.globs.iter().any(|glob| glob.is_match(&entry.name)) {
            return true;
        }
        if self.mime_patterns.is_empty() {
            return false;
        }
        let Some(mime) = detect() else {
            return false;
        };
        self.mime_patterns.iter().any(|pattern| {
            mime == *pattern
                || pattern.strip_suffix("/*").is_some_and(|prefix| {
                    mime.strip_prefix(prefix)
                        .is_some_and(|rest| rest.starts_with('/'))
                })
        })
    }
    pub fn filter(&self, entries: &mut Vec<Entry>, cancel: &AtomicBool) -> bool {
        let mut cancelled = false;
        entries.retain(|entry| {
            if cancelled || cancel.load(Ordering::Relaxed) {
                cancelled = true;
                return false;
            }
            self.matches(entry)
        });
        !cancelled && !cancel.load(Ordering::Relaxed)
    }
}
impl App {
    pub(super) fn accept_picker(&mut self, confirmed: bool) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let Some(picker) = &mut self.picker else {
            return;
        };
        let tab = &self.tabs[self.active];
        if tab.loading {
            return;
        }
        let result = (|| -> anyhow::Result<Vec<PathBuf>> {
            if confirmed {
                return picker
                    .pending_paths
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("Save destination expired"));
            }
            match picker.request.mode {
                PickerMode::Open => {
                    let selected = tab.selected_paths();
                    let paths = if picker.request.directory && selected.is_empty() {
                        vec![tab.path.clone()]
                    } else {
                        selected
                    };
                    anyhow::ensure!(!paths.is_empty(), "Choose a file before opening");
                    Ok(paths)
                }
                PickerMode::Save => {
                    let name = window.global::<FilePicker>().get_name().to_string();
                    anyhow::ensure!(
                        !name.is_empty()
                            && name != "."
                            && name != ".."
                            && !name.contains(['/', '\0']),
                        "Enter a valid file name"
                    );
                    let original = picker.request.current_file.as_ref().and_then(|bytes| {
                        Path::new(std::ffi::OsStr::from_bytes(bytes)).file_name()
                    });
                    let basename = original
                        .filter(|original| original.to_string_lossy() == name)
                        .map(std::ffi::OsStr::to_os_string)
                        .unwrap_or_else(|| OsString::from(name));
                    Ok(vec![tab.path.join(basename)])
                }
                PickerMode::SaveFiles => Ok(picker
                    .request
                    .files
                    .iter()
                    .map(|bytes| tab.path.join(OsString::from_vec(bytes.clone())))
                    .collect()),
            }
        })();
        let paths = match result {
            Ok(paths) => paths,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        };
        if !confirmed
            && picker.request.mode != PickerMode::Open
            && paths.iter().any(|path| path.exists())
        {
            picker.pending_paths = Some(paths.clone());
            self.dialog(
                8,
                "Replace existing file?",
                i18n::tr(
                    "The selected destination already exists. Allow the application to replace it?",
                )
                .to_string(),
                paths,
                String::new(),
            );
            return;
        }
        let response = PickerResponse {
            response: 0,
            uris: paths
                .iter()
                .filter_map(|path| rix_portal::picker::path_to_uri(path).ok())
                .collect(),
            current_filter: picker.filter.clone(),
            choices: picker.choices.clone(),
        };
        match picker.request.validate_response(&response) {
            Ok(response) => {
                picker.response = Some(response);
                let _ = slint::quit_event_loop();
            }
            Err(error) => self.error(error.to_string()),
        }
    }
}
pub(super) fn install(window: &FilesWindow, app: &Rc<RefCell<App>>) {
    for cancel in [false, true] {
        let app = app.clone();
        if cancel {
            window.global::<FilePicker>().on_cancel(move || {
                if let Some(picker) = &mut app.borrow_mut().picker {
                    picker.response = Some(PickerResponse::cancelled());
                }
                let _ = slint::quit_event_loop();
            });
        } else {
            window
                .global::<FilePicker>()
                .on_accept(move || app.borrow_mut().accept_picker(false));
        }
    }
    let state = app.clone();
    window
        .global::<FilePicker>()
        .on_change_filter(move |index| {
            let mut app = state.borrow_mut();
            if let Some(picker) = &mut app.picker {
                picker.filter = picker.request.filters.get(index.max(0) as usize).cloned();
            }
            window_filter(&app, index);
            app.load();
        });
    let state = app.clone();
    window
        .global::<FilePicker>()
        .on_change_choice(move |index, option| {
            let mut app = state.borrow_mut();
            let Some(window) = app.window.upgrade() else {
                return;
            };
            let ui = window.global::<FilePicker>();
            let Some(mut row) = ui.get_choices().row_data(index.max(0) as usize) else {
                return;
            };
            let Some(value) = row.options.row_data(option.max(0) as usize) else {
                return;
            };
            if let Some(picker) = &mut app.picker {
                if let Some(choice) = picker.choices.get_mut(index.max(0) as usize) {
                    choice.1 = value.value.to_string();
                    row.selected = option;
                    if let Some(model) = ui
                        .get_choices()
                        .as_any()
                        .downcast_ref::<VecModel<PickerChoice>>()
                    {
                        model.set_row_data(index as usize, row);
                    }
                }
            }
        });
}
fn window_filter(app: &App, index: i32) {
    if let Some(window) = app.window.upgrade() {
        window.global::<FilePicker>().set_selected_filter(index);
    }
}

#[cfg(test)]
#[path = "picker_tests.rs"]
mod tests;
