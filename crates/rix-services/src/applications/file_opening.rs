use freedesktop_desktop_entry::DesktopEntry;
#[path = "mime_database.rs"]
mod mime_database;
use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
};
#[derive(Clone, Debug)]
pub struct Application {
    pub id: String,
    pub name: String,
    pub icon: String,
}
fn locales() -> Vec<String> {
    freedesktop_desktop_entry::get_languages_from_env()
}
fn catalog() -> HashMap<String, DesktopEntry> {
    catalog_in(&application_dirs())
}
fn application_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| "/".into());
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".local/share"));
    let data_dirs = std::env::var_os("XDG_DATA_DIRS")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    std::iter::once(data)
        .chain(std::env::split_paths(&data_dirs).filter(|p| p.is_absolute()))
        .map(|p| p.join("applications"))
        .collect()
}
fn catalog_in(roots: &[PathBuf]) -> HashMap<String, DesktopEntry> {
    let mut entries = HashMap::new();
    let mut seen = HashSet::new();
    let locales = locales();
    for root in roots {
        let root = root.canonicalize().unwrap_or_else(|_| root.clone());
        for path in freedesktop_desktop_entry::Iter::new(std::iter::once(root.clone())) {
            let Some(id) = path
                .strip_prefix(&root)
                .ok()
                .and_then(|p| p.to_str())
                .map(|p| p.replace('/', "-"))
            else {
                continue;
            };

            if !seen.insert(id.clone()) {
                continue;
            }
            if let Ok(mut entry) = DesktopEntry::from_path(path, Some(&locales)) {
                entry.appid = id.strip_suffix(".desktop").unwrap_or(&id).into();
                entries.insert(id, entry);
            }
        }
    }
    entries
}
fn available(entry: &DesktopEntry) -> bool {
    !entry.hidden()
        && entry.type_() == Some("Application")
        && entry.try_exec().is_none_or(executable)
        && (entry.dbus_activatable() || entry.exec().is_some_and(|exec| !exec.trim().is_empty()))
}
fn executable(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let check = |p: PathBuf| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        return check(program.into());
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|p| check(p.join(program))))
}

#[derive(Clone)]
pub struct MimeDetector(std::sync::Arc<xdg_mime::SharedMimeInfo>);
impl Default for MimeDetector {
    fn default() -> Self {
        Self::new()
    }
}
impl MimeDetector {
    pub fn new() -> Self {
        Self(mime_database::snapshot())
    }
    pub fn mime(&self, path: &Path) -> Result<String, String> {
        mime_with_database(path, &self.0)
    }

