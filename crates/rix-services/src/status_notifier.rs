use futures_util::StreamExt;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};
use tokio::sync::{mpsc, watch, Mutex};
use zbus::{message::Header, object_server::SignalEmitter, Connection, Proxy};

const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const PATH: &str = "/StatusNotifierWatcher";
const ITEM: &str = "org.kde.StatusNotifierItem";

#[derive(Clone, Debug, PartialEq)]
pub struct TrayItem {
    pub address: String,
    pub id: String,
    pub title: String,
    pub icon_name: String,
    pub icon_theme_path: String,
    pub pixmaps: Vec<(i32, i32, Vec<u8>)>,
    pub passive: bool,
    pub menu_only: bool,
}
#[derive(Clone, Debug)]
pub enum TrayAction {
    Activate,
    SecondaryActivate,
    ContextMenu,
    Scroll(i32),
}
pub struct TrayCommand {
    pub address: String,
    pub action: TrayAction,
}
pub struct TrayHost {
    pub items: watch::Receiver<Vec<TrayItem>>,
    pub commands: mpsc::Sender<TrayCommand>,
}
pub fn start() -> TrayHost {
    let (items, receiver) = watch::channel(Vec::new());
    let (commands, mut incoming) = mpsc::channel(32);
    tokio::spawn(async move {
        let mut delay = std::time::Duration::from_secs(1);
        loop {
            let started = std::time::Instant::now();
            match run(items.clone(), &mut incoming).await {
                Ok(()) => break,
                Err(error) => tracing::warn!("StatusNotifier reconnecting: {error}"),
            }
            items.send_replace(Vec::new());
            if started.elapsed().as_secs() > 30 {
                delay = std::time::Duration::from_secs(1);
            }
            tokio::select! {
                _ = items.closed() => break,
                _ = tokio::time::sleep(delay) => {}
            }
            delay = (delay * 2).min(std::time::Duration::from_secs(30));
        }
    });
    TrayHost {
        items: receiver,
        commands,
    }
}

