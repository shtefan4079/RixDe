use futures_util::StreamExt;
use std::{collections::HashMap, time::Duration};
use tokio::sync::{mpsc, watch};
use zbus::{
    zvariant::{OwnedObjectPath, OwnedValue, Value},
    Connection, Proxy,
};

type Layout = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    pub id: i32,
    pub label: String,
    pub icon_name: String,
    pub enabled: bool,
    pub separator: bool,
    pub checkable: bool,
    pub radio: bool,
    pub checked: bool,
    pub submenu: bool,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Page {
    pub entries: Vec<Entry>,
    pub can_go_back: bool,
}
pub enum Action {
    Select(i32),
    Back,
}
pub struct Session {
    pub pages: watch::Receiver<Page>,
    pub actions: mpsc::Sender<Action>,
}

pub async fn open(address: &str) -> anyhow::Result<Session> {
    let conn = zbus::connection::Builder::session()?
        .method_timeout(Duration::from_secs(3))
        .build()
        .await?;
    let item = super::status_notifier::item_proxy(&conn, address).await?;
    let path: OwnedObjectPath = item.get_property("Menu").await?;
    anyhow::ensure!(path.as_str() != "/", "Item has no exported menu");
    let service = item.destination().to_string();
    let proxy = menu_proxy(&conn, &service, path.as_str()).await?;
    let mut signals = proxy.receive_all_signals().await?;
    let page = read_page(&proxy, 0, false, true).await?;
    let (pages, receiver) = watch::channel(page.clone());
    let (actions, mut commands) = mpsc::channel(16);
    tokio::spawn(async move {
        let outcome = async {
            let mut stack = vec![0];
            let mut page = page;
            loop {
                let refresh = tokio::select! {
                    _ = pages.closed() => break,
                    signal = signals.next() => {
                        let Some(signal) = signal else { break };
                        let header = signal.header();
                        if !header.member().is_some_and(|m| matches!(m.as_str(), "LayoutUpdated" | "ItemsPropertiesUpdated")) { continue; }
                        false
                    }
                    command = commands.recv() => {
                        match command {
                            None => break,
                            Some(Action::Back) => { if stack.len() > 1 { stack.pop(); } }
                            Some(Action::Select(id)) => {
                                let Some(entry) = page.entries.iter().find(|e| e.id == id && e.enabled && !e.separator) else { continue };
                                if entry.submenu {
                                    if stack.len() >= 32 { continue; }
                                    stack.push(id);
                                } else {
                                    proxy.call_noreply("Event", &(id, "clicked", Value::from(0i32), 0u32)).await?;
                                    break;
                                }
                            }
                        }
                        true
                    }
                };
                page = read_page(&proxy, *stack.last().unwrap(), stack.len() > 1, refresh).await?;
                pages.send_if_modified(|old| { if *old == page { false } else { *old = page.clone(); true } });
            }
            let _ = proxy.call_noreply("Event", &(*stack.last().unwrap(), "closed", Value::from(0i32), 0u32)).await;
            Ok::<(), anyhow::Error>(())
        }.await;
        if let Err(error) = outcome {
            tracing::debug!("DBusMenu session ended: {error}");
        }
    });
    Ok(Session {
        pages: receiver,
        actions,
    })
}
async fn menu_proxy(
    conn: &Connection,
    service: &str,
    path: &str,
) -> anyhow::Result<Proxy<'static>> {
    Ok(zbus::proxy::Builder::<Proxy>::new(conn)
        .destination(service.to_owned())?
        .path(path.to_owned())?
        .interface("com.canonical.dbusmenu")?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?)
}
async fn read_page(
    proxy: &Proxy<'_>,
    parent: i32,
    can_go_back: bool,
    opening: bool,
) -> anyhow::Result<Page> {
    if opening {
        let _ = proxy.call::<_, _, bool>("AboutToShow", &(parent)).await;
        let _ = proxy
            .call_noreply("Event", &(parent, "opened", Value::from(0i32), 0u32))
            .await;
    }

    let (_, (_, _, children)): (u32, Layout) = proxy
        .call("GetLayout", &(parent, 1i32, Vec::<String>::new()))
        .await?;
    let entries = children
        .into_iter()
        .map(Layout::try_from)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter_map(|(id, props, children)| entry(id, &props, !children.is_empty()))
        .collect();
    Ok(Page {
        entries,
        can_go_back,
    })
}
fn entry(id: i32, props: &HashMap<String, OwnedValue>, children: bool) -> Option<Entry> {
    let boolean = |key, default| {
        props
            .get(key)
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(default)
    };
    let string = |key| {
        props
            .get(key)
            .and_then(|v| <&str>::try_from(v).ok())
            .unwrap_or("")
    };
    if !boolean("visible", true) {
        return None;
    }
    Some(Entry {
        id,
        label: unescape_mnemonic(string("label")),
        icon_name: string("icon-name").to_owned(),
        enabled: boolean("enabled", true),
        separator: string("type") == "separator",
        checkable: !string("toggle-type").is_empty(),
        radio: string("toggle-type") == "radio",
        checked: props
            .get("toggle-state")
            .and_then(|v| i32::try_from(v).ok())
            == Some(1),
        submenu: children || string("children-display") == "submenu",
    })
}
fn unescape_mnemonic(label: &str) -> String {
    let mut result = String::new();
    let mut chars = label.chars();
    while let Some(ch) = chars.next() {
        if ch == '_' {
            if let Some(next) = chars.next() {
                result.push(next);
            }
        } else {
            result.push(ch);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    struct MockItem;
    #[zbus::interface(name = "org.kde.StatusNotifierItem")]
    impl MockItem {
        #[zbus(property)]
        fn menu(&self) -> OwnedObjectPath {
            "/Menu".try_into().unwrap()
        }
    }
    struct MockMenu {
        label: String,
        clicked: mpsc::Sender<i32>,
    }
    #[zbus::interface(name = "com.canonical.dbusmenu")]
    impl MockMenu {
        fn get_layout(&self, parent: i32, depth: i32, _properties: Vec<String>) -> (u32, Layout) {
            assert_eq!(depth, 1);
            let children = if parent == 0 {
                vec![
                    mock_entry(1, "Disabled", false, false),
                    mock_entry(2, &self.label, true, true),
                ]
            } else {
                vec![mock_entry(10, "Leaf", true, false)]
            };
            (1, (parent, HashMap::new(), children))
        }
        fn about_to_show(&self, _id: i32) -> bool {
            false
        }
        async fn event(&self, id: i32, event: &str, _data: OwnedValue, _timestamp: u32) {
            if event == "clicked" {
                let _ = self.clicked.send(id).await;
            }
        }
        #[zbus(signal)]
        async fn layout_updated(
            emitter: &zbus::object_server::SignalEmitter<'_>,
            revision: u32,
            parent: i32,
        ) -> zbus::Result<()>;
    }
    fn mock_entry(id: i32, label: &str, enabled: bool, submenu: bool) -> OwnedValue {
        let mut props = HashMap::<String, OwnedValue>::new();
        props.insert("label".into(), Value::from(label).try_to_owned().unwrap());
        props.insert("enabled".into(), enabled.into());
        if submenu {
            props.insert(
                "children-display".into(),
                Value::from("submenu").try_to_owned().unwrap(),
            );
        }
        Value::from((id, props, Vec::<OwnedValue>::new()))
            .try_to_owned()
            .unwrap()
    }
    #[tokio::test]
    #[ignore = "requires an isolated dbus-run-session"]
    async fn private_bus_menu_updates_navigation_and_activation() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let conn = Connection::session().await.unwrap();
            let (clicked, mut clicks) = mpsc::channel(8);
            conn.object_server()
                .at("/StatusNotifierItem", MockItem)
                .await
                .unwrap();
            conn.object_server()
                .at(
                    "/Menu",
                    MockMenu {
                        label: "Before".into(),
                        clicked,
                    },
                )
                .await
                .unwrap();
            let mut session = open(&format!(
                "{}/StatusNotifierItem",
                conn.unique_name().unwrap()
            ))
            .await
            .unwrap();
            assert_eq!(session.pages.borrow().entries[1].label, "Before");
            session.actions.send(Action::Select(1)).await.unwrap();
            let interface = conn
                .object_server()
                .interface::<_, MockMenu>("/Menu")
                .await
                .unwrap();
            interface.get_mut().await.label = "After".into();
            MockMenu::layout_updated(interface.signal_emitter(), 2, 0)
                .await
                .unwrap();
            session
                .pages
                .wait_for(|p| p.entries[1].label == "After")
                .await
                .unwrap();
            session.actions.send(Action::Select(2)).await.unwrap();
            session.pages.wait_for(|p| p.can_go_back).await.unwrap();
            assert_eq!(session.pages.borrow().entries[0].id, 10);
            session.actions.send(Action::Back).await.unwrap();
            session.pages.wait_for(|p| !p.can_go_back).await.unwrap();
            session.actions.send(Action::Select(2)).await.unwrap();
            session.pages.wait_for(|p| p.can_go_back).await.unwrap();
            session.actions.send(Action::Select(10)).await.unwrap();
            assert_eq!(clicks.recv().await, Some(10));
        })
        .await
        .expect("DBusMenu lifecycle timed out");
    }
    #[test]
    fn preserves_menu_semantics_and_escaped_mnemonics() {
        let mut props = HashMap::new();
        props.insert(
            "label".into(),
            Value::from("_Open __file").try_to_owned().unwrap(),
        );
        props.insert("enabled".into(), OwnedValue::from(false));
        props.insert(
            "toggle-type".into(),
            Value::from("checkmark").try_to_owned().unwrap(),
        );
        props.insert("toggle-state".into(), OwnedValue::from(1i32));
        let item = entry(3, &props, true).unwrap();
        assert_eq!(item.label, "Open _file");
        assert!(!item.enabled && item.submenu && item.checkable && item.checked);
        props.insert("visible".into(), OwnedValue::from(false));
        assert!(entry(3, &props, false).is_none());
    }
}
