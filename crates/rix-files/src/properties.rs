use super::*;
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;

#[derive(Clone, Debug, Default)]
pub(super) struct Summary {
    pub bytes: u64,
    pub allocated: u64,
    pub files: u64,
    pub folders: u64,
    pub complete: bool,
    pub unreadable: bool,
}

pub(super) fn scan(
    items: &[FileProperties],
    cancel: &AtomicBool,
    limit: u64,
    budget: Duration,
) -> Summary {
    let mut summary = Summary {
        complete: true,
        ..Summary::default()
    };
    let deadline = Instant::now() + budget;
    if cancel.load(Ordering::Relaxed) || limit == 0 || Instant::now() >= deadline {
        summary.complete = false;
        return summary;
    }

    let selected_folders: HashSet<_> = items
        .iter()
        .filter(|item| item.kind == "Folder")
        .map(|item| item.path.as_path())
        .collect();
    let mut seen_paths = HashSet::new();
    let mut pending: Vec<_> = items
        .iter()
        .filter(|item| {
            seen_paths.insert(item.path.clone())
                && !item
                    .path
                    .ancestors()
                    .skip(1)
                    .any(|parent| selected_folders.contains(parent))
        })
        .map(|item| item.path.clone())
        .collect();
    let mut seen_storage = HashSet::new();
    let mut visited = 0;
    while let Some(path) = pending.pop() {
        if cancel.load(Ordering::Relaxed) || visited >= limit || Instant::now() >= deadline {
            summary.complete = false;
            break;
        }
        visited += 1;
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                summary.complete = false;
                summary.unreadable = true;
                continue;
            }
        };
        if seen_storage.insert((metadata.dev(), metadata.ino())) {
            summary.allocated = summary
                .allocated
                .saturating_add(metadata.blocks().saturating_mul(512));
        }
        if metadata.is_dir() {
            summary.folders += 1;
            let entries = match std::fs::read_dir(&path) {
                Ok(entries) => entries,
                Err(_) => {
                    summary.complete = false;
                    summary.unreadable = true;
                    continue;
                }
            };
            for entry in entries {
                if cancel.load(Ordering::Relaxed)
                    || pending.len() as u64 + visited >= limit
                    || Instant::now() >= deadline
                {
                    summary.complete = false;
                    break;
                }
                match entry {
                    Ok(entry) => pending.push(entry.path()),
                    Err(_) => {
                        summary.complete = false;
                        summary.unreadable = true;
                    }
                }
            }
        } else {
            summary.files += 1;
            summary.bytes = summary.bytes.saturating_add(metadata.len());
        }
    }
    summary
}

fn detail(label: &str, value: impl Into<String>) -> PropertyDetail {
    PropertyDetail {
        label: i18n::tr(label),
        value: value.into().into(),
    }
}

fn common<T: Eq + Copy>(
    items: &[FileProperties],
    value: impl Fn(&FileProperties) -> T,
) -> Option<T> {
    let first = value(items.first()?);
    items
        .iter()
        .all(|item| value(item) == first)
        .then_some(first)
}

fn bytes(value: u64) -> String {
    translated(
        "{size} ({bytes} bytes)",
        &[("size", size(value)), ("bytes", value.to_string())],
    )
}

fn timestamp(
    items: &[FileProperties],
    value: impl Fn(&FileProperties) -> Option<SystemTime>,
) -> String {
    match common(items, value) {
        Some(Some(value)) => date(Some(value)),
        Some(None) => i18n::tr("Unavailable").to_string(),
        None => i18n::tr("Varies").to_string(),
    }
}

impl App {
    pub(super) fn invalidate_properties_metadata(&self) {
        self.properties_metadata_request
            .fetch_add(1, Ordering::SeqCst);
    }

