use crate::{RixPanelWindow, TaskbarItemData};
use rix_config::ConfigManager;
use slint::{ComponentHandle, Model};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

const MAX_SAVED_APPS: usize = 4096;
const MAX_ORDER_BYTES: u64 = 1024 * 1024;

struct InstalledOrder {
    window: slint::Weak<RixPanelWindow>,
    state: Rc<RefCell<Order>>,
}

thread_local! {
    static ORDERS: RefCell<HashMap<usize, InstalledOrder>> = RefCell::new(HashMap::new());
}

fn window_key(window: &RixPanelWindow) -> usize {
    window.window() as *const slint::Window as usize
}

pub fn order_file_path(config: &ConfigManager) -> PathBuf {
    config.config_dir().join("taskbar-order.json")
}

fn app_key(id: &str) -> String {

    id.trim().to_ascii_lowercase()
}

fn unique_order(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .map(|value| app_key(&value))
        .filter(|value| !value.is_empty() && seen.insert(value.clone()))
        .take(MAX_SAVED_APPS)
        .collect()
}

fn load_order(path: &Path) -> Vec<String> {

    let result = (|| {
        let metadata = std::fs::metadata(path).ok()?;
        if !metadata.is_file() || metadata.len() > MAX_ORDER_BYTES {
            return None;
        }
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice::<Vec<String>>(&bytes).ok()
    })();
    unique_order(result.unwrap_or_default())
}

#[derive(Clone, Debug)]
struct Row {
    app: String,
    id: i32,
    windows: Vec<i32>,
    pinned: bool,
}

impl Row {
    fn from_item(item: &TaskbarItemData) -> Self {
        Self {
            app: app_key(item.app_id.as_str()),
            id: item.id,
            windows: item.windows.iter().map(|window| window.id).collect(),
            pinned: item.is_pinned,
        }
    }

    fn same_window(&self, other: &Self) -> bool {
        self.app == other.app
            && (self.id == other.id || self.windows.iter().any(|id| other.windows.contains(id)))
    }
}

#[derive(Default)]
struct Order {
    apps: Vec<String>,
    previous: Vec<Row>,
}

impl Order {
    fn loaded(apps: Vec<String>) -> Self {
        Self {
            apps,
            ..Self::default()
        }
    }

    fn reconcile(&mut self, items: Vec<TaskbarItemData>) -> Vec<TaskbarItemData> {
        let rows: Vec<_> = items.iter().map(Row::from_item).collect();
        for row in &rows {
            if !row.app.is_empty() && !self.apps.contains(&row.app) {
                self.apps.push(row.app.clone());
            }
        }
        let mut used = HashSet::new();
        let mut matched = Vec::new();
        let mut added = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let previous = self
                .previous
                .iter()
                .enumerate()
                .find(|(old, old_row)| !used.contains(old) && row.same_window(old_row));
            let previous = previous.or_else(|| {
                self.previous.iter().enumerate().find(|(old, old_row)| {
                    !used.contains(old)
                        && !row.app.is_empty()
                        && row.app == old_row.app
                        && (row.pinned && old_row.pinned
                            || rows.iter().filter(|other| other.app == row.app).count() == 1)
                })
            });
            if let Some((old, _)) = previous {
                used.insert(old);
                matched.push((old, index));
            } else {
                added.push(index);
            }
        }
        matched.sort_by_key(|(old, _)| *old);
        let mut indices: Vec<_> = matched.into_iter().map(|(_, index)| index).collect();
        for index in added {
            let row = &rows[index];

            let position = indices
                .iter()
                .rposition(|other| !row.app.is_empty() && rows[*other].app == row.app)
                .map(|position| position + 1)
                .unwrap_or_else(|| {
                    let rank = self
                        .apps
                        .iter()
                        .position(|app| app == &row.app)
                        .unwrap_or(usize::MAX);
                    indices
                        .iter()
                        .position(|other| {
                            self.apps
                                .iter()
                                .position(|app| app == &rows[*other].app)
                                .unwrap_or(usize::MAX)
                                > rank
                        })
                        .unwrap_or(indices.len())
                });
            indices.insert(position, index);
        }
        let mut source: Vec<_> = items.into_iter().map(Some).collect();
        let items: Vec<_> = indices
            .into_iter()
            .map(|index| source[index].take().unwrap())
            .collect();
        self.previous = items.iter().map(Row::from_item).collect();
        items
    }

    fn moved(
        &mut self,
        mut items: Vec<TaskbarItemData>,
        source: i32,
        destination: i32,
    ) -> Option<Vec<TaskbarItemData>> {
        let source = items.iter().position(|item| item.id == source)?;
        let destination = usize::try_from(destination).ok()?;
        if destination >= items.len() || source == destination {
            return None;
        }
        let item = items.remove(source);
        items.insert(destination, item);
        let visible = unique_order(items.iter().map(|item| item.app_id.to_string()));
        self.apps = merge_visible_order(&self.apps, &visible);
        self.previous = items.iter().map(Row::from_item).collect();
        Some(items)
    }
}

