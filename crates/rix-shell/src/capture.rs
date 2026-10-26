use crate::RixPanelWindow;
use slint::{ComponentHandle, Model, ModelRc, VecModel};
pub fn apply(window: &RixPanelWindow, active: bool) {
    window.set_capture_active(active);
    let mut modules: Vec<_> = window
        .get_system_modules()
        .iter()
        .filter(|m| m != "capture")
        .collect();
    if active {
        modules.insert(0, "capture".into());
    }
    window.set_system_modules(ModelRc::new(VecModel::from(modules)));
    crate::layout::reflow(window);
}
pub fn install(window: &RixPanelWindow) {
    let weak = window.as_weak();
    window.on_stop_capture(move || {
        let weak = weak.clone();
        tokio::spawn(async move {
            let result = async {
                let mut client = rix_ipc::IpcClient::connect().await?;
                client.send(rix_ipc::IpcRequest::StopScreenCapture).await
            }
            .await;
            let _ = weak.upgrade_in_event_loop(move |w| match result {
                Ok(rix_ipc::IpcResponse::Ok) => {
                    w.set_system_popup("".into());
                    apply(&w, false);
                }
                Ok(rix_ipc::IpcResponse::Error(e)) => w.set_capture_error(crate::i18n::tr(&e)),
                Err(e) => w.set_capture_error(e.to_string().into()),
                _ => {}
            });
        });
    });
}
