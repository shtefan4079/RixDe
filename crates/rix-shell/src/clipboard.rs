use crate::{ClipboardItem, ClipboardState, RixPanelWindow};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::{cell::RefCell, rc::Rc};
thread_local! { static ENTRIES: RefCell<Vec<rix_ipc::ClipboardEntry>> = const { RefCell::new(Vec::new()) }; }
fn filter(window: &RixPanelWindow) {
    let global = window.global::<ClipboardState>();
    let query = global.get_query().to_lowercase();
    ENTRIES.with(|entries| {
        global.set_items(ModelRc::from(Rc::new(VecModel::from(
            entries
                .borrow()
                .iter()
                .filter(|e| e.label.to_lowercase().contains(&query))
                .map(|e| ClipboardItem {
                    id: e.id.to_string().into(),
                    label: if e.mime == "image/png" {
                        crate::i18n::tr("Image")
                    } else {
                        e.label.as_str().into()
                    },
                    image: e.mime == "image/png",
                    pinned: e.pinned,
                })
                .collect::<Vec<_>>(),
        ))))
    });
}
pub fn refresh(weak: slint::Weak<RixPanelWindow>) {
    tokio::spawn(async move {
        let result = async {
            rix_ipc::IpcClient::connect()
                .await?
                .send(rix_ipc::IpcRequest::GetClipboard)
                .await
        }
        .await;
        let _ = weak.upgrade_in_event_loop(move |window| {
            if let Ok(rix_ipc::IpcResponse::Clipboard { entries, error }) = result {
                ENTRIES.with(|cache| *cache.borrow_mut() = entries);
                window
                    .global::<ClipboardState>()
                    .set_error(crate::i18n::tr(&error));
                filter(&window);
            }
        });
    });
}
fn action(weak: slint::Weak<RixPanelWindow>, request: rix_ipc::IpcRequest, close: bool) {
    tokio::spawn(async move {
        let result = async { rix_ipc::IpcClient::connect().await?.send(request).await }.await;
        let _ = weak.upgrade_in_event_loop(move |window| match result {
            Ok(rix_ipc::IpcResponse::Ok) => {
                if close {
                    window.set_system_popup("".into());
                }
            }
            Ok(rix_ipc::IpcResponse::Error(error)) => window
                .global::<ClipboardState>()
                .set_error(crate::i18n::tr(&error)),
            Err(error) => window
                .global::<ClipboardState>()
                .set_error(crate::i18n::tr(&error.to_string())),
            _ => {}
        });
    });
}
pub fn install(window: &RixPanelWindow) {
    let weak = window.as_weak();
    window.global::<ClipboardState>().on_search(move |_| {
        if let Some(window) = weak.upgrade() {
            filter(&window);
        }
    });
    let weak = window.as_weak();
    window.global::<ClipboardState>().on_select(move |id| {
        if let Ok(id) = id.parse() {
            action(weak.clone(), rix_ipc::IpcRequest::ClipboardSelect(id), true);
        }
    });
    let weak = window.as_weak();
    window.global::<ClipboardState>().on_remove(move |id| {
        if let Ok(id) = id.parse() {
            action(
                weak.clone(),
                rix_ipc::IpcRequest::ClipboardDelete(id),
                false,
            );
        }
    });
    let weak = window.as_weak();
    window.global::<ClipboardState>().on_pin(move |id, pinned| {
        if let Ok(id) = id.parse() {
            action(
                weak.clone(),
                rix_ipc::IpcRequest::ClipboardPin { id, pinned },
                false,
            );
        }
    });
    let weak = window.as_weak();
    window
        .global::<ClipboardState>()
        .on_clear(move || action(weak.clone(), rix_ipc::IpcRequest::ClipboardClear, false));
}
