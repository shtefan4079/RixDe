use std::path::Path;

pub fn names(path: &Path, directory: bool, executable: bool) -> Vec<String> {
    names_with_mime(path, directory, executable, Vec::new())
}

pub fn names_with_mime(
    path: &Path,
    directory: bool,
    executable: bool,
    mime_icons: Vec<String>,
) -> Vec<String> {
    if directory {
        return vec!["folder".into()];
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let category: &[&str] = match extension.as_str() {
        "exe" | "com" | "msi" | "dll" => &[
            "application-x-ms-dos-executable",
            "application-x-executable",
        ],
        "apk" | "aab" => &[
            "application-vnd.android.package-archive",
            "application-x-apk",
            "package-x-generic",
        ],
        "appimage" | "desktop" => &["application-x-executable"],
        "deb" | "rpm" | "pkg" => &["package-x-generic"],
        "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar" | "zst" | "tgz" | "tbz" | "tbz2"
        | "txz" | "lz" | "lzma" | "cab" | "iso" => &["package-x-generic"],
        "pdf" => &["application-pdf", "x-office-document"],
        "doc" | "docx" | "odt" | "rtf" | "epub" | "mobi" => &["x-office-document"],
        "xls" | "xlsx" | "ods" | "csv" => &["x-office-spreadsheet"],
        "ppt" | "pptx" | "odp" => &["x-office-presentation"],
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "svg" | "svgz" | "bmp" | "tif" | "tiff"
        | "ico" | "avif" | "heic" | "jxl" | "xcf" | "psd" => &["image-x-generic"],
        "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" | "mpeg" | "mpg" | "ogv" | "3gp" => {
            &["video-x-generic"]
        }
        "mp3" | "wav" | "ogg" | "flac" | "opus" | "m4a" | "aac" | "aiff" | "wma" | "mid"
        | "midi" => &["audio-x-generic"],
        "rs" | "py" | "js" | "ts" | "jsx" | "tsx" | "c" | "cpp" | "h" | "hpp" | "sh" | "bash"
        | "zsh" | "go" | "java" | "kt" | "lua" | "rb" | "php" | "css" | "html" | "json"
        | "toml" | "yaml" | "yml" | "xml" | "kdl" => &["text-x-script", "text-x-generic"],
        "ttf" | "otf" | "woff" | "woff2" => &["font-x-generic"],
        _ if executable => &["application-x-executable"],
        _ => &[],
    };
    let mut names = mime_icons;

    names.retain(|name| name != "application-x-generic" && name != "application-octet-stream");
    for name in category {
        if !names.iter().any(|candidate| candidate == name) {
            names.push((*name).into());
        }
    }
    if !names.iter().any(|name| name == "text-x-generic") {
        names.push("text-x-generic".into());
    }
    names
}

pub fn fallback_svg(name: &str, foreground: &str, accent: &str) -> String {
    let detail = if name.contains("executable") || name.contains("ms-dos") {
        "<path d=\"m9 12 3 3-3 3m5 0h3\"/>"
    } else if name.contains("package") || name.contains("apk") || name.contains("archive") {
        "<path d=\"M10 4v16m0-13h3m-3 3h3m-3 3h3m-3 3h3\"/>"
    } else if name.starts_with("image-") {
        "<circle cx=\"10\" cy=\"11\" r=\"1\"/><path d=\"m7 18 4-4 2 2 2-3 3 5\"/>"
    } else if name.starts_with("audio-") {
        "<path d=\"M13 17v-6l4-1v6\"/><circle cx=\"11.5\" cy=\"17\" r=\"1.5\"/><circle cx=\"15.5\" cy=\"16\" r=\"1.5\"/>"
    } else if name.starts_with("video-") {
        "<path d=\"m10 11 6 4-6 4z\"/>"
    } else if name.contains("spreadsheet") {
        "<path d=\"M8 11h9v7H8zM8 14h9m-6-3v7\"/>"
    } else if name.contains("script") {
        "<path d=\"m10 12-3 3 3 3m5-6 3 3-3 3m-2-7-1 8\"/>"
    } else {
        "<path d=\"M9 12h6M9 16h6\"/>"
    };
    format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"64\" height=\"64\" viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"{foreground}\" stroke-width=\"1.5\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><path d=\"M14 3H6a1 1 0 0 0-1 1v16a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1V8z\" fill=\"{accent}\" fill-opacity=\"0.14\"/><path d=\"M14 3v5h5\"/>{detail}</svg>")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn executables_packages_and_unknown_formats_always_have_semantic_fallbacks() {
        for (name, expected) in [
            ("Installer.EXE", "application-x-executable"),
            ("Game.APK", "package-x-generic"),
            ("book.docx", "x-office-document"),
            ("data.unknown-format", "text-x-generic"),
        ] {
            let names = names(Path::new(name), false, false);
            assert!(
                names.iter().any(|name| name == expected),
                "{name}: {names:?}"
            );
            assert_eq!(names.last().unwrap(), "text-x-generic");
        }
        assert_eq!(
            names(Path::new("bin/run"), false, true)[0],
            "application-x-executable"
        );
        assert_eq!(names(Path::new("folder.exe"), true, true), ["folder"]);
    }
    #[test]
    fn custom_mime_icons_keep_exact_name_and_semantic_fallback_without_duplicates() {
        let names = names_with_mime(
            Path::new("a.apk"),
            false,
            false,
            vec!["custom-apk".into(), "application-x-generic".into()],
        );
        assert_eq!(names[0], "custom-apk");
        assert!(names.iter().any(|name| name == "package-x-generic"));
        assert!(!names.iter().any(|name| name == "application-x-generic"));
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
    }
}
