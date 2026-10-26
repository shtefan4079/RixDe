use crate::{BundledThemes, InstalledTheme, SettingsWindow};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::rc::Rc;

pub fn install(window: &SettingsWindow, editor: crate::theme_editor::State) {
    let weak = window.as_weak();
    tokio::spawn(async move {
        let packages = tokio::task::spawn_blocking(rix_config::themes::discover).await;
        if let Ok(packages) = packages {
            let _ = weak.upgrade_in_event_loop(move |window| {
                window
                    .global::<BundledThemes>()
                    .set_packages(ModelRc::from(Rc::new(VecModel::from(
                        packages
                            .into_iter()
                            .map(|package| InstalledTheme {
                                id: package.info.id.into(),
                                name: package.info.name.into(),
                                description: crate::i18n::tr(&package.info.description),
                            })
                            .collect::<Vec<_>>(),
                    ))));
            });
        }
    });
    let weak = window.as_weak();
    window.global::<BundledThemes>().on_apply(move |id| {
        let Some(window) = weak.upgrade() else { return };
        let include_layout = window.global::<BundledThemes>().get_include_layout();
        let id = id.to_string();
        let editor = editor.clone();
        crate::jobs::run(
            &window,
            move || {
                tokio::runtime::Handle::current().block_on(async {
                    let mut client = rix_ipc::IpcClient::connect().await?;
                    match client
                        .send(rix_ipc::IpcRequest::ApplyThemePackage { id, include_layout })
                        .await?
                    {
                        rix_ipc::IpcResponse::Appearance(snapshot) => Ok(snapshot),
                        response => anyhow::bail!("Could not apply theme: {response:?}"),
                    }
                })
            },
            move |window, snapshot| {
                editor.load(window, &snapshot.theme);
                window.set_status_message(crate::i18n::tr("Theme applied"));
            },
        );
    });
}
