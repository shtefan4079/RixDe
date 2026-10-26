use crate::SettingsWindow;
use slint::ComponentHandle;

pub fn commit_layout(
    base: rix_config::LayoutConfig,
    updated: rix_config::LayoutConfig,
) -> anyhow::Result<rix_config::LayoutConfig> {
    tokio::runtime::Handle::current().block_on(async move {
        let mut client = rix_ipc::IpcClient::connect().await?;
        match client
            .send(rix_ipc::IpcRequest::EditLayout { base, updated })
            .await?
        {
            rix_ipc::IpcResponse::Layout(layout) => Ok(layout),
            response => anyhow::bail!("Unexpected config response: {response:?}"),
        }
    })
}

pub fn commit_theme(
    base: rix_config::ThemeSettings,
    updated: rix_config::ThemeSettings,
) -> anyhow::Result<rix_config::ThemeSettings> {
    tokio::runtime::Handle::current().block_on(async move {
        let mut client = rix_ipc::IpcClient::connect().await?;
        match client
            .send(rix_ipc::IpcRequest::EditTheme { base, updated })
            .await?
        {
            rix_ipc::IpcResponse::Theme(theme) => Ok(theme),
            response => anyhow::bail!("Unexpected config response: {response:?}"),
        }
    })
}

pub fn commit_desktop(
    base: rix_config::DesktopSettings,
    updated: rix_config::DesktopSettings,
) -> anyhow::Result<rix_config::DesktopSettings> {
    tokio::runtime::Handle::current().block_on(async move {
        let mut client = rix_ipc::IpcClient::connect().await?;
        match client
            .send(rix_ipc::IpcRequest::EditDesktop { base, updated })
            .await?
        {
            rix_ipc::IpcResponse::Desktop(desktop) => Ok(desktop),
            response => anyhow::bail!("Unexpected config response: {response:?}"),
        }
    })
}
pub fn run<T: Send + 'static>(
    window: &SettingsWindow,
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
    complete: impl FnOnce(&SettingsWindow, T) + Send + 'static,
) {
    if window.get_busy() {
        return;
    }
    window.set_busy(true);
    window.set_status_message(crate::i18n::tr("Applying…"));
    let weak = window.as_weak();
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(work).await;
        let _ = weak.upgrade_in_event_loop(move |window| {
            window.set_busy(false);
            match result {
                Ok(Ok(value)) => complete(&window, value),
                Ok(Err(error)) => window.set_status_message(error.to_string().into()),
                Err(error) => window.set_status_message(
                    format!("{}: {error}", crate::i18n::tr("Operation did not finish")).into(),
                ),
            }
        });
    });
}
pub fn save_theme(
    window: &SettingsWindow,
    editor: crate::theme_editor::State,
    theme: rix_config::ThemeSettings,
) {
    let base = editor.baseline();
    run(
        window,
        move || commit_theme(base, theme),
        move |window, theme| {
            editor.load(window, &theme);
            window.set_status_message(crate::i18n::tr("Appearance saved"));
        },
    );
}
