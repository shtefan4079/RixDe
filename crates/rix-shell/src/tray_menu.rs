use crate::{ContextMenuItemData, RixPanelWindow};
use rix_services::{
    dbus_menu::{self, Action, Page, Session},
    status_notifier::{TrayAction, TrayCommand},
};
use slint::ComponentHandle;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::mpsc;

enum Request {
    Open {
        address: String,
        x: f32,
        bottom: f32,
        generation: u64,
    },
    Select(String),
    Close,
}
pub struct Controller {
    requests: mpsc::Sender<Request>,
    generation: Arc<AtomicU64>,
}
impl Controller {
    pub fn open(&self, window: &RixPanelWindow, address: String, x: f32, y: f32) {
        window.invoke_tray_menu_closed();
        window.set_is_context_menu_open(false);
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let bottom = window.window().size().height as f32 / window.window().scale_factor() - y;
        window.set_is_app_menu_open(false);
        window.set_context_menu_top(y);
        let _ = self.requests.try_send(Request::Open {
            address,
            x,
            bottom,
            generation,
        });
    }
}
pub fn install(window: &RixPanelWindow, fallback: mpsc::Sender<TrayCommand>) -> Controller {
    let (requests, mut incoming) = mpsc::channel(16);
    let generation = Arc::new(AtomicU64::new(0));
    let sender = requests.clone();
    window.on_tray_menu_selected(move |id| {
        let _ = sender.try_send(Request::Select(id.to_string()));
    });
    let sender = requests.clone();
    let counter = generation.clone();
    window.on_tray_menu_closed(move || {
        counter.fetch_add(1, Ordering::Relaxed);
        let _ = sender.try_send(Request::Close);
    });
    let weak = window.as_weak();
    let counter = generation.clone();
    tokio::spawn(async move {
        let mut session: Option<Session> = None;
        let mut position = (0., 0., 0);
        loop {
            let page = tokio::select! {
                request = incoming.recv() => {
                    match request {
                        None => break,
                        Some(Request::Close) => { session = None; continue; }
                        Some(Request::Select(id)) => {
                            if let Some(session) = &session {
                                let action = if id == "back" { Some(Action::Back) } else { id.parse().ok().map(Action::Select) };
                                if let Some(action) = action { let _ = session.actions.try_send(action); }
                            }
                            continue;
                        }
                        Some(Request::Open { address, x, bottom, generation }) => {
                            session = None;
                            if counter.load(Ordering::Relaxed) != generation { continue; }
                            match dbus_menu::open(&address).await {
                                Ok(mut opened) => {
                                    if counter.load(Ordering::Relaxed) != generation { continue; }
                                    let page = opened.pages.borrow_and_update().clone();
                                    session = Some(opened);
                                    position = (x, bottom, generation);
                                    Some(page)
                                }
                                Err(error) => {
                                    tracing::debug!(%address, "No DBusMenu: {error}");
                                    if counter.load(Ordering::Relaxed) == generation {
                                        let _ = fallback.try_send(TrayCommand { address, action: TrayAction::ContextMenu });
                                    }
                                    continue;
                                }
                            }
                        }
                    }
                }
                changed = async {
                    match &mut session {
                        Some(s) => s.pages.changed().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if changed.is_ok() { session.as_mut().map(|s| s.pages.borrow_and_update().clone()) }
                    else { session = None; None }
                }
            };
            let weak = weak.clone();
            let counter = counter.clone();
            let (x, bottom, generation) = position;
            let _ = slint::invoke_from_event_loop(move || {
                if counter.load(Ordering::Relaxed) != generation {
                    return;
                }
                let Some(window) = weak.upgrade() else { return };
                if let Some(page) = page {
                    window.set_context_can_go_back(page.can_go_back);
                    window.set_context_prefer_below(window.get_panel_at_top());
                    crate::popup::set_items(&window, rows(page));
                    window.set_context_is_tray(true);
                    window.set_context_menu_x(x);
                    window.set_context_menu_bottom(bottom);
                    window.set_is_context_menu_open(true);
                } else if window.get_context_is_tray() {
                    window.set_is_context_menu_open(false);
                }
            });
        }
    });
    Controller {
        requests,
        generation,
    }
}
fn rows(page: Page) -> Vec<ContextMenuItemData> {
    let mut rows = Vec::new();
    if page.can_go_back {
        rows.push(ContextMenuItemData {
            id: "back".into(),
            label: crate::i18n::tr("Back"),
            ..Default::default()
        });
    }
    rows.extend(page.entries.into_iter().map(|entry| {
        let icon = crate::apps::load_icon_at_size(&entry.icon_name, 20);
        ContextMenuItemData {
            has_icon: icon.is_some(),
            icon: icon.unwrap_or_default(),
            id: entry.id.to_string().into(),
            label: entry.label.into(),
            disabled: !entry.enabled,
            separator: entry.separator,
            checkable: entry.checkable,
            radio: entry.radio,
            checked: entry.checked,
            submenu: entry.submenu,
            ..Default::default()
        }
    }));
    rows
}