    pub(super) fn cancel_properties_scan(&mut self) {
        if let Some(cancel) = self.properties_scan.take() {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    pub(super) fn show_properties(&mut self, paths: Vec<PathBuf>, items: Vec<FileProperties>) {
        if items.is_empty() {
            return;
        }
        self.present_dialog(10, "Properties", String::new(), paths, String::new());
        self.properties_items = items;
        self.properties_summary = None;
        self.render_properties(None);
        if let Some(window) = self.window.upgrade() {
            window.invoke_focus_dialog();
        }
        let items = self.properties_items.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.properties_scan = Some(cancel.clone());
        self.properties_request = self.properties_request.wrapping_add(1);
        let request = self.properties_request;
        let sender = self.sender.clone();
        tokio::task::spawn_blocking(move || {
            let summary = scan(&items, &cancel, 200_000, Duration::from_secs(3));
            if !cancel.load(Ordering::Relaxed) {
                let _ = sender.send(Message::PropertiesSummary { request, summary });
            }
        });
    }

    pub(super) fn render_properties(&mut self, summary: Option<&Summary>) {
        if self.dialog_kind != 10 || self.properties_items.is_empty() {
            return;
        }
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let items = self.properties_items.clone();
        let first = &items[0];
        let ui = window.global::<PropertiesView>();
        let multiple = items.len() > 1;
        ui.set_multiple(multiple);
        ui.set_name(if multiple {
            translated(
                "{count} selected items",
                &[("count", items.len().to_string())],
            )
            .into()
        } else {
            first
                .path
                .file_name()
                .unwrap_or(first.path.as_os_str())
                .to_string_lossy()
                .into_owned()
                .into()
        });
        let kind = common(&items, |item| item.kind).unwrap_or("Mixed types");
        let type_name = if !multiple && kind == "File" {
            first
                .path
                .extension()
                .and_then(|extension| extension.to_str())
                .filter(|extension| !extension.is_empty())
                .map(|extension| entry_kind(&format!("{} file", extension.to_uppercase())))
                .unwrap_or_else(|| i18n::tr(kind).to_string())
        } else {
            i18n::tr(kind).to_string()
        };
        ui.set_description(type_name.clone().into());
        let icon_names = if multiple {
            vec!["edit-select-all".into(), "folder".into()]
        } else if first.kind == "Symbolic link" {
            vec!["emblem-symbolic-link".into(), "text-x-generic".into()]
        } else {
            rix_files::file_icons::names(
                &first.path,
                first.kind == "Folder",
                first.permissions & 0o111 != 0,
            )
        };
        ui.set_icon(self.icon_candidates(&icon_names));
        let location = if multiple {
            let parent = first.path.parent();
            if items.iter().all(|item| item.path.parent() == parent) {
                parent
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default()
            } else {
                i18n::tr("Multiple locations").to_string()
            }
        } else {
            first.path.to_string_lossy().into_owned()
        };
        ui.set_location(location.into());
        ui.set_link_target(if multiple {
            String::new().into()
        } else {
            first
                .link_target
                .as_deref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default()
                .into()
        });
        let has_folders = items.iter().any(|item| item.kind == "Folder");
        let initial_bytes = items
            .iter()
            .filter(|item| item.kind != "Folder")
            .fold(0u64, |total, item| total.saturating_add(item.size));
        let initial_allocated = items
            .iter()
            .fold(0u64, |total, item| total.saturating_add(item.allocated));
        let mut details = vec![
            detail("Type", type_name),
            detail(
                "Size",
                summary.map(|value| bytes(value.bytes)).unwrap_or_else(|| {
                    if has_folders {
                        i18n::tr("Calculating…").to_string()
                    } else {
                        bytes(initial_bytes)
                    }
                }),
            ),
            detail(
                "Size on disk",
                summary
                    .map(|value| bytes(value.allocated))
                    .unwrap_or_else(|| {
                        if has_folders {
                            i18n::tr("Calculating…").to_string()
                        } else {
                            bytes(initial_allocated)
                        }
                    }),
            ),
        ];
        if let Some(summary) = summary.filter(|_| has_folders || multiple) {
            let folders = summary
                .folders
                .saturating_sub(u64::from(!multiple && first.kind == "Folder"));
            details.push(detail(
                "Contents",
                translated(
                    "{files} files and links · {folders} folders",
                    &[
                        ("files", summary.files.to_string()),
                        ("folders", folders.to_string()),
                    ],
                ),
            ));
        }
        if !multiple {
            if let Some(original) = &first.original_path {
                details.push(detail(
                    "Original location",
                    original.to_string_lossy().into_owned(),
                ));
            }
        }
        ui.set_details(model(details));
        ui.set_note(if let Some(summary) = summary.filter(|summary| !summary.complete) {
            if summary.unreadable { i18n::tr("Some items could not be read. Sizes and counts are partial.") }
            else { i18n::tr("Large folder: sizes and counts shown so far. Counting stopped to keep the app responsive.") }
        } else if has_folders && summary.is_none() {
            i18n::tr("Counting folder contents… Symbolic links are not followed.")
        } else if has_folders {
            i18n::tr("Symbolic links are not followed.")
        } else {
            String::new().into()
        });
        let mut dates = vec![
            detail("Modified", timestamp(&items, |item| item.modified)),
            detail("Accessed", timestamp(&items, |item| item.accessed)),
            detail("Created", timestamp(&items, |item| item.created)),
        ];
        if !multiple && first.deleted.is_some() {
            dates.push(detail("Deleted", date(first.deleted)));
        }
        ui.set_dates(model(dates));
        let mode = common(&items, |item| item.permissions);
        let meaningful_mode =
            mode.filter(|_| items.iter().all(|item| item.kind != "Symbolic link"));
        ui.set_permissions(model(
            meaningful_mode
                .map(|mode| {
                    [("Owner", 6), ("Group", 3), ("Others", 0)]
                        .into_iter()
                        .map(|(label, shift)| PropertyPermission {
                            label: i18n::tr(label),
                            read: mode & (4 << shift) != 0,
                            write: mode & (2 << shift) != 0,
                            execute: mode & (1 << shift) != 0,
                        })
                        .collect()
                })
                .unwrap_or_default(),
        ));
        ui.set_permission_note(if items.iter().any(|item| item.kind == "Symbolic link") {
            i18n::tr("Access to a symbolic link depends on its target and parent folders.")
        } else if mode.is_none() {
            i18n::tr("Permissions vary between selected items.")
        } else {
            i18n::tr("These are the Unix mode permissions. Access can also depend on ACLs and parent folders.")
        });
        let id = |value: Option<u32>| {
            value
                .map(|value| value.to_string())
                .unwrap_or_else(|| i18n::tr("Varies").to_string())
        };
        ui.set_ownership(model(vec![
            detail("Owner ID", id(common(&items, |item| item.owner))),
            detail("Group ID", id(common(&items, |item| item.group))),
            detail(
                "Mode",
                mode.map(|mode| format!("{:04o} · {}", mode, symbolic_mode(mode)))
                    .unwrap_or_else(|| i18n::tr("Varies").to_string()),
            ),
        ]));
    }

    pub(super) fn copy_property_paths(&self) {
        if self.dialog_kind != 10 {
            return;
        }
        let paths = self.dialog_paths.clone();
        let sender = self.sender.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(error) =
                rix_files::clipboard::path_text(&paths).and_then(rix_files::clipboard::publish_text)
            {
                let _ = sender.send(Message::Error(error));
            }
        });
    }
}

fn symbolic_mode(mode: u32) -> String {
    let mut result = String::new();
    for (mask, character) in [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ] {
        result.push(if mode & mask != 0 { character } else { '-' });
    }
    let mut bytes = result.into_bytes();
    if mode & 0o4000 != 0 {
        bytes[2] = if mode & 0o100 != 0 { b's' } else { b'S' };
    }
    if mode & 0o2000 != 0 {
        bytes[5] = if mode & 0o010 != 0 { b's' } else { b'S' };
    }
    if mode & 0o1000 != 0 {
        bytes[8] = if mode & 0o001 != 0 { b't' } else { b'T' };
    }
    String::from_utf8(bytes).expect("Permission symbols are ASCII")
}