fn split_address(address: &str) -> Option<(&str, &str)> {
    let at = address.find('/')?;
    let (service, path) = address.split_at(at);
    (!service.is_empty()).then_some((service, path))
}
pub(crate) async fn item_proxy<'a>(
    conn: &'a Connection,
    address: &'a str,
) -> anyhow::Result<Proxy<'a>> {
    let (service, path) =
        split_address(address).ok_or_else(|| anyhow::anyhow!("Invalid tray address"))?;
    Ok(zbus::proxy::Builder::<Proxy<'a>>::new(conn)
        .destination(service)?
        .path(path)?
        .interface(ITEM)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?)
}
async fn snapshot(proxy: &Proxy<'_>, address: &str) -> TrayItem {
    let status: String = proxy.get_property("Status").await.unwrap_or_default();
    let attention = status == "NeedsAttention";
    let mut icon_name: String = proxy
        .get_property(if attention {
            "AttentionIconName"
        } else {
            "IconName"
        })
        .await
        .unwrap_or_default();
    if icon_name.is_empty() {
        icon_name = proxy.get_property("IconName").await.unwrap_or_default();
    }
    let mut pixmaps: Vec<(i32, i32, Vec<u8>)> = proxy
        .get_property(if attention {
            "AttentionIconPixmap"
        } else {
            "IconPixmap"
        })
        .await
        .unwrap_or_default();
    if pixmaps.is_empty() {
        pixmaps = proxy.get_property("IconPixmap").await.unwrap_or_default();
    }
    TrayItem {
        address: address.into(),
        id: proxy.get_property("Id").await.unwrap_or_default(),
        title: proxy.get_property("Title").await.unwrap_or_default(),
        icon_name,
        icon_theme_path: proxy
            .get_property("IconThemePath")
            .await
            .unwrap_or_default(),
        pixmaps,
        passive: status == "Passive",
        menu_only: proxy.get_property("ItemIsMenu").await.unwrap_or(false),
    }
}
struct ItemTask(tokio::task::JoinHandle<()>);
impl Drop for ItemTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn run(
    items: watch::Sender<Vec<TrayItem>>,
    commands: &mut mpsc::Receiver<TrayCommand>,
) -> anyhow::Result<()> {
    let conn = Connection::session().await?;
    let bus = zbus::fdo::DBusProxy::new(&conn).await?;
    let mut owner_events = bus.receive_name_owner_changed().await?;
    let own_watcher = if !bus.name_has_owner(WATCHER.try_into()?).await? {
        let watcher = Watcher::default();
        let state = watcher.items.clone();
        conn.object_server().at(PATH, watcher).await?;
        conn.request_name(WATCHER).await?;
        Some(state)
    } else {
        None
    };
    let host_name = format!("org.kde.StatusNotifierHost-rix-{}", std::process::id());
    conn.request_name(host_name.as_str()).await?;
    let watcher = zbus::proxy::Builder::<Proxy>::new(&conn)
        .destination(WATCHER)?
        .path(PATH)?
        .interface(WATCHER)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;
    let mut registration = watcher.receive_all_signals().await?;
    watcher
        .call::<_, _, ()>("RegisterStatusNotifierHost", &(host_name.as_str()))
        .await?;
    let (updates, mut incoming) = mpsc::channel::<TrayItem>(32);
    let mut jobs = HashMap::<String, ItemTask>::new();
    let mut model = BTreeMap::<String, TrayItem>::new();
    let mut refresh = true;
    loop {
        if refresh {
            let addresses: Vec<String> = watcher
                .get_property("RegisteredStatusNotifierItems")
                .await?;
            jobs.retain(|address, _| addresses.contains(address));
            model.retain(|address, _| addresses.contains(address));
            for address in addresses {
                if jobs.contains_key(&address) {
                    continue;
                }
                let conn = conn.clone();
                let updates = updates.clone();
                let key = address.clone();
                let task = tokio::spawn(async move {
                    let outcome = async {
                        let proxy = item_proxy(&conn, &address).await?;
                        let mut signals = proxy.receive_all_signals().await?;
                        if let Ok(item) = tokio::time::timeout(
                            std::time::Duration::from_secs(3),
                            snapshot(&proxy, &address),
                        )
                        .await
                        {
                            updates.send(item).await?;
                        }
                        while let Some(signal) = signals.next().await {
                            let header = signal.header();
                            if header.member().is_some_and(|name| {
                                matches!(
                                    name.as_str(),
                                    "NewIcon"
                                        | "NewAttentionIcon"
                                        | "NewStatus"
                                        | "NewTitle"
                                        | "NewToolTip"
                                )
                            }) {
                                if let Ok(item) = tokio::time::timeout(
                                    std::time::Duration::from_secs(3),
                                    snapshot(&proxy, &address),
                                )
                                .await
                                {
                                    updates.send(item).await?;
                                }
                            }
                        }
                        Ok::<(), anyhow::Error>(())
                    }
                    .await;
                    if let Err(error) = outcome {
                        tracing::debug!(%address, "Tray item unavailable: {error}");
                    }
                });
                jobs.insert(key, ItemTask(task));
            }
            items.send_replace(model.values().cloned().collect());
            refresh = false;
        }
        tokio::select! {
            _ = items.closed() => return Ok(()),
            event = registration.next() => {
                if event.is_none() { anyhow::bail!("Watcher disconnected"); }
                refresh = true;
            }
            Some(update) = incoming.recv() => {
                if jobs.contains_key(&update.address) && model.get(&update.address) != Some(&update) {
                    model.insert(update.address.clone(), update);
                    items.send_replace(model.values().cloned().collect());
                }
            }
            command = commands.recv() => {
                let Some(command) = command else { return Ok(()); };
                if jobs.contains_key(&command.address) {
                    let conn = conn.clone();
                    tokio::spawn(async move {
                        if let Err(error) = activate(&conn, command).await { tracing::debug!("Tray action: {error}"); }
                    });
                }
            }
            event = owner_events.next() => {
                let Some(event) = event else { anyhow::bail!("Session bus disconnected"); };
                let args = event.args()?;
                if args.name().as_str() == WATCHER && args.old_owner().is_some() {
                    anyhow::bail!("StatusNotifier watcher changed owner");
                }
                if args.new_owner().is_none() {
                    if let Some(state) = &own_watcher {
                        let mut state = state.lock().await;
                        let removed: Vec<_> = state.iter().filter(|(_,owner)| owner.as_str() == args.name().as_str()).map(|(key,_)| key.clone()).collect();
                        for address in removed {
                            state.remove(&address);
                            Watcher::status_notifier_item_unregistered(&SignalEmitter::new(&conn, PATH)?, &address).await?;
                        }
                    }
                }
            }
        }
    }
}
async fn activate(conn: &Connection, command: TrayCommand) -> anyhow::Result<()> {
    let proxy = item_proxy(conn, &command.address).await?;
    match command.action {
        TrayAction::Scroll(delta) => {
            proxy
                .call::<_, _, ()>("Scroll", &(delta, "vertical"))
                .await?;
        }
        action => {
            let method = match action {
                TrayAction::Activate => "Activate",
                TrayAction::SecondaryActivate => "SecondaryActivate",
                _ => "ContextMenu",
            };
            proxy.call::<_, _, ()>(method, &(0i32, 0i32)).await?;
        }
    }
    Ok(())
}

