use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_REQUEST_BYTES: usize = 256 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub const MAX_FILES: usize = 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PickerMode {
    Open,
    Save,
    SaveFiles,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Filter {
    pub name: String,
    pub rules: Vec<(u32, String)>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Choice {
    pub id: String,
    pub label: String,

    pub options: Vec<(String, String)>,
    pub selected: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PickerRequest {
    pub version: u32,
    pub mode: PickerMode,
    pub app_id: String,
    pub title: String,
    pub accept_label: String,
    pub parent_window: String,
    pub modal: bool,
    pub multiple: bool,
    pub directory: bool,

    pub current_folder: Option<Vec<u8>>,

    pub current_file: Option<Vec<u8>>,
    pub current_name: String,

    pub files: Vec<Vec<u8>>,
    pub filters: Vec<Filter>,
    pub current_filter: Option<Filter>,
    pub choices: Vec<Choice>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PickerResponse {

    pub response: u32,
    pub uris: Vec<String>,
    pub current_filter: Option<Filter>,
    pub choices: Vec<(String, String)>,
}

impl PickerResponse {
    pub fn cancelled() -> Self {
        Self {
            response: 1,
            uris: vec![],
            current_filter: None,
            choices: vec![],
        }
    }
    pub fn failed() -> Self {
        Self {
            response: 2,
            ..Self::cancelled()
        }
    }
}

pub fn read_request(mut input: impl Read) -> anyhow::Result<PickerRequest> {
    let mut bytes = Vec::new();
    input
        .by_ref()
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= MAX_REQUEST_BYTES,
        "File chooser request is too large"
    );
    let request: PickerRequest = serde_json::from_slice(&bytes)?;
    request.validate()?;
    Ok(request)
}

pub fn write_response(mut output: impl Write, response: &PickerResponse) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(response)?;
    anyhow::ensure!(
        bytes.len() < MAX_RESPONSE_BYTES,
        "File chooser response is too large"
    );
    output.write_all(&bytes)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

pub fn path_to_uri(path: &Path) -> anyhow::Result<String> {
    anyhow::ensure!(
        path.is_absolute() && !path.as_os_str().as_bytes().contains(&0),
        "A local absolute path is required"
    );
    url::Url::from_file_path(path)
        .map(|uri| uri.to_string())
        .map_err(|_| anyhow::anyhow!("Cannot encode file path"))
}

pub fn uri_to_path(uri: &str) -> anyhow::Result<PathBuf> {
    let uri = url::Url::parse(uri)?;
    anyhow::ensure!(
        uri.scheme() == "file"
            && matches!(uri.host_str(), None | Some("") | Some("localhost"))
            && uri.query().is_none()
            && uri.fragment().is_none()
            && uri.username().is_empty()
            && uri.password().is_none()
            && uri.port().is_none(),
        "Only local file URIs are supported"
    );
    let path = uri
        .to_file_path()
        .map_err(|_| anyhow::anyhow!("Cannot decode file URI"))?;
    anyhow::ensure!(
        path.is_absolute() && !path.as_os_str().as_bytes().contains(&0),
        "Invalid file URI path"
    );
    Ok(path)
}

fn text(value: &str, maximum: usize) -> anyhow::Result<()> {
    anyhow::ensure!(
        value.len() <= maximum && !value.contains('\0'),
        "Invalid or oversized file chooser text"
    );
    Ok(())
}

fn path_bytes(value: &[u8], basename: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.is_empty() && value.len() <= 4096 && !value.contains(&0),
        "Invalid file chooser path"
    );
    if basename {
        anyhow::ensure!(
            !value.contains(&b'/') && value != b"." && value != b"..",
            "A filename must be a basename"
        );
    } else {
        anyhow::ensure!(value[0] == b'/', "File chooser paths must be absolute");
    }
    Ok(())
}

fn validate_filter(filter: &Filter) -> anyhow::Result<()> {
    text(&filter.name, 256)?;
    anyhow::ensure!(
        !filter.name.is_empty() && !filter.rules.is_empty() && filter.rules.len() <= 128,
        "Invalid file chooser filter"
    );
    for (kind, pattern) in &filter.rules {
        text(pattern, 1024)?;
        anyhow::ensure!(
            *kind <= 1 && !pattern.is_empty(),
            "Unsupported file chooser filter"
        );
        if *kind == 1 {
            anyhow::ensure!(
                pattern.contains('/') && !pattern.contains(char::is_whitespace),
                "Invalid MIME filter"
            );
        }
    }
    Ok(())
}

impl PickerRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == PROTOCOL_VERSION,
            "Unsupported file chooser protocol"
        );
        text(&self.app_id, 255)?;
        text(&self.title, 4096)?;
        text(&self.accept_label, 256)?;
        text(&self.parent_window, 1024)?;
        anyhow::ensure!(
            self.parent_window.is_empty()
                || self.parent_window.starts_with("wayland:")
                || self.parent_window.starts_with("x11:"),
            "Unknown parent window identifier"
        );
        for path in [&self.current_folder, &self.current_file]
            .into_iter()
            .flatten()
        {
            path_bytes(path, false)?;
        }
        text(&self.current_name, 255)?;
        if !self.current_name.is_empty() {
            path_bytes(self.current_name.as_bytes(), true)?;
        }
        anyhow::ensure!(
            self.files.len() <= MAX_FILES
                && (self.mode != PickerMode::SaveFiles || !self.files.is_empty()),
            "Invalid number of files to save"
        );
        for name in &self.files {
            path_bytes(name, true)?;
        }
        anyhow::ensure!(
            self.filters.len() <= 64 && self.choices.len() <= 32,
            "Too many file chooser controls"
        );
        for filter in &self.filters {
            validate_filter(filter)?;
        }
        if let Some(filter) = &self.current_filter {
            validate_filter(filter)?;
            anyhow::ensure!(
                self.filters.is_empty() || self.filters.contains(filter),
                "Current filter is absent from filter list"
            );
        }
        let mut ids = std::collections::HashSet::new();
        for choice in &self.choices {
            text(&choice.id, 256)?;
            text(&choice.label, 256)?;
            text(&choice.selected, 256)?;
            anyhow::ensure!(
                !choice.id.is_empty()
                    && !choice.label.is_empty()
                    && ids.insert(&choice.id)
                    && choice.options.len() <= 128,
                "Invalid file chooser choice"
            );
            let mut option_ids = std::collections::HashSet::new();
            for (id, label) in &choice.options {
                text(id, 256)?;
                text(label, 256)?;
                anyhow::ensure!(
                    !id.is_empty() && !label.is_empty() && option_ids.insert(id),
                    "Invalid choice option"
                );
            }
            anyhow::ensure!(
                choice.selected.is_empty()
                    || if choice.options.is_empty() {
                        matches!(choice.selected.as_str(), "true" | "false")
                    } else {
                        option_ids.contains(&choice.selected)
                    },
                "Invalid selected choice"
            );
        }
        Ok(())
    }

    pub fn validate_response(&self, response: &PickerResponse) -> anyhow::Result<PickerResponse> {
        anyhow::ensure!(response.response <= 2, "Invalid picker response");
        if response.response != 0 {
            return Ok(if response.response == 1 {
                PickerResponse::cancelled()
            } else {
                PickerResponse::failed()
            });
        }
        anyhow::ensure!(
            !response.uris.is_empty() && response.uris.len() <= MAX_FILES,
            "Picker selected no files or too many files"
        );
        let expected = if self.mode == PickerMode::SaveFiles {
            self.files.len()
        } else if self.multiple && self.mode == PickerMode::Open {
            response.uris.len()
        } else {
            1
        };
        anyhow::ensure!(
            response.uris.len() == expected,
            "Picker selected an invalid number of files"
        );
        let mut result = response.clone();
        let mut seen = std::collections::HashSet::new();
        let mut save_folder = None;
        for uri in &mut result.uris {
            text(uri, 16 * 1024)?;
            let path = uri_to_path(uri)?;
            anyhow::ensure!(seen.insert(path.clone()), "Picker returned duplicate files");
            if self.mode == PickerMode::Open {
                anyhow::ensure!(
                    if self.directory {
                        path.is_dir()
                    } else {
                        path.is_file()
                    },
                    "Selected file is unavailable or has the wrong type"
                );
            } else {
                let parent = path
                    .parent()
                    .filter(|parent| parent.is_dir())
                    .ok_or_else(|| anyhow::anyhow!("Selected destination folder is unavailable"))?;
                anyhow::ensure!(
                    path.file_name().is_some() && !path.is_dir(),
                    "Selected save destination is a directory"
                );
                if self.mode == PickerMode::SaveFiles {
                    if let Some(folder) = &save_folder {
                        anyhow::ensure!(
                            folder == parent,
                            "SaveFiles destinations must share a folder"
                        );
                    } else {
                        save_folder = Some(parent.to_path_buf());
                    }
                }
            }
            *uri = path_to_uri(&path)?;
        }
        if let Some(filter) = &result.current_filter {
            anyhow::ensure!(
                self.filters.contains(filter) || self.current_filter.as_ref() == Some(filter),
                "Picker returned an unknown filter"
            );
        }
        anyhow::ensure!(
            result.choices.len() == self.choices.len(),
            "Picker omitted requested choices"
        );
        let mut ids = std::collections::HashSet::new();
        for (id, value) in &result.choices {
            let choice = self
                .choices
                .iter()
                .find(|choice| choice.id == *id)
                .ok_or_else(|| anyhow::anyhow!("Picker returned an unknown choice"))?;
            anyhow::ensure!(
                ids.insert(id)
                    && if choice.options.is_empty() {
                        matches!(value.as_str(), "true" | "false")
                    } else {
                        choice.options.iter().any(|(option, _)| option == value)
                    },
                "Picker returned an invalid choice"
            );
        }
        Ok(result)
    }
}
