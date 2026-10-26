pub async fn choose_file(title: &str, filter: &str) -> anyhow::Result<Option<std::path::PathBuf>> {
    let result = tokio::process::Command::new("zenity")
        .args([
            "--file-selection",
            "--title",
            title,
            "--file-filter",
            filter,
        ])
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "{}: {e}",
                crate::i18n::tr("Could not open the file chooser; install zenity")
            )
        })?;
    if result.status.code() == Some(1) {
        return Ok(None);
    }
    if !result.status.success() {
        anyhow::bail!(crate::i18n::tr("The file chooser failed"));
    }
    let path = String::from_utf8(result.stdout)?
        .trim_end_matches('\n')
        .to_owned();
    Ok(Some(std::fs::canonicalize(path)?))
}

pub async fn choose_color(initial: &str) -> anyhow::Result<Option<String>> {
    let result = tokio::process::Command::new("zenity")
        .args([
            "--color-selection",
            "--show-palette",
            "--title",
            crate::i18n::tr("Choose color").as_str(),
            "--color",
            initial,
        ])
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "{}: {e}",
                crate::i18n::tr("Could not open the color chooser; install zenity")
            )
        })?;
    if result.status.code() == Some(1) {
        return Ok(None);
    }
    if !result.status.success() {
        anyhow::bail!(crate::i18n::tr("The color chooser failed"));
    }
    let value = String::from_utf8(result.stdout)?.trim().to_owned();
    if crate::is_valid_hex_color(&value) {
        return Ok(Some(value));
    }
    let body = value
        .strip_prefix("rgba(")
        .or_else(|| value.strip_prefix("rgb("))
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(|| anyhow::anyhow!(crate::i18n::tr("Unsupported color format")))?;
    let channels = body
        .split(',')
        .map(|v| v.trim().parse::<f32>())
        .collect::<Result<Vec<_>, _>>()?;
    anyhow::ensure!(
        channels.len() == 3 || channels.len() == 4,
        crate::i18n::tr("Invalid color")
    );
    let mut color = format!(
        "#{:02x}{:02x}{:02x}",
        channels[0].clamp(0., 255.) as u8,
        channels[1].clamp(0., 255.) as u8,
        channels[2].clamp(0., 255.) as u8
    );
    if channels.len() == 4 {
        color.push_str(&format!(
            "{:02x}",
            (channels[3].clamp(0., 1.) * 255.).round() as u8
        ));
    }
    Ok(Some(color))
}
pub fn install(window: &crate::SettingsWindow) {
    use slint::{ComponentHandle, Model, ModelRc, VecModel};
    let weak = window.as_weak();
    tokio::spawn(async move {
        if let Ok(output) = tokio::process::Command::new("fc-list")
            .args(["--format", "%{family[0]}\n"])
            .output()
            .await
        {
            let families: std::collections::BTreeSet<_> = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            let _ = weak.upgrade_in_event_loop(move |w| {
                w.set_font_families(ModelRc::new(VecModel::from(
                    families.into_iter().map(Into::into).collect::<Vec<_>>(),
                )));
            });
        }
    });
    let weak = window.as_weak();
    window.on_choose_theme_file(move || {
        let weak = weak.clone();
        tokio::spawn(async move {
            let result = choose_file(
                crate::i18n::tr("Import theme").as_str(),
                "Themes | *.zip *.ZIP",
            )
            .await;
            let _ = weak.upgrade_in_event_loop(move |w| match result {
                Ok(Some(path)) => w.invoke_import_theme_zip(path.to_string_lossy().as_ref().into()),
                Ok(None) => {}
                Err(e) => w.set_status_message(e.to_string().into()),
            });
        });
    });
    let weak = window.as_weak();
    window.on_choose_style_file(move |index| {
        let weak = weak.clone();
        tokio::spawn(async move {
            let result =
                choose_file(crate::i18n::tr("Theme SVG").as_str(), "SVG | *.svg *.svgz").await;
            let _ = weak.upgrade_in_event_loop(move |w| match result {
                Ok(Some(path)) => {
                    w.invoke_edit_style_setting(index, path.to_string_lossy().as_ref().into())
                }
                Ok(None) => {}
                Err(e) => w.set_status_message(e.to_string().into()),
            });
        });
    });
    let weak = window.as_weak();
    window.on_choose_color(move |index, initial| {
        let weak = weak.clone();
        tokio::spawn(async move {
            let result = choose_color(initial.as_str()).await;
            let _ = weak.upgrade_in_event_loop(move |window| match result {
                Ok(Some(color)) if index < 0 => window.set_accent_color(color.into()),
                Ok(Some(color)) => {
                    if window
                        .get_style_settings()
                        .row_data(index as usize)
                        .is_some_and(|r| r.group == "palette")
                    {
                        window.invoke_edit_style_setting(index, color.into());
                    }
                }
                Ok(None) => {}
                Err(e) => window.set_status_message(e.to_string().into()),
            });
        });
    });
}