#[derive(Default)]
struct Watcher {
    items: Arc<Mutex<BTreeMap<String, String>>>,
}
#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    async fn register_status_notifier_item(
        &self,
        service: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("Missing sender".into()))?;
        let (address, owner) = if service.starts_with('/') {
            (format!("{sender}{service}"), sender.to_string())
        } else {
            let bus = zbus::fdo::DBusProxy::new(conn).await?;
            let name = service
                .try_into()
                .map_err(|_| zbus::fdo::Error::InvalidArgs("Invalid service".into()))?;
            let owner = bus.get_name_owner(name).await?;
            (format!("{service}/StatusNotifierItem"), owner.to_string())
        };
        if self
            .items
            .lock()
            .await
            .insert(address.clone(), owner)
            .is_none()
        {
            Self::status_notifier_item_registered(&emitter, &address).await?;
        }
        Ok(())
    }
    async fn register_status_notifier_host(&self, _service: &str) {}
    #[zbus(property)]
    async fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items.lock().await.keys().cloned().collect()
    }
    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }
    #[zbus(signal)]
    async fn status_notifier_item_registered(
        emitter: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn status_notifier_item_unregistered(
        emitter: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;
}

pub fn rgba(pixmaps: &[(i32, i32, Vec<u8>)], desired: u32) -> Option<(u32, u32, Vec<u8>)> {
    let (width, height, bytes) = pixmaps
        .iter()
        .filter(|(w, h, data)| {
            *w > 0
                && *h > 0
                && *w <= 512
                && *h <= 512
                && data.len() == *w as usize * *h as usize * 4
        })
        .min_by_key(|(w, _, _)| {
            (
                u32::try_from(*w).unwrap_or(0) < desired,
                (i64::from(*w) - i64::from(desired)).abs(),
            )
        })?;
    let mut pixels = bytes.clone();
    for pixel in pixels.chunks_exact_mut(4) {
        let [a, r, g, b] = <[u8; 4]>::try_from(&*pixel).ok()?;
        let premultiply = |c: u8| ((c as u16 * a as u16 + 127) / 255) as u8;
        pixel.copy_from_slice(&[premultiply(r), premultiply(g), premultiply(b), a]);
    }
    Some((*width as u32, *height as u32, pixels))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "requires an isolated dbus-run-session"]
    async fn private_bus_recovers_after_watcher_restart() {
        use std::time::Duration;
        tokio::time::timeout(Duration::from_secs(12), async {
            let old = Connection::session().await.unwrap();
            old.object_server()
                .at(PATH, Watcher::default())
                .await
                .unwrap();
            old.request_name(WATCHER).await.unwrap();
            let mut host = start();
            let item = Connection::session().await.unwrap();
            item.object_server()
                .at(
                    "/StatusNotifierItem",
                    MockItem {
                        title: "Survives watcher restart".into(),
                        activated: Arc::new(tokio::sync::Notify::new()),
                    },
                )
                .await
                .unwrap();
            let proxy = Proxy::new(&item, WATCHER, PATH, WATCHER).await.unwrap();
            proxy
                .call::<_, _, ()>("RegisterStatusNotifierItem", &"/StatusNotifierItem")
                .await
                .unwrap();
            host.items.wait_for(|items| items.len() == 1).await.unwrap();
            old.close().await.unwrap();
            host.items.wait_for(Vec::is_empty).await.unwrap();
            let bus = zbus::fdo::DBusProxy::new(&item).await.unwrap();
            while !bus
                .name_has_owner(WATCHER.try_into().unwrap())
                .await
                .unwrap()
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let replacement = Proxy::new(&item, WATCHER, PATH, WATCHER).await.unwrap();
            replacement
                .call::<_, _, ()>("RegisterStatusNotifierItem", &"/StatusNotifierItem")
                .await
                .unwrap();
            host.items.wait_for(|items| items.len() == 1).await.unwrap();
        })
        .await
        .expect("Tray did not recover after the watcher restarted");
    }
    struct MockItem {
        title: String,
        activated: Arc<tokio::sync::Notify>,
    }
    #[zbus::interface(name = "org.kde.StatusNotifierItem")]
    impl MockItem {
        #[zbus(property)]
        fn status(&self) -> &str {
            "Active"
        }
        #[zbus(property)]
        fn title(&self) -> &str {
            &self.title
        }
        #[zbus(property)]
        fn icon_name(&self) -> &str {
            "test-icon"
        }
        #[zbus(property)]
        fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
            vec![]
        }
        #[zbus(property)]
        fn icon_theme_path(&self) -> &str {
            ""
        }
        #[zbus(property)]
        fn item_is_menu(&self) -> bool {
            false
        }
        fn activate(&self, _x: i32, _y: i32) {
            self.activated.notify_one();
        }
        #[zbus(signal)]
        async fn new_title(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    }

    #[tokio::test]
    #[ignore = "requires an isolated dbus-run-session"]
    async fn private_bus_registration_updates_actions_and_disconnect() {
        use std::time::Duration;
        tokio::time::timeout(Duration::from_secs(10), async {
            let client = Connection::session().await.unwrap();
            let bus = zbus::fdo::DBusProxy::new(&client).await.unwrap();
            assert!(
                !bus.name_has_owner(WATCHER.try_into().unwrap())
                    .await
                    .unwrap(),
                "Run this test under dbus-run-session"
            );
            let mut host = start();
            while !bus
                .name_has_owner(WATCHER.try_into().unwrap())
                .await
                .unwrap()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let activated = Arc::new(tokio::sync::Notify::new());
            let item_conn = Connection::session().await.unwrap();
            item_conn
                .object_server()
                .at(
                    "/StatusNotifierItem",
                    MockItem {
                        title: "Before".into(),
                        activated: activated.clone(),
                    },
                )
                .await
                .unwrap();
            let watcher = Proxy::new(&item_conn, WATCHER, PATH, WATCHER)
                .await
                .unwrap();
            watcher
                .call::<_, _, ()>("RegisterStatusNotifierItem", &("/StatusNotifierItem"))
                .await
                .unwrap();
            let item = host.items.wait_for(|items| items.len() == 1).await.unwrap()[0].clone();
            assert_eq!(item.title, "Before");
            assert_eq!(item.icon_name, "test-icon");
            host.commands
                .send(TrayCommand {
                    address: item.address,
                    action: TrayAction::Activate,
                })
                .await
                .unwrap();
            activated.notified().await;
            let interface = item_conn
                .object_server()
                .interface::<_, MockItem>("/StatusNotifierItem")
                .await
                .unwrap();
            interface.get_mut().await.title = "After".into();
            MockItem::new_title(interface.signal_emitter())
                .await
                .unwrap();
            host.items
                .wait_for(|items| items.first().is_some_and(|item| item.title == "After"))
                .await
                .unwrap();
            drop(interface);
            drop(watcher);
            item_conn.close().await.unwrap();
            host.items.wait_for(Vec::is_empty).await.unwrap();
        })
        .await
        .expect("StatusNotifier lifecycle timed out");
    }
    #[test]
    fn pixmap_byte_order_and_bounds() {
        assert_eq!(
            rgba(&[(1, 1, vec![128, 200, 100, 50])], 22),
            Some((1, 1, vec![100, 50, 25, 128]))
        );
        assert!(rgba(&[(-1, 2, vec![])], 22).is_none());
        assert!(rgba(&[(2, 2, vec![0; 4])], 22).is_none());
    }
}
