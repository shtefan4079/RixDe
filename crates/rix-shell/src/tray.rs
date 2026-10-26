use crate::{RixPanelWindow, TrayItemData};
use rix_services::status_notifier::{self, TrayAction, TrayCommand, TrayHost, TrayItem};
use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use std::{collections::HashMap, path::Path, rc::Rc, time::Duration};
use tokio::sync::mpsc;
static ICON_SIZE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(22);

pub(crate) fn system_modules(
    configured: &[String],
    clipboard_enabled: bool,
    capture_active: bool,
) -> Vec<String> {
    let mut modules: Vec<_> = configured
        .iter()
        .filter(|module| module.as_str() != "clipboard" && module.as_str() != "capture")
        .cloned()
        .collect();
    if capture_active {
        modules.insert(0, "capture".into());
    }
    if clipboard_enabled {
        modules.push("clipboard".into());
    }
    modules
}

pub fn set_icon_size(size: u32) {
    let size = size.clamp(8, 256);
    if ICON_SIZE.swap(size, std::sync::atomic::Ordering::Relaxed) != size {
        invalidate_icons();
    }
}

static ICON_THEME_CHANGED: tokio::sync::Notify = tokio::sync::Notify::const_new();
static ICON_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
pub fn invalidate_icons() {

    ICON_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    ICON_THEME_CHANGED.notify_one();
}

type TrayImageCache = HashMap<String, (TrayItem, Option<Image>)>;

thread_local! {

    static TRAY_IMAGES: std::cell::RefCell<(u64, TrayImageCache)> =
        std::cell::RefCell::new((0, HashMap::new()));
}

pub(crate) struct DecodedRow {
    pub(crate) item: TrayItem,
    pub(crate) tile: Option<rix_assets::Tile>,
}

#[derive(Clone, Copy)]
pub(crate) struct BackoffPolicy {
    pub(crate) initial: Duration,
    pub(crate) max: Duration,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
        }
    }
}

#[derive(Default)]
struct Decoder {
    cache: HashMap<String, (TrayItem, Option<rix_assets::Tile>)>,
    resolver: rix_assets::IconResolver,
    theme: String,
    size: u32,
}

impl Decoder {
    fn invalidate(&mut self) {
        self.cache.clear();
        self.theme.clear();
        self.size = 0;
    }

    fn decode(&mut self, items: Vec<TrayItem>, theme: &str, size: u32) -> Vec<DecodedRow> {
        if self.theme != theme || self.size != size {
            self.cache.clear();
            self.resolver = rix_assets::IconResolver::default();
            self.theme = theme.to_string();
            self.size = size;
        }
        self.cache
            .retain(|address, _| items.iter().any(|item| &item.address == address));
        items
            .into_iter()
            .map(|item| {
                let tile = match self.cache.get(&item.address) {
                    Some((old, tile)) if old == &item => tile.clone(),
                    _ => {
                        let extra = (!item.icon_theme_path.is_empty())
                            .then(|| Path::new(&item.icon_theme_path));
                        let tile = self
                            .resolver
                            .resolve(&item.icon_name, theme, size, extra)
                            .and_then(|path| rix_assets::rasterize_icon(&path, size).ok())
                            .or_else(|| {
                                status_notifier::rgba(&item.pixmaps, size).and_then(
                                    |(width, height, pixels)| {
                                        rix_assets::resize_icon(
                                            rix_assets::Tile {
                                                width,
                                                height,
                                                pixels,
                                            },
                                            size,
                                        )
                                        .ok()
                                    },
                                )
                            });
                        self.cache
                            .insert(item.address.clone(), (item.clone(), tile.clone()));
                        tile
                    }
                };
                DecodedRow { item, tile }
            })
            .collect()
    }
}