fn merge_visible_order(known: &[String], visible: &[String]) -> Vec<String> {
    let visible_set: HashSet<_> = visible.iter().collect();
    let mut remaining = visible.iter();
    let mut order: Vec<_> = known
        .iter()
        .map(|app| {
            if visible_set.contains(app) {
                remaining.next().unwrap_or(app).clone()
            } else {
                app.clone()
            }
        })
        .collect();
    order.extend(remaining.cloned());
    order
}

pub fn apply(window: &RixPanelWindow, items: Vec<TaskbarItemData>) -> Vec<TaskbarItemData> {
    ORDERS.with(|orders| {
        let mut orders = orders.borrow_mut();
        orders.retain(|_, order| order.window.upgrade().is_some());
        let order = orders
            .entry(window_key(window))
            .or_insert_with(|| InstalledOrder {
                window: window.as_weak(),
                state: Rc::new(RefCell::new(Order::default())),
            });
        let result = order.state.borrow_mut().reconcile(items);
        result
    })
}

fn save(config: &ConfigManager, order: &Order, items: &[TaskbarItemData]) -> anyhow::Result<()> {
    std::fs::create_dir_all(config.config_dir())?;
    let json = serde_json::to_vec_pretty(&order.apps)?;
    rix_config::write_atomic(&order_file_path(config), &json)?;
    let visible = crate::pins::collect_pinned_execs(items);
    let pins = merge_visible_order(&crate::pins::load_pinned_execs(config), &visible);
    crate::pins::save_pinned_execs(config, &pins)?;
    Ok(())
}