    pub fn icon_names_for_filename(&self, name: &str) -> Vec<String> {
        let guess = self.0.guess_mime_type().file_name(name).guess();
        self.0.lookup_icon_names(guess.mime_type())
    }
}
pub fn mime(path: &Path) -> Result<String, String> {
    MimeDetector::new().mime(path)
}
fn mime_with_database(path: &Path, database: &xdg_mime::SharedMimeInfo) -> Result<String, String> {
    use std::{io::Read, os::unix::fs::OpenOptionsExt};
    let metadata = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if metadata.is_dir() {
        return Ok("inode/directory".into());
    }
    if !metadata.is_file() {
        return Err("Only regular files and folders can be opened".into());
    }
    let mut data = Vec::new();

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path);
    if let Ok(file) = file {
        if !file.metadata().is_ok_and(|m| m.is_file()) {
            return Err("Only regular files and folders can be opened".into());
        }
        let _ = file.take(64 * 1024).read_to_end(&mut data);
    }
    let mut guess = database.guess_mime_type();
    guess.metadata(metadata).data(&data);
    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
        guess.file_name(name);
    }
    Ok(guess.guess().mime_type().to_string())
}
fn mime_files() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/tmp".into());
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".local/share"));
    let config_dirs = std::env::var_os("XDG_CONFIG_DIRS")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/etc/xdg".into());
    let data_dirs = std::env::var_os("XDG_DATA_DIRS")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    let dirs = std::iter::once(config)
        .chain(std::env::split_paths(&config_dirs).filter(|p| p.is_absolute()))
        .chain(std::iter::once(data.join("applications")))
        .chain(
            std::env::split_paths(&data_dirs)
                .filter(|p| p.is_absolute())
                .map(|p| p.join("applications")),
        );
    let desktops = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "RixDE".into());
    let desktops: Vec<_> = desktops
        .split(':')
        .map(str::to_ascii_lowercase)
        .filter(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        })
        .collect();
    let mut files = Vec::new();
    for dir in dirs {
        for desktop in &desktops {
            files.push(dir.join(format!("{desktop}-mimeapps.list")));
        }
        files.push(dir.join("mimeapps.list"));
    }
    files
}
fn associations(
    files: &[PathBuf],
    mime: &str,
    mut applies: impl FnMut(&Path, &str) -> bool,
) -> (Vec<String>, Vec<String>, HashSet<String>) {
    let mut defaults = Vec::new();
    let mut added = Vec::new();
    let mut removed = HashSet::new();
    let mut decided = HashSet::new();
    for file in files {
        let Ok(content) = read_associations_file_with_links(file, true) else {
            continue;
        };
        let mut section = "";
        for line in content.lines().map(str::trim) {
            if line.starts_with('[') {
                section = line;
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key.trim() != mime {
                continue;
            }
            for id in value.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                match section {
                    "[Default Applications]" => defaults.push(id.to_owned()),
                    "[Added Associations]"
                        if file.file_name().is_some_and(|n| n == "mimeapps.list")
                            && applies(file, id) =>
                    {
                        if decided.insert(id.to_owned()) {
                            added.push(id.to_owned());
                        }
                    }
                    "[Removed Associations]"
                        if file.file_name().is_some_and(|n| n == "mimeapps.list")
                            && applies(file, id)
                            && decided.insert(id.to_owned()) =>
                    {
                        removed.insert(id.to_owned());
                    }
                    _ => {}
                }
            }
        }
    }
    (defaults, added, removed)
}
pub fn applications_for(path: &Path) -> Result<Vec<Application>, String> {
    let mime = mime(path)?;
    applications_for_mime(&mime)
}
pub fn applications_for_mime(mime: &str) -> Result<Vec<Application>, String> {
    validate_mime(mime)?;
    let roots: Vec<_> = application_dirs()
        .into_iter()
        .map(|p| p.canonicalize().unwrap_or(p))
        .collect();
    let catalog = catalog_in(&roots);
    let files = mime_files();
    let database = mime_database::snapshot();
    let mut types = vec![mime.to_owned()];
    let mut index = 0;
    while index < types.len() {
        if let Ok(parsed) = types[index].parse() {
            for parent in database.get_parents(&parsed).unwrap_or_default() {
                let parent = parent.to_string();
                if !types.contains(&parent) {
                    types.push(parent);
                }
            }
        }
        index += 1;
    }
    let mut seen = HashSet::new();
    Ok(types
        .iter()
        .flat_map(|mime| applications_for_type(&catalog, &roots, &files, mime))
        .filter(|app| seen.insert(app.id.clone()))
        .collect())
}
fn applications_for_type(
    catalog: &HashMap<String, DesktopEntry>,
    roots: &[PathBuf],
    files: &[PathBuf],
    mime: &str,
) -> Vec<Application> {
    let (defaults, added, removed) = associations(files, mime, |file, id| {
        let parent = file.parent().unwrap_or(Path::new("/"));
        let parent = parent.canonicalize().unwrap_or_else(|_| parent.into());

        let Some(file_rank) = roots.iter().position(|p| p == &parent) else {
            return true;
        };
        catalog
            .get(id)
            .and_then(|e| roots.iter().position(|p| e.path.starts_with(p)))
            .is_some_and(|entry_rank| file_rank <= entry_rank)
    });
    let associated = |id: &String, entry: &DesktopEntry| {
        !removed.contains(id)
            && (added.contains(id) || entry.mime_type().is_some_and(|types| types.contains(&mime)))
    };
    let mut candidates: Vec<_> = catalog
        .iter()
        .filter(|(id, e)| available(e) && associated(id, e))
        .map(|(id, e)| Application {
            id: id.clone(),
            name: e
                .name(&locales())
                .unwrap_or_else(|| e.appid.clone().into())
                .into_owned(),
            icon: e.icon().unwrap_or("application-x-executable").into(),
        })
        .collect();
    candidates.sort_by_key(|e| {
        (
            added
                .iter()
                .position(|id| id == &e.id)
                .unwrap_or(usize::MAX),
            e.name.to_lowercase(),
            e.id.clone(),
        )
    });

    for id in defaults.iter().rev() {
        if let Some(e) = catalog
            .get(id)
            .filter(|e| available(e) && associated(id, e))
        {
            candidates.retain(|a| &a.id != id);
            candidates.insert(
                0,
                Application {
                    id: id.clone(),
                    name: e
                        .name(&locales())
                        .unwrap_or_else(|| e.appid.clone().into())
                        .into_owned(),
                    icon: e.icon().unwrap_or("application-x-executable").into(),
                },
            );
        }
    }
    candidates
}

#[derive(Clone, Debug)]
pub struct ApplicationChoices {
    pub mime: String,
    pub applications: Vec<Application>,
    pub recommended: HashSet<String>,
    pub default_id: Option<String>,
}

pub fn application_choices(path: &Path) -> Result<ApplicationChoices, String> {
    choices_for_mime(&mime(path)?)
}

pub fn choices_for_mime(mime: &str) -> Result<ApplicationChoices, String> {
    let mut applications = applications_for_mime(mime)?;
    let recommended = applications.iter().map(|app| app.id.clone()).collect();
    let entries = catalog();
    let roots: Vec<_> = application_dirs()
        .into_iter()
        .map(|p| p.canonicalize().unwrap_or(p))
        .collect();
    let files = mime_files();
    let supported = applications_for_type(&entries, &roots, &files, mime);
    let default_id = associations(&files, mime, |_, _| true)
        .0
        .into_iter()
        .find(|id| supported.iter().any(|app| &app.id == id));
    let mut others: Vec<_> = entries
        .iter()
        .filter(|(id, entry)| {
            available(entry)
                && !entry.no_display()
                && !applications.iter().any(|app| &app.id == *id)
        })
        .map(|(id, entry)| Application {
            id: id.clone(),
            name: entry
                .name(&locales())
                .unwrap_or_else(|| entry.appid.clone().into())
                .into_owned(),
            icon: entry.icon().unwrap_or("application-x-executable").into(),
        })
        .collect();
    others.sort_by_key(|app| (app.name.to_lowercase(), app.id.clone()));
    applications.extend(others);
    Ok(ApplicationChoices {
        mime: mime.into(),
        applications,
        recommended,
        default_id,
    })
}