pub(crate) async fn supervise_hosts(
    mut new_host: impl FnMut() -> TrayHost,
    mut actions: mpsc::Receiver<TrayCommand>,
    backoff: BackoffPolicy,
    mut publish: impl FnMut(Vec<DecodedRow>),
) {
    let mut decoder = Decoder::default();
    let mut delay = backoff.initial;
    loop {
        let host = new_host();
        let mut updates = host.items;
        let commands = host.commands;
        let connected = std::time::Instant::now();
        loop {
            tokio::select! {
                changed = updates.changed() => { if changed.is_err() { break; } }
                action = actions.recv() => {
                    let Some(command) = action else { return };
                    let _ = commands.try_send(command);
                    continue;
                }
                _ = ICON_THEME_CHANGED.notified() => decoder.invalidate(),
            }
            let items = updates.borrow_and_update().clone();
            let theme = crate::apps::get_active_icon_theme();
            let size = ICON_SIZE.load(std::sync::atomic::Ordering::Relaxed);
            let decoded = tokio::task::spawn_blocking(move || {
                let rows = decoder.decode(items, &theme, size);
                (rows, decoder)
            })
            .await;
            match decoded {
                Ok((rows, next_decoder)) => {
                    decoder = next_decoder;
                    publish(rows);
                }
                Err(error) => {

                    tracing::warn!("Tray icon decoding failed: {error}");
                    decoder = Decoder::default();
                    break;
                }
            }
        }

        publish(Vec::new());
        if connected.elapsed() >= Duration::from_secs(30) {
            delay = backoff.initial;
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(backoff.max);
    }
}

pub(crate) fn apply_rows(window: &RixPanelWindow, rows: Vec<DecodedRow>) {
    let native_network = window
        .get_system_modules()
        .iter()
        .any(|module| module == "network");
    let generation = ICON_GENERATION.load(std::sync::atomic::Ordering::Relaxed);
    let items: Vec<TrayItemData> = TRAY_IMAGES.with(|cache| {
        let mut guard = cache.borrow_mut();
        if guard.0 != generation {
            guard.0 = generation;
            guard.1.clear();
        }
        let cache = &mut guard.1;
        cache.retain(|address, _| rows.iter().any(|row| &row.item.address == address));
        rows.into_iter()
            .filter(|row| !(native_network && row.item.id.rsplit('.').next() == Some("nm-applet")))
            .map(|row| {
                let icon = match cache.get(&row.item.address) {
                    Some((old, icon)) if old == &row.item => icon.clone(),
                    _ => {
                        let icon = row.tile.as_ref().map(|tile| {
                            Image::from_rgba8_premultiplied(
                                SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                                    &tile.pixels,
                                    tile.width,
                                    tile.height,
                                ),
                            )
                        });
                        cache.insert(row.item.address.clone(), (row.item.clone(), icon.clone()));
                        icon
                    }
                };
                let has_icon = icon.is_some();
                TrayItemData {
                    address: row.item.address.into(),
                    title: row.item.title.into(),
                    menu_only: row.item.menu_only,
                    has_icon,
                    icon: icon.unwrap_or_default(),
                }
            })
            .collect()
    });
    let current = window.get_tray_items();
    if let Some(model) = current.as_any().downcast_ref::<VecModel<TrayItemData>>() {
        let unchanged = model.row_count() == items.len()
            && model.iter().zip(&items).all(|(old, new)| {
                old.address == new.address
                    && old.title == new.title
                    && old.menu_only == new.menu_only
                    && old.has_icon == new.has_icon
                    && old.icon == new.icon
            });
        if unchanged {
            return;
        }
        while model.row_count() > items.len() {
            model.remove(model.row_count() - 1);
        }
        for (index, item) in items.into_iter().enumerate() {
            if index < model.row_count() {
                let old = model.row_data(index).unwrap();
                if old.address != item.address
                    || old.title != item.title
                    || old.menu_only != item.menu_only
                    || old.has_icon != item.has_icon
                    || old.icon != item.icon
                {
                    model.set_row_data(index, item);
                }
            } else {
                model.push(item);
            }
        }
    } else {
        window.set_tray_items(ModelRc::from(Rc::new(VecModel::from(items))));
    }
    crate::layout::reflow(window);
}