pub fn install(window: &RixPanelWindow, config: Arc<ConfigManager>) {
    let state = Rc::new(RefCell::new(Order::loaded(load_order(&order_file_path(
        &config,
    )))));
    ORDERS.with(|orders| {
        let mut orders = orders.borrow_mut();
        orders.retain(|_, order| order.window.upgrade().is_some());
        orders.insert(
            window_key(window),
            InstalledOrder {
                window: window.as_weak(),
                state: state.clone(),
            },
        );
    });
    let weak = window.as_weak();
    window.on_taskbar_drag_started(move |id| {
        let Some(window) = weak.upgrade() else { return };
        if !window.get_taskbar_items().iter().any(|item| item.id == id) {
            return;
        }
        window.invoke_preview_dismiss();
        crate::popup::dismiss_transient(&window);
    });
    let weak = window.as_weak();
    window.on_taskbar_item_moved(move |id, index| {
        let Some(window) = weak.upgrade() else { return };
        let items = window.get_taskbar_items().iter().collect();
        let Some(items) = state.borrow_mut().moved(items, id, index) else {
            return;
        };

        if let Err(error) = save(&config, &state.borrow(), &items) {
            tracing::error!(%error, "Could not save taskbar order");
            window.set_launcher_message(
                format!(
                    "{}: {error}",
                    crate::i18n::tr("Could not save taskbar order")
                )
                .into(),
            );
        }
        crate::set_taskbar(&window, items);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TaskWindowData;
    use slint::{ModelRc, VecModel};

    fn task(app: &str, id: i32, pinned: bool, members: &[i32]) -> TaskbarItemData {
        TaskbarItemData {
            app_id: app.into(),
            id,
            is_pinned: pinned,
            is_running: !members.is_empty(),
            windows: ModelRc::from(Rc::new(VecModel::from(
                members
                    .iter()
                    .map(|id| TaskWindowData {
                        id: *id,
                        ..Default::default()
                    })
                    .collect::<Vec<_>>(),
            ))),
            ..Default::default()
        }
    }

    fn ids(items: &[TaskbarItemData]) -> Vec<i32> {
        items.iter().map(|item| item.id).collect()
    }

    #[test]
    fn snapshots_keep_mixed_pinned_running_and_distinct_same_app_rows() {
        let mut order = Order::default();
        let input = vec![
            task("a", 1, true, &[1]),
            task("a", 2, false, &[2]),
            task("b", 3, false, &[3]),
            task("c", -4, true, &[]),
        ];
        let items = order.reconcile(input.clone());
        let items = order.moved(items, 2, 3).unwrap();
        assert_eq!(ids(&items), [1, 3, -4, 2]);
        for _ in 0..4 {
            assert_eq!(ids(&order.reconcile(input.clone())), [1, 3, -4, 2]);
        }
        let mut changed = input.clone();
        changed[0].is_active = true;
        changed[0].audio_volume = 88;
        let result = order.reconcile(changed);
        assert_eq!(ids(&result), [1, 3, -4, 2]);
        assert!(result[0].is_active);
        assert_eq!(result[0].audio_volume, 88);
        assert_eq!(result.iter().filter(|item| item.is_pinned).count(), 2);
    }

    #[test]
    fn grouped_representative_and_pinned_launch_close_keep_positions() {
        let mut order = Order::default();
        let input = vec![task("a", -1, true, &[]), task("b", 8, false, &[8])];
        let items = order.reconcile(input);
        order.moved(items, 8, 0).unwrap();
        assert_eq!(
            ids(&order.reconcile(vec![
                task("A", 10, true, &[10, 11]),
                task("b", 8, false, &[8])
            ])),
            [8, 10]
        );
        assert_eq!(
            ids(&order.reconcile(vec![
                task("a", 11, true, &[10, 11]),
                task("b", 8, false, &[8])
            ])),
            [8, 11]
        );
        assert_eq!(
            ids(&order.reconcile(vec![task("a", -20, true, &[]), task("b", 8, false, &[8])])),
            [8, -20]
        );

        order.reconcile(vec![task("a", -20, true, &[])]);
        assert_eq!(
            ids(&order.reconcile(vec![
                task("a", -20, true, &[]),
                task("b", 100, false, &[100])
            ])),
            [100, -20]
        );
    }

    #[test]
    fn new_window_joins_its_app_and_grouping_reuses_the_first_app_slot() {
        let mut order = Order::default();
        let items = order.reconcile(vec![task("a", 1, false, &[1]), task("b", 3, false, &[3])]);
        order.moved(items, 3, 0).unwrap();
        assert_eq!(
            ids(&order.reconcile(vec![
                task("a", 2, false, &[2]),
                task("b", 3, false, &[3]),
                task("a", 1, false, &[1])
            ])),
            [3, 1, 2]
        );
        assert_eq!(
            ids(&order.reconcile(vec![
                task("a", 2, false, &[1, 2]),
                task("b", 3, false, &[3])
            ])),
            [3, 2]
        );
    }

    #[test]
    fn moving_visible_apps_retains_closed_and_unavailable_app_slots() {
        let known = vec!["closed".into(), "a".into(), "missing".into(), "b".into()];
        assert_eq!(
            merge_visible_order(&known, &["b".into(), "a".into(), "new".into()]),
            ["closed", "b", "missing", "a", "new"]
        );
    }

    #[test]
    fn canonical_telegram_ids_remain_distinct_and_unknown_apps_append() {
        let mut order = Order::loaded(vec!["org.telegram.desktop".into(), "org.telegram".into()]);
        let result = order.reconcile(vec![
            task("org.telegram", 1, false, &[1]),
            task("org.telegram.desktop", 2, false, &[2]),
        ]);
        assert_eq!(ids(&result), [2, 1]);
        let result = order.reconcile(vec![
            task("", 3, false, &[3]),
            task("org.telegram", 1, false, &[1]),
            task("org.telegram.desktop", 2, false, &[2]),
            task("", 4, false, &[4]),
        ]);
        assert_eq!(ids(&result), [2, 1, 3, 4]);
        assert_eq!(order.apps, ["org.telegram.desktop", "org.telegram"]);
    }

    #[tokio::test]
    async fn installed_callbacks_save_reload_and_do_not_leak_between_windows() {
        i_slint_backend_testing::init_no_event_loop();
        let directory = tempfile::tempdir().unwrap();
        let config = Arc::new(ConfigManager::init_in(directory.path().to_path_buf()).unwrap());
        crate::pins::save_pinned_execs(&config, &["a".into(), "unavailable".into(), "c".into()])
            .unwrap();
        let window = RixPanelWindow::new().unwrap();
        install(&window, config.clone());
        let items = vec![
            task("a", -1, true, &[]),
            task("b", 8, false, &[8]),
            task("c", -3, true, &[]),
        ];
        crate::set_taskbar(&window, items.clone());
        window.invoke_taskbar_drag_started(-1);
        assert!(!order_file_path(&config).exists());
        window.invoke_taskbar_item_moved(999, 0);
        window.invoke_taskbar_item_moved(-1, 0);
        assert!(!order_file_path(&config).exists());
        window.invoke_taskbar_item_moved(-3, 0);
        assert_eq!(
            ids(&window.get_taskbar_items().iter().collect::<Vec<_>>()),
            [-3, -1, 8]
        );
        assert_eq!(load_order(&order_file_path(&config)), ["c", "a", "b"]);
        assert_eq!(
            crate::pins::load_pinned_execs(&config),
            ["c", "unavailable", "a"]
        );
        let restored = RixPanelWindow::new().unwrap();
        install(&restored, config.clone());
        crate::set_taskbar(&restored, items.clone());
        assert_eq!(
            ids(&restored.get_taskbar_items().iter().collect::<Vec<_>>()),
            [-3, -1, 8]
        );
        let fresh = RixPanelWindow::new().unwrap();
        crate::set_taskbar(&fresh, items);
        assert_eq!(
            ids(&fresh.get_taskbar_items().iter().collect::<Vec<_>>()),
            [-1, 8, -3]
        );
    }

    #[test]
    fn malformed_or_oversized_order_is_ignored_and_ids_are_normalized() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("order.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(load_order(&path).is_empty());
        std::fs::write(&path, r#"["A", "a", "", "org.telegram.desktop", "b"]"#).unwrap();
        assert_eq!(load_order(&path), ["a", "org.telegram.desktop", "b"]);
        std::fs::write(&path, vec![b' '; MAX_ORDER_BYTES as usize + 1]).unwrap();
        assert!(load_order(&path).is_empty());
    }
}