fn validate_mime(mime: &str) -> Result<(), String> {
    let valid_part = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&byte))
    };
    if mime.len() > 255
        || !mime
            .split_once('/')
            .is_some_and(|(kind, subtype)| valid_part(kind) && valid_part(subtype))
    {
        return Err("Invalid file type".into());
    }
    Ok(())
}

pub fn set_default(mime: &str, desktop_id: &str) -> Result<(), String> {
    set_defaults(&[mime], desktop_id)
}

pub fn set_defaults(mimes: &[&str], desktop_id: &str) -> Result<(), String> {
    for mime in mimes {
        validate_mime(mime)?;
    }
    if mimes.is_empty() || !catalog().get(desktop_id).is_some_and(available) {
        return Err("The selected application is no longer installed".into());
    }
    let first = mime_files()
        .into_iter()
        .next()
        .ok_or("No user association directory")?;
    let config = first.parent().ok_or("Invalid association directory")?;
    set_defaults_in(config, &first, mimes, desktop_id)
}

fn set_defaults_in(
    config: &Path,
    priority_file: &Path,
    mimes: &[&str],
    desktop_id: &str,
) -> Result<(), String> {
    use std::{
        io::Write,
        os::unix::{fs::OpenOptionsExt, io::AsRawFd},
    };
    for mime in mimes {
        validate_mime(mime)?;
    }
    if desktop_id.is_empty()
        || !desktop_id.ends_with(".desktop")
        || desktop_id.len() > 255
        || desktop_id
            .bytes()
            .any(|byte| byte.is_ascii_control() || b";/=\\".contains(&byte))
        || priority_file.parent() != Some(config)
    {
        return Err("Invalid application association".into());
    }
    std::fs::create_dir_all(config).map_err(|error| error.to_string())?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(config.join(".rix-mimeapps.lock"))
        .map_err(|error| error.to_string())?;
    if !lock
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("Application association lock is not a regular file".into());
    }
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let generic_file = config.join("mimeapps.list");
    let mut generic = read_associations_file(&generic_file)?;
    for mime in mimes {
        let added = association_values(&generic, "Added Associations", mime);
        let mut next = vec![desktop_id.to_owned()];
        next.extend(added.into_iter().filter(|id| id != desktop_id));
        generic = update_association(&generic, "Added Associations", mime, &next);
        let removed = association_values(&generic, "Removed Associations", mime)
            .into_iter()
            .filter(|id| id != desktop_id)
            .collect::<Vec<_>>();
        generic = update_association(&generic, "Removed Associations", mime, &removed);

        generic = update_association(
            &generic,
            "Default Applications",
            mime,
            &[desktop_id.to_owned()],
        );
    }
    let mut writes = vec![(generic_file.clone(), generic)];
    if priority_file != generic_file {
        let mut priority = read_associations_file(priority_file)?;
        for mime in mimes {
            priority = update_association(
                &priority,
                "Default Applications",
                mime,
                &[desktop_id.to_owned()],
            );
        }
        writes.push((priority_file.to_path_buf(), priority));
    }

    let mut staged = Vec::new();
    for (path, content) in writes {
        let mut temp =
            tempfile::NamedTempFile::new_in(config).map_err(|error| error.to_string())?;
        temp.write_all(content.as_bytes())
            .map_err(|error| error.to_string())?;
        temp.as_file()
            .sync_all()
            .map_err(|error| error.to_string())?;
        staged.push((path, temp));
    }
    for (path, temp) in staged {
        temp.persist(path).map_err(|error| error.to_string())?;
    }
    std::fs::File::open(config)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| error.to_string())
}

fn read_associations_file(path: &Path) -> Result<String, String> {
    read_associations_file_with_links(path, false)
}

fn read_associations_file_with_links(path: &Path, follow_links: bool) -> Result<String, String> {
    use std::{io::Read, os::unix::fs::OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | if follow_links { 0 } else { libc::O_NOFOLLOW })
        .open(path);
    let mut file = match file {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => return Err(error.to_string()),
    };
    let meta = file.metadata().map_err(|error| error.to_string())?;
    if !meta.is_file() || meta.len() > 2 * 1024 * 1024 {
        return Err("Application associations file is not a regular bounded file".into());
    }
    let mut content = String::new();
    file.by_ref()
        .take(2 * 1024 * 1024 + 1)
        .read_to_string(&mut content)
        .map_err(|error| error.to_string())?;
    if content.len() > 2 * 1024 * 1024 || content.contains('\0') {
        return Err("Application associations file is invalid".into());
    }
    Ok(content)
}

fn association_values(content: &str, section: &str, key: &str) -> Vec<String> {
    let mut current = "";
    let mut values = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            current = &trimmed[1..trimmed.len() - 1];
        } else if current == section {
            if let Some((name, value)) = trimmed.split_once('=') {
                if name.trim() == key {
                    values.extend(
                        value
                            .split(';')
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(str::to_owned),
                    );
                }
            }
        }
    }
    values
}