pub fn install(window: &RixPanelWindow) {

    let (actions, incoming) = mpsc::channel::<TrayCommand>(32);
    let menus = crate::tray_menu::install(window, actions.clone());
    let weak = window.as_weak();
    let clicks = actions.clone();
    window.on_tray_action(move |address, action, x, y| {
        if action == 1 {
            if let Some(window) = weak.upgrade() {
                menus.open(&window, address.to_string(), x, y);
            }
            return;
        }
        let action = match action {
            1 => TrayAction::ContextMenu,
            2 => TrayAction::SecondaryActivate,
            _ => TrayAction::Activate,
        };
        let _ = clicks.try_send(TrayCommand {
            address: address.to_string(),
            action,
        });
    });
    window.on_tray_scroll(move |address, delta| {
        let _ = actions.try_send(TrayCommand {
            address: address.to_string(),
            action: TrayAction::Scroll(delta),
        });
    });
    let weak = window.as_weak();
    tokio::spawn(async move {
        let publish = move |rows: Vec<DecodedRow>| {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    apply_rows(&window, rows);
                }
            });
        };
        supervise_hosts(
            status_notifier::start,
            incoming,
            BackoffPolicy::default(),
            publish,
        )
        .await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    fn item(address: &str, title: &str) -> TrayItem {
        TrayItem {
            address: address.into(),
            id: format!("applet-{address}"),
            title: title.into(),
            icon_name: String::new(),
            icon_theme_path: String::new(),
            pixmaps: Vec::new(),
            passive: false,
            menu_only: false,
        }
    }

    fn row(address: &str, title: &str) -> DecodedRow {
        DecodedRow {
            item: item(address, title),
            tile: None,
        }
    }

    #[derive(Default)]
    struct FakeBus {
        hosts: Mutex<Vec<Option<tokio::sync::watch::Sender<Vec<TrayItem>>>>>,
        commands: Mutex<Vec<mpsc::Receiver<TrayCommand>>>,
    }

    impl FakeBus {
        fn host(&self) -> TrayHost {
            let (items, receiver) = tokio::sync::watch::channel(Vec::new());
            let (commands, incoming) = mpsc::channel(4);
            self.hosts.lock().unwrap().push(Some(items));
            self.commands.lock().unwrap().push(incoming);
            TrayHost {
                items: receiver,
                commands,
            }
        }

        fn publish(&self, index: usize, items: Vec<TrayItem>) {
            self.hosts.lock().unwrap()[index]
                .as_ref()
                .unwrap()
                .send_replace(items);
        }

        fn drop_host(&self, index: usize) {
            self.hosts.lock().unwrap()[index] = None;
        }
    }

    async fn wait_for(mut condition: impl FnMut() -> bool, context: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(tokio::time::Instant::now() < deadline, "{context}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn host_loss_clears_the_tray_and_reconnects_with_a_fresh_registration() {
        let bus = Arc::new(FakeBus::default());
        let created = Arc::new(AtomicUsize::new(0));
        let frames = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let factory = {
            let bus = bus.clone();
            let created = created.clone();
            move || {
                created.fetch_add(1, Ordering::SeqCst);
                bus.host()
            }
        };
        let (actions, incoming) = mpsc::channel(4);
        let publish = {
            let frames = frames.clone();
            move |rows: Vec<DecodedRow>| {
                frames
                    .lock()
                    .unwrap()
                    .push(rows.into_iter().map(|row| row.item.title.clone()).collect());
            }
        };
        let task = tokio::spawn(supervise_hosts(
            factory,
            incoming,
            BackoffPolicy {
                initial: Duration::from_millis(10),
                max: Duration::from_millis(20),
            },
            publish,
        ));
        wait_for(|| created.load(Ordering::SeqCst) == 1, "first host").await;
        bus.publish(0, vec![item("host-a/1", "First")]);
        wait_for(
            || {
                frames
                    .lock()
                    .unwrap()
                    .last()
                    .is_some_and(|f| f == &["First"])
            },
            "first snapshot",
        )
        .await;

        bus.drop_host(0);
        wait_for(
            || frames.lock().unwrap().last().is_some_and(Vec::is_empty),
            "empty tray after host loss",
        )
        .await;
        wait_for(|| created.load(Ordering::SeqCst) == 2, "reconnect").await;
        bus.publish(
            1,
            vec![item("host-a/1", "First"), item("host-b/2", "Second")],
        );
        wait_for(
            || {
                frames
                    .lock()
                    .unwrap()
                    .last()
                    .is_some_and(|f| f == &["First", "Second"])
            },
            "full resync after reconnect",
        )
        .await;

        actions
            .try_send(TrayCommand {
                address: "host-a/1".into(),
                action: TrayAction::Activate,
            })
            .unwrap();
        let mut receiver = bus.commands.lock().unwrap().remove(1);
        let forwarded = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .expect("action was not forwarded")
            .expect("action channel closed");
        assert_eq!(forwarded.address, "host-a/1");

        drop(actions);
        task.await.unwrap();
    }

    #[test]
    fn tray_rows_update_the_existing_delegate_model_in_place() {
        i_slint_backend_testing::init_no_event_loop();
        let window = RixPanelWindow::new().unwrap();
        window.show().unwrap();
        apply_rows(
            &window,
            vec![row("host-a/1", "First"), row("host-b/2", "Second")],
        );
        let original = window.get_tray_items();
        let model = original
            .as_any()
            .downcast_ref::<VecModel<TrayItemData>>()
            .unwrap();
        assert_eq!(model.row_count(), 2);
        apply_rows(
            &window,
            vec![row("host-a/1", "First renamed"), row("host-b/2", "Second")],
        );
        let updated = window.get_tray_items();
        assert!(std::ptr::eq(
            model,
            updated
                .as_any()
                .downcast_ref::<VecModel<TrayItemData>>()
                .unwrap()
        ));
        assert_eq!(model.row_data(0).unwrap().title, "First renamed");
        apply_rows(&window, vec![row("host-b/2", "Second")]);
        assert_eq!(model.row_count(), 1);
        assert_eq!(model.row_data(0).unwrap().address, "host-b/2");
    }

    #[test]
    fn system_modules_keep_enabled_clipboard_and_live_capture() {
        let configured = vec!["keyboard".into(), "audio".into(), "network".into()];
        assert_eq!(
            system_modules(&configured, true, true),
            ["capture", "keyboard", "audio", "network", "clipboard"]
        );
        assert_eq!(
            system_modules(&configured, false, false),
            ["keyboard", "audio", "network"]
        );
    }
}