fn update_association(content: &str, section: &str, key: &str, values: &[String]) -> String {
    let mut output = Vec::new();
    let mut current = "";
    let mut inserted = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            current = &trimmed[1..trimmed.len() - 1];
            output.push(line.to_owned());
            if current == section && !inserted {
                if !values.is_empty() {
                    output.push(format!("{key}={};", values.join(";")));
                }
                inserted = true;
            }
        } else if current != section
            || !trimmed
                .split_once('=')
                .is_some_and(|(name, _)| name.trim() == key)
        {
            output.push(line.to_owned());
        }
    }
    if !inserted && !values.is_empty() {
        if !output.is_empty() {
            output.push(String::new());
        }
        output.push(format!("[{section}]"));
        output.push(format!("{key}={};", values.join(";")));
    }
    if output.is_empty() {
        String::new()
    } else {
        output.join("\n") + "\n"
    }
}

fn unescape_value(value: &str) -> String {
    let mut unescaped = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek().copied() {
                Some('s') => {
                    chars.next();
                    unescaped.push(' ');
                }
                Some('n') => {
                    chars.next();
                    unescaped.push('\n');
                }
                Some('t') => {
                    chars.next();
                    unescaped.push('\t');
                }
                Some('r') => {
                    chars.next();
                    unescaped.push('\r');
                }
                Some('\\') => {
                    chars.next();
                    unescaped.push('\\');
                }
                _ => unescaped.push(c),
            }
        } else {
            unescaped.push(c)
        }
    }
    unescaped
}
fn tokenize(exec: &str) -> Result<Vec<String>, String> {

    let unescaped = unescape_value(exec);
    let mut out = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut active = false;
    let mut chars = unescaped.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                active = true;
            }
            '\\' => {
                let next = chars.next().ok_or("Trailing escape in Exec")?;
                if quoted && !matches!(next, '\\' | '"' | '$' | '`') {
                    return Err("Unsupported escape inside quoted desktop Exec argument".into());
                }
                token.push(next);
                active = true;
            }
            c if c.is_whitespace() && !quoted => {
                if active {
                    out.push(std::mem::take(&mut token));
                    active = false;
                }
            }
            c => {
                token.push(c);
                active = true;
            }
        }
    }
    if quoted {
        return Err("Unclosed quote in desktop Exec".into());
    }
    if active {
        out.push(token);
    }
    if out.is_empty() {
        return Err("Desktop Exec is empty".into());
    }
    Ok(out)
}
fn arguments(entry: &DesktopEntry, path: Option<&Path>) -> Result<Vec<OsString>, String> {
    let tokens = tokenize(entry.exec().ok_or("Application has no Exec command")?)?;
    if tokens[0].contains('%') {
        return Err("Desktop Exec executable cannot contain field codes".into());
    }
    let mut result = Vec::new();
    let mut file_codes = 0;
    for token in tokens {
        match token.as_str() {
            "%f" | "%F" => {
                file_codes += 1;
                if let Some(p) = path {
                    result.push(p.as_os_str().to_owned());
                }
            }
            "%u" | "%U" => {
                file_codes += 1;
                if let Some(p) = path {
                    result.push(
                        url::Url::from_file_path(p)
                            .map_err(|_| "Cannot convert path to URI")?
                            .to_string()
                            .into(),
                    );
                }
            }
            "%i" => {
                if let Some(icon) = entry.icon() {
                    result.push("--icon".into());
                    result.push(icon.into());
                }
            }
            "%c" => result.push(
                entry
                    .name(&locales())
                    .unwrap_or_else(|| entry.appid.clone().into())
                    .into_owned()
                    .into(),
            ),
            "%k" => result.push(entry.path.as_os_str().to_owned()),
            "%d" | "%D" | "%n" | "%N" | "%v" | "%m" => {}
            _ => {
                let mut text = String::new();
                let mut chars = token.chars();
                while let Some(c) = chars.next() {
                    if c == '%' {
                        if chars.next() != Some('%') {
                            return Err("Unsupported or embedded field code in Exec".into());
                        }
                        text.push('%');
                    } else {
                        text.push(c);
                    }
                }
                result.push(text.into());
            }
        }
    }
    if file_codes > 1 {
        return Err("Desktop Exec contains multiple file field codes".into());
    }
    if result.is_empty() || result[0].is_empty() {
        return Err("Desktop Exec has no executable".into());
    }
    Ok(result)
}
async fn launch(entry: DesktopEntry, path: PathBuf) -> Result<(), String> {
    if entry.dbus_activatable() {
        let activation = async {
            let bus = zbus::Connection::session()
                .await
                .map_err(|e| e.to_string())?;
            let id = entry.appid.as_str();
            let object = format!("/{}", id.replace('.', "/").replace('-', "_"));
            let proxy = zbus::Proxy::new(&bus, id, object.as_str(), "org.freedesktop.Application")
                .await
                .map_err(|e| e.to_string())?;
            let uri = url::Url::from_file_path(&path)
                .map_err(|_| "Invalid file URI")?
                .to_string();
            let data = HashMap::<String, zbus::zvariant::Value<'_>>::new();
            proxy
                .call::<_, _, ()>("Open", &(vec![uri], data))
                .await
                .map_err(|e| e.to_string())
        };
        if tokio::time::timeout(std::time::Duration::from_secs(5), activation)
            .await
            .is_ok_and(|v| v.is_ok())
        {
            return Ok(());
        }
    }
    let args = arguments(&entry, Some(&path))?;
    let executable = PathBuf::from(&args[0]);
    tokio::task::spawn_blocking(move || super::native_compat::prepare(&executable))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    if entry.terminal() {
        return spawn_terminal(
            entry
                .path()
                .filter(|s| !s.is_empty())
                .map(|s| PathBuf::from(unescape_value(s)))
                .unwrap_or_else(|| path.parent().unwrap_or(Path::new("/")).into()),
            Some(args),
        )
        .await;
    }
    let mut command = tokio::process::Command::new(&args[0]);
    command.args(&args[1..]);
    if let Some(dir) = entry.path().filter(|s| !s.is_empty()) {
        command.current_dir(unescape_value(dir));
    }
    spawn(command)
}
fn spawn(mut command: tokio::process::Command) -> Result<(), String> {
    command.stdin(Stdio::null());
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        if let Ok(status) = child.wait().await {
            if !status.success() {
                tracing::warn!(%status,"Launched application exited unsuccessfully");
            }
        }
    });
    Ok(())
}
pub async fn open_default(path: PathBuf) -> Result<(), String> {
    let path = absolute_path(path)?;
    let lookup = path.clone();
    let applications = tokio::task::spawn_blocking(move || applications_for(&lookup))
        .await
        .map_err(|e| e.to_string())??;
    let app = applications
        .first()
        .ok_or("No application is associated with this file. Use Open with.")?;
    open_with(path, app.id.clone()).await
}
pub async fn open_with(path: PathBuf, desktop_id: String) -> Result<(), String> {
    let path = absolute_path(path)?;
    let entry = tokio::task::spawn_blocking(move || {
        catalog()
            .remove(&desktop_id)
            .filter(available)
            .ok_or("Application is unavailable")
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(str::to_owned)?;
    launch(entry, path).await
}
pub async fn open_terminal(directory: PathBuf) -> Result<(), String> {
    spawn_terminal(absolute_path(directory)?, None).await
}
fn absolute_path(path: PathBuf) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStrExt;
    if path.as_os_str().as_bytes().contains(&0) {
        return Err("Path contains a NUL byte".into());
    }
    if path.is_absolute() {
        Ok(path)
    } else {
        std::env::current_dir()
            .map(|directory| directory.join(path))
            .map_err(|e| e.to_string())
    }
}
async fn spawn_terminal(directory: PathBuf, run: Option<Vec<OsString>>) -> Result<(), String> {
    if !directory.is_dir() {
        return Err("Terminal working directory is unavailable".into());
    }
    let args = tokio::task::spawn_blocking(terminal_command)
        .await
        .map_err(|e| e.to_string())??;
    let args = terminal_arguments(args, &directory, run)?;
    let mut command = tokio::process::Command::new(&args[0]);
    command.args(&args[1..]).current_dir(&directory);
    spawn(command)
}
fn terminal_command() -> Result<Vec<OsString>, String> {
    let configured = std::env::var("TERMINAL").ok();
    let candidates = [
        "ghostty",
        "foot",
        "alacritty",
        "kitty",
        "konsole",
        "gnome-terminal",
        "xterm",
    ];
    if let Some(configured) = configured {
        return Ok(terminal_tokens(&configured)?
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>());
    }
    let entries = catalog();
    let files = mime_files();
    for mime in [
        "x-scheme-handler/terminal",
        "application/x-terminal-emulator",
    ] {
        for id in associations(&files, mime, |_, _| true).0 {
            if let Some(entry) = entries.get(&id).filter(|e| available(e)) {
                return arguments(entry, None);
            }
        }
    }

    let mut launchers: Vec<_> = entries
        .values()
        .filter(|entry| {
            available(entry)
                && entry
                    .categories()
                    .is_some_and(|categories| categories.contains(&"TerminalEmulator"))
        })
        .collect();
    launchers.sort_by_key(|entry| &entry.appid);
    for entry in launchers {
        if let Ok(args) = arguments(entry, None) {
            if terminal_arguments(args.clone(), Path::new("/"), None).is_ok() {
                return Ok(args);
            }
        }
    }
    let name = candidates
        .iter()
        .find(|name| executable(name))
        .ok_or("No terminal is installed; set TERMINAL to your preferred terminal")?;
    Ok(vec![(*name).into()])
}
fn terminal_tokens(command: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut active = false;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' if quote.is_none() => {
                quote = Some(c);
                active = true;
            }
            c if Some(c) == quote => quote = None,
            '\\' if quote != Some('\'') => {
                let next = chars.next().ok_or("Trailing escape in TERMINAL")?;
                if quote == Some('"') && !matches!(next, '$' | '`' | '"' | '\\') {
                    token.push('\\');
                }
                token.push(next);
                active = true;
            }
            c if c.is_whitespace() && quote.is_none() => {
                if active {
                    args.push(std::mem::take(&mut token));
                    active = false;
                }
            }
            c => {
                token.push(c);
                active = true;
            }
        }
    }
    if quote.is_some() {
        return Err("Unclosed quote in TERMINAL".into());
    }
    if active {
        args.push(token);
    }
    if args.first().is_none_or(|program| program.is_empty()) {
        return Err("TERMINAL is empty".into());
    }
    Ok(args)
}
fn terminal_arguments(
    mut args: Vec<OsString>,
    directory: &Path,
    run: Option<Vec<OsString>>,
) -> Result<Vec<OsString>, String> {
    if args.is_empty() {
        return Err("TERMINAL is empty".into());
    }
    let name = Path::new(&args[0])
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let name = name.into_owned();
    match name.as_ref() {
        "ghostty" => {
            let mut flag = OsString::from("--working-directory=");
            flag.push(directory);
            args.push(flag);
        }
        "kitty" => {
            args.push("--directory".into());
            args.push(directory.as_os_str().to_owned());
        }
        "alacritty" | "foot" | "footclient" | "gnome-terminal" | "xfce4-terminal" => {
            args.push("--working-directory".into());
            args.push(directory.as_os_str().to_owned());
        }
        "konsole" => {
            args.push("--workdir".into());
            args.push(directory.as_os_str().to_owned());
        }
        "xterm" | "urxvt" | "rxvt" => {}
        _ => return Err(format!("Cannot open a working directory with terminal {name}; set TERMINAL to a supported terminal")),
    }
    if let Some(run) = run {
        args.push(
            match name.as_str() {
                "gnome-terminal" | "kitty" | "foot" | "footclient" => "--",
                "xfce4-terminal" => "--execute",
                _ => "-e",
            }
            .into(),
        );
        args.extend(run);
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filename_icon_candidates_reuse_custom_mime_names_without_reading_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let mime = directory.path().join("mime");
        std::fs::create_dir(&mime).unwrap();
        std::fs::write(
            mime.join("globs2"),
            "80:application/x-rix-fixture:*.rix-icon-fixture\n",
        )
        .unwrap();
        std::fs::write(
            mime.join("icons"),
            "application/x-rix-fixture:rix-fixture-icon\n",
        )
        .unwrap();
        std::fs::write(
            mime.join("generic-icons"),
            "application/x-rix-fixture:package-x-generic\n",
        )
        .unwrap();
        let detector = MimeDetector(std::sync::Arc::new(
            xdg_mime::SharedMimeInfo::new_for_directory(directory.path()),
        ));
        assert_eq!(
            detector.icon_names_for_filename("missing.rix-icon-fixture"),
            [
                "rix-fixture-icon",
                "application-x-rix-fixture",
                "package-x-generic"
            ]
        );
    }
    fn entry(exec: &str) -> DesktopEntry {
        let mut e = DesktopEntry::from_appid("test".into());
        e.add_desktop_entry("Exec".into(), exec.into());
        e.add_desktop_entry("Name".into(), "Test App".into());
        e
    }
    #[test]
    fn exec_arguments_preserve_quoting_and_byte_exact_files_without_shell() {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(OsString::from_vec(b"/tmp/a $(touch hacked)\xff".to_vec()));
        let args = arguments(&entry("test \"two words\" %% %f"), Some(&path)).unwrap();
        assert_eq!(
            args,
            vec![
                OsString::from("test"),
                "two words".into(),
                "%".into(),
                path.as_os_str().to_owned()
            ]
        );
        assert!(arguments(&entry("test %Q"), Some(&path)).is_err());
        assert!(arguments(&entry("test %f %u"), Some(&path)).is_err());
        assert!(tokenize("test \"unclosed").is_err());
    }
    #[test]
    fn mime_default_order_and_removals() {
        let r = tempfile::tempdir().unwrap();
        let high = r.path().join("mimeapps.list");
        let low = r.path().join("system.list");
        std::fs::write(&high,"[Default Applications]\ntext/plain=preferred.desktop;\n[Removed Associations]\ntext/plain=removed.desktop;\n").unwrap();
        std::fs::write(
            &low,
            "[Default Applications]\ntext/plain=fallback.desktop;\n",
        )
        .unwrap();
        let (d, _, removed) = associations(&[high, low], "text/plain", |_, _| true);
        assert_eq!(d, vec!["preferred.desktop", "fallback.desktop"]);
        assert!(removed.contains("removed.desktop"));
    }
    #[test]
    fn exec_uri_arguments_escape_reserved_characters_and_preserve_raw_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(OsString::from_vec(b"/tmp/% #?\xff".to_vec()));
        let args = arguments(&entry("test %u"), Some(&path)).unwrap();
        assert_eq!(args[1], "file:///tmp/%25%20%23%3F%FF");
        assert!(arguments(&entry("%f"), Some(&path)).is_err());
    }
    #[test]
    fn user_desktop_override_hides_system_copy_even_in_nested_directory() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user/applications");
        let system = dir.path().join("system/applications");
        std::fs::create_dir_all(user.join("vendor")).unwrap();
        std::fs::create_dir_all(system.join("vendor")).unwrap();
        std::fs::write(
            user.join("vendor/test.desktop"),
            "[Desktop Entry]\nType=Application\nName=Hidden\nHidden=true\nExec=test\n",
        )
        .unwrap();
        std::fs::write(
            system.join("vendor/test.desktop"),
            "[Desktop Entry]\nType=Application\nName=System\nExec=test\n",
        )
        .unwrap();
        let catalog = catalog_in(&[user, system]);
        assert_eq!(catalog.len(), 1);
        assert!(catalog["vendor-test.desktop"].hidden());
        assert!(!available(&catalog["vendor-test.desktop"]));
    }
    #[test]
    fn desktop_specific_files_only_set_defaults_and_higher_additions_override_removals() {
        let dir = tempfile::tempdir().unwrap();
        let desktop = dir.path().join("rix-mimeapps.list");
        let high = dir.path().join("mimeapps.list");
        let low = dir.path().join("low/mimeapps.list");
        std::fs::create_dir_all(low.parent().unwrap()).unwrap();
        std::fs::write(&desktop, "[Default Applications]\ntext/plain=preferred.desktop;\n[Removed Associations]\ntext/plain=preferred.desktop;\n").unwrap();
        std::fs::write(
            &high,
            "[Added Associations]\ntext/plain=preferred.desktop;\n",
        )
        .unwrap();
        std::fs::write(
            &low,
            "[Removed Associations]\ntext/plain=preferred.desktop;\n",
        )
        .unwrap();
        let (defaults, added, removed) =
            associations(&[desktop, high, low], "text/plain", |_, _| true);
        assert_eq!(defaults, vec!["preferred.desktop"]);
        assert_eq!(added, vec!["preferred.desktop"]);
        assert!(removed.is_empty());
    }
    #[test]
    fn terminal_directory_and_command_arguments_are_not_shell_strings() {
        let directory = Path::new("/tmp/a $() #");
        let run = vec!["editor".into(), "/tmp/a $() #/file".into()];
        let args = terminal_arguments(vec!["kitty".into()], directory, Some(run.clone())).unwrap();
        assert_eq!(
            args,
            vec![
                OsString::from("kitty"),
                "--directory".into(),
                directory.as_os_str().into(),
                "--".into(),
                run[0].clone(),
                run[1].clone()
            ]
        );
        assert!(terminal_arguments(vec!["unknown-terminal".into()], directory, None).is_err());
        assert_eq!(
            terminal_tokens("kitty --title 'Rix Files' \"$(touch hacked)\"").unwrap(),
            vec!["kitty", "--title", "Rix Files", "$(touch hacked)"]
        );
        assert!(terminal_tokens("kitty 'unfinished").is_err());
    }
    #[test]
    fn mime_lookup_does_not_open_special_files() {
        assert!(mime(Path::new("/dev/null")).is_err());
    }
    #[test]
    fn defaults_require_an_association_and_system_removals_do_not_override_user_desktop_files() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user/applications");
        let system = dir.path().join("system/applications");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&system).unwrap();
        std::fs::write(user.join("viewer.desktop"), "[Desktop Entry]\nType=Application\nName=Viewer\nExec=viewer %f\nMimeType=text/plain;\nNoDisplay=true\n").unwrap();
        std::fs::write(system.join("unrelated.desktop"), "[Desktop Entry]\nType=Application\nName=Unrelated\nExec=unrelated %f\nMimeType=image/png;\n").unwrap();
        let preferences = system.join("mimeapps.list");
        std::fs::write(&preferences, "[Default Applications]\ntext/plain=unrelated.desktop;viewer.desktop;\n[Removed Associations]\ntext/plain=viewer.desktop;\n").unwrap();
        let roots = vec![user, system];
        let catalog = catalog_in(&roots);
        let apps = applications_for_type(&catalog, &roots, &[preferences], "text/plain");
        assert_eq!(
            apps.iter().map(|app| app.id.as_str()).collect::<Vec<_>>(),
            vec!["viewer.desktop"]
        );
    }

    #[test]
    fn setting_a_default_preserves_other_types_and_overrides_desktop_priority() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let generic = dir.path().join("mimeapps.list");
        let priority = dir.path().join("rix-mimeapps.list");
        std::fs::write(&generic, "# personal associations\n[Default Applications]\ntext/plain=editor.desktop;\napplication/pdf=old.desktop;\n[Added Associations]\napplication/pdf=old.desktop;viewer.desktop;\n[Removed Associations]\napplication/pdf=viewer.desktop;blocked.desktop;\n[Custom Section]\nkeep=value\n").unwrap();
        std::fs::write(&priority, "# desktop preferences\n[Default Applications]\napplication/pdf=old.desktop;\nimage/png=image.desktop;\n").unwrap();
        set_defaults_in(
            dir.path(),
            &priority,
            &["application/pdf"],
            "viewer.desktop",
        )
        .unwrap();
        let generic_content = std::fs::read_to_string(&generic).unwrap();
        let desktop_content = std::fs::read_to_string(&priority).unwrap();
        assert_eq!(
            association_values(&generic_content, "Default Applications", "text/plain"),
            vec!["editor.desktop"]
        );
        assert_eq!(
            association_values(&generic_content, "Default Applications", "application/pdf"),
            vec!["viewer.desktop"]
        );
        assert_eq!(
            association_values(&generic_content, "Added Associations", "application/pdf"),
            vec!["viewer.desktop", "old.desktop"]
        );
        assert_eq!(
            association_values(&generic_content, "Removed Associations", "application/pdf"),
            vec!["blocked.desktop"]
        );
        assert_eq!(
            association_values(&desktop_content, "Default Applications", "application/pdf"),
            vec!["viewer.desktop"]
        );
        assert_eq!(
            association_values(&desktop_content, "Default Applications", "image/png"),
            vec!["image.desktop"]
        );
        assert!(generic_content.contains("# personal associations"));
        assert!(generic_content.contains("[Custom Section]\nkeep=value"));
        assert_eq!(
            std::fs::metadata(&generic).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&priority).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn default_write_preflights_both_files_before_replacing_either() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let generic = dir.path().join("mimeapps.list");
        let priority = dir.path().join("rix-mimeapps.list");
        let victim = dir.path().join("unrelated");
        let original = "[Default Applications]\napplication/pdf=old.desktop;\n";
        std::fs::write(&generic, original).unwrap();
        std::fs::write(&victim, "preserve me").unwrap();
        symlink(&victim, &priority).unwrap();
        assert!(set_defaults_in(
            dir.path(),
            &priority,
            &["application/pdf"],
            "viewer.desktop"
        )
        .is_err());
        assert_eq!(std::fs::read_to_string(&generic).unwrap(), original);
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "preserve me");
        std::fs::remove_file(&priority).unwrap();
        std::fs::write(&priority, "invalid\0content").unwrap();
        assert!(set_defaults_in(
            dir.path(),
            &priority,
            &["application/pdf"],
            "viewer.desktop"
        )
        .is_err());
        assert_eq!(std::fs::read_to_string(&generic).unwrap(), original);
    }

    #[test]
    fn linked_preferences_can_be_read_without_replacing_the_link() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles-mimeapps.list");
        let linked = dir.path().join("mimeapps.list");
        std::fs::write(
            &real,
            "[Default Applications]\napplication/pdf=viewer.desktop;\n",
        )
        .unwrap();
        symlink(&real, &linked).unwrap();
        assert_eq!(
            associations(std::slice::from_ref(&linked), "application/pdf", |_, _| {
                true
            })
            .0,
            vec!["viewer.desktop"]
        );
        assert!(
            set_defaults_in(dir.path(), &linked, &["application/pdf"], "other.desktop").is_err()
        );
        assert!(std::fs::symlink_metadata(&linked).unwrap().is_symlink());
        assert!(std::fs::read_to_string(&real)
            .unwrap()
            .contains("viewer.desktop"));
    }

    #[test]
    fn concurrent_default_writes_preserve_every_file_type() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().to_path_buf();
        let threads: Vec<_> = (0..8)
            .map(|index| {
                let config = config.clone();
                std::thread::spawn(move || {
                    set_defaults_in(
                        &config,
                        &config.join("rix-mimeapps.list"),
                        &[&format!("application/x-test-{index}")],
                        &format!("viewer-{index}.desktop"),
                    )
                    .unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        for filename in ["mimeapps.list", "rix-mimeapps.list"] {
            let content = std::fs::read_to_string(config.join(filename)).unwrap();
            for index in 0..8 {
                assert_eq!(
                    association_values(
                        &content,
                        "Default Applications",
                        &format!("application/x-test-{index}")
                    ),
                    vec![format!("viewer-{index}.desktop")]
                );
            }
        }
    }

    #[test]
    fn malformed_associations_cannot_inject_sections_or_escape_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let priority = dir.path().join("rix-mimeapps.list");
        for mime in [
            "",
            "application/pdf\n[Other]",
            "application/pdf;",
            "application/",
            "/pdf",
            "application/a=b",
        ] {
            assert!(
                set_defaults_in(dir.path(), &priority, &[mime], "viewer.desktop").is_err(),
                "{mime:?}"
            );
        }
        for desktop in [
            "../viewer.desktop",
            "viewer.desktop;other.desktop",
            "viewer.desktop\n",
            "viewer",
            "viewer.desktop=other",
        ] {
            assert!(
                set_defaults_in(dir.path(), &priority, &["application/pdf"], desktop).is_err(),
                "{desktop:?}"
            );
        }
        assert!(!priority.exists());
    }

    #[test]
    fn standard_libgio_reads_the_new_pdf_default_in_a_private_xdg_profile() {
        if !Path::new("/usr/bin/gio").is_file() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        let data = dir.path().join("data");
        let empty = dir.path().join("empty");
        std::fs::create_dir_all(data.join("applications")).unwrap();
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(
            data.join("applications/viewer.desktop"),
            "[Desktop Entry]\nType=Application\nName=Test PDF Viewer\nExec=/usr/bin/true %f\n",
        )
        .unwrap();
        set_defaults_in(
            &config,
            &config.join("rix-mimeapps.list"),
            &["application/pdf"],
            "viewer.desktop",
        )
        .unwrap();
        let output = std::process::Command::new("/usr/bin/gio")
            .args(["mime", "application/pdf"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .env("HOME", dir.path())
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_DATA_HOME", &data)
            .env("XDG_CONFIG_DIRS", &empty)
            .env("XDG_DATA_DIRS", &empty)
            .env("XDG_CURRENT_DESKTOP", "RIX:GNOME")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            text.lines()
                .next()
                .is_some_and(|line| line.ends_with("viewer.desktop")),
            "{text}"
        );
        let apps = applications_for_type(
            &catalog_in(&[data.join("applications")]),
            &[data.join("applications")],
            &[
                config.join("rix-mimeapps.list"),
                config.join("mimeapps.list"),
            ],
            "application/pdf",
        );
        assert_eq!(apps.first().unwrap().id, "viewer.desktop");
    }
}
