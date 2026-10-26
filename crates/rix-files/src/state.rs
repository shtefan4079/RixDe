use crate::{
    fs::{Entry, SortBy},
    selection::{EntryLayout, SelectionRect},
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc},
};

pub struct Tab {
    pub id: u64,
    pub path: PathBuf,
    pub entries: Vec<Entry>,
    pub selected: HashSet<PathBuf>,
    pub anchor: Option<usize>,
    pub focus: Option<usize>,
    pub query: String,
    pub show_hidden: bool,
    pub sort: SortBy,
    pub descending: bool,
    pub loading: bool,
    pub search_limited: bool,
    pub error: Option<String>,
    history: Vec<PathBuf>,
    cursor: usize,
    pub generation: u64,
    pub cancellation: Arc<AtomicBool>,
    rectangle: Option<RectangleSelection>,
}

struct RectangleSelection {
    original: HashSet<PathBuf>,
    anchor: Option<usize>,
    focus: Option<usize>,
    toggle: bool,
    extend: bool,
}
impl Tab {
    pub fn new(id: u64, path: PathBuf, show_hidden: bool, sort: SortBy) -> Self {
        Self {
            id,
            path: path.clone(),
            entries: vec![],
            selected: HashSet::new(),
            anchor: None,
            focus: None,
            query: String::new(),
            show_hidden,
            sort,
            descending: false,
            loading: false,
            search_limited: false,
            error: None,
            history: vec![path],
            cursor: 0,
            generation: 0,
            cancellation: Arc::new(AtomicBool::new(false)),
            rectangle: None,
        }
    }
    pub fn navigate(&mut self, path: PathBuf) {
        if self.path != path || !self.query.is_empty() {
            self.clear_snapshot();
        }
        if self.path != path {
            self.history.truncate(self.cursor + 1);
            self.history.push(path.clone());
            if self.history.len() > 256 {
                self.history.remove(0);
            }
            self.cursor = self.history.len() - 1;
            self.path = path;
        }
        self.query.clear();
        self.error = None;
    }
    pub fn can_back(&self) -> bool {
        self.cursor > 0
    }
    pub fn resolve_location(&mut self, path: PathBuf) {
        if path == self.path {
            return;
        }
        self.selected = self
            .selected
            .iter()
            .map(|selected| {
                selected
                    .strip_prefix(&self.path)
                    .map(|relative| path.join(relative))
                    .unwrap_or_else(|_| selected.clone())
            })
            .collect();
        self.path = path.clone();
        self.history[self.cursor] = path;
    }
    pub fn can_forward(&self) -> bool {
        self.cursor + 1 < self.history.len()
    }
    pub fn history_step(&mut self, forward: bool) -> bool {
        if (forward && !self.can_forward()) || (!forward && !self.can_back()) {
            return false;
        }
        if forward {
            self.cursor += 1;
        } else {
            self.cursor -= 1;
        }
        self.path = self.history[self.cursor].clone();
        self.query.clear();
        self.error = None;
        self.clear_snapshot();
        true
    }
    fn clear_snapshot(&mut self) {
        self.rectangle = None;
        self.entries.clear();
        self.selected.clear();
        self.focus = None;
        self.anchor = None;
    }
    pub fn install_entries(&mut self, entries: Vec<Entry>) {

        self.rectangle = None;

        let focused = self
            .focus
            .and_then(|i| self.entries.get(i))
            .map(|e| e.path.clone());
        let anchored = self
            .anchor
            .and_then(|i| self.entries.get(i))
            .map(|e| e.path.clone());
        self.entries = entries;
        self.selected
            .retain(|path| self.entries.iter().any(|e| &e.path == path));
        self.focus = focused.and_then(|path| self.entries.iter().position(|e| e.path == path));
        self.anchor = anchored.and_then(|path| self.entries.iter().position(|e| e.path == path));
    }
    pub fn select(&mut self, index: usize, ctrl: bool, shift: bool) {
        self.rectangle = None;
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        if shift {
            let anchor = self.anchor.unwrap_or(index).min(self.entries.len() - 1);
            if !ctrl {
                self.selected.clear();
            }
            for e in &self.entries[anchor.min(index)..=anchor.max(index)] {
                self.selected.insert(e.path.clone());
            }
        } else if ctrl {
            if !self.selected.remove(&entry.path) {
                self.selected.insert(entry.path.clone());
            }
            self.anchor = Some(index);
        } else {
            self.selected.clear();
            self.selected.insert(entry.path.clone());
            self.anchor = Some(index);
        }
        self.focus = Some(index);
    }
    pub fn begin_rectangle(&mut self, ctrl: bool, shift: bool) {
        self.rectangle = Some(RectangleSelection {
            original: self.selected.clone(),
            anchor: self.anchor,
            focus: self.focus,
            toggle: ctrl,
            extend: shift,
        });
        if !ctrl && !shift {
            self.selected.clear();
            self.anchor = None;
            self.focus = None;
        }
    }

    pub fn update_rectangle(&mut self, rect: SelectionRect, layout: EntryLayout) -> bool {
        let Some(gesture) = &self.rectangle else {
            return false;
        };
        let indices = layout.intersecting(rect, self.entries.len());
        let mut selected = if gesture.toggle || gesture.extend {
            gesture.original.clone()
        } else {
            HashSet::new()
        };
        for &index in &indices {
            let path = &self.entries[index].path;
            if !gesture.toggle || !selected.remove(path) {
                selected.insert(path.clone());
            }
        }
        if let Some(&index) = indices.first() {
            self.focus = Some(index);
            if !gesture.toggle && !gesture.extend {
                self.anchor = Some(index);
            }
        }
        let changed = self.selected != selected;
        self.selected = selected;
        changed
    }
    pub fn end_rectangle(&mut self, cancelled: bool) {
        if let Some(gesture) = self.rectangle.take() {
            if cancelled {
                self.selected = gesture.original;
                self.anchor = gesture.anchor;
                self.focus = gesture.focus;
            }
        }
    }
    pub fn selected_paths(&self) -> Vec<PathBuf> {
        self.entries
            .iter()
            .filter(|e| self.selected.contains(&e.path))
            .map(|e| e.path.clone())
            .collect()
    }
    pub fn selected_one(&self) -> Option<&Path> {
        (self.selected.len() == 1)
            .then(|| self.selected.iter().next())
            .flatten()
            .map(PathBuf::as_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(names: &[&str]) -> Vec<Entry> {
        names
            .iter()
            .map(|name| Entry {
                path: Path::new("/a").join(name),
                name: (*name).into(),
                is_dir: false,
                is_symlink: false,
                size: 0,
                modified: None,
                kind: "File".into(),
                icon_name: "text-x-generic".into(),
                icon_names: Vec::new(),
            })
            .collect()
    }

    #[test]
    fn navigation_removes_entries_from_the_previous_location() {
        let mut tab = Tab::new(1, "/a".into(), false, SortBy::Name);
        tab.install_entries(snapshot(&["file"]));
        tab.select(0, false, false);
        tab.navigate("/b".into());
        assert!(tab.entries.is_empty());
        assert!(tab.selected_paths().is_empty());
        assert!(tab.focus.is_none());
        assert!(tab.anchor.is_none());

        tab.install_entries(snapshot(&["other-file"]));
        tab.error = Some("old failure".into());
        assert!(tab.history_step(false));
        assert!(tab.entries.is_empty());
        assert!(tab.error.is_none());
    }

    #[test]
    fn leaving_search_clears_results_even_in_the_same_location() {
        let mut tab = Tab::new(1, "/a".into(), false, SortBy::Name);
        tab.query = "file".into();
        tab.install_entries(snapshot(&["nested/file"]));
        tab.select(0, false, false);
        tab.navigate("/a".into());
        assert!(tab.entries.is_empty());
        assert!(tab.selected.is_empty());
        assert!(tab.query.is_empty());
    }

    #[test]
    fn refresh_preserves_focus_and_range_anchor_by_path() {
        let mut tab = Tab::new(1, "/a".into(), false, SortBy::Name);
        tab.install_entries(snapshot(&["b", "c", "d"]));
        tab.select(1, false, false);
        tab.install_entries(snapshot(&["a", "b", "c", "d"]));
        assert_eq!(tab.focus, Some(2));
        assert_eq!(tab.anchor, Some(2));
        tab.select(3, false, true);
        assert_eq!(
            tab.selected_paths(),
            vec![PathBuf::from("/a/c"), PathBuf::from("/a/d")]
        );
        tab.install_entries(snapshot(&["a", "b"]));
        assert!(tab.focus.is_none());
        assert!(tab.anchor.is_none());
        assert!(tab.selected.is_empty());
    }

    #[test]
    fn navigation_discards_forward_history_and_selection() {
        let mut t = Tab::new(1, "/a".into(), false, SortBy::Name);
        t.navigate("/b".into());
        t.navigate("/c".into());
        assert!(t.history_step(false));
        assert!(t.can_forward());
        t.selected.insert("/b/file".into());
        t.navigate("/d".into());
        assert!(!t.can_forward());
        assert!(t.selected.is_empty());
        assert!(t.history_step(false));
        assert_eq!(t.path, Path::new("/b"));
    }
    #[test]
    fn selection_range_and_toggle() {
        let root = tempfile::tempdir().unwrap();
        for name in ["a", "b", "c"] {
            std::fs::write(root.path().join(name), "").unwrap();
        }
        let mut t = Tab::new(1, root.path().into(), false, SortBy::Name);
        t.install_entries(
            crate::fs::read_directory(
                root.path(),
                false,
                SortBy::Name,
                false,
                &AtomicBool::new(false),
            )
            .unwrap(),
        );
        t.select(0, false, false);
        t.select(2, false, true);
        assert_eq!(t.selected.len(), 3);
        t.select(1, true, false);
        assert_eq!(t.selected.len(), 2);
        t.select(99, false, false);
        assert_eq!(t.selected.len(), 2);
    }

    #[test]
    fn rectangle_recomputes_ctrl_toggle_from_initial_selection_and_escape_restores_focus() {
        let mut tab = Tab::new(1, "/a".into(), false, SortBy::Name);
        tab.install_entries(snapshot(&["a", "b", "c", "d"]));
        tab.select(1, false, false);
        tab.select(3, true, false);
        tab.begin_rectangle(true, false);
        let layout = EntryLayout {
            width: 500.,
            height: 40.,
            gap: 0.,
            columns: 1,
        };
        let rect = SelectionRect {
            x1: 20.,
            y1: 50.,
            x2: 90.,
            y2: 100.,
        };
        assert!(tab.update_rectangle(rect, layout));
        assert_eq!(
            tab.selected_paths(),
            vec![PathBuf::from("/a/c"), PathBuf::from("/a/d")]
        );
        assert!(
            !tab.update_rectangle(rect, layout),
            "Repeating a move cannot toggle again"
        );
        tab.update_rectangle(SelectionRect { y2: 70., ..rect }, layout);
        assert_eq!(tab.selected_paths(), vec![PathBuf::from("/a/d")]);
        tab.end_rectangle(true);
        assert_eq!(
            tab.selected_paths(),
            vec![PathBuf::from("/a/b"), PathBuf::from("/a/d")]
        );
        assert_eq!(tab.focus, Some(3));
        assert_eq!(tab.anchor, Some(3));
    }

    #[test]
    fn rectangle_shift_adds_and_refresh_cannot_apply_old_geometry_to_new_entries() {
        let mut tab = Tab::new(1, "/a".into(), false, SortBy::Name);
        tab.install_entries(snapshot(&["a", "b", "c"]));
        tab.select(2, false, false);
        let layout = EntryLayout {
            width: 80.,
            height: 80.,
            gap: 10.,
            columns: 2,
        };
        let rect = SelectionRect {
            x1: 5.,
            y1: 5.,
            x2: 15.,
            y2: 15.,
        };
        tab.begin_rectangle(false, true);
        tab.update_rectangle(rect, layout);
        assert_eq!(
            tab.selected_paths(),
            vec![PathBuf::from("/a/a"), PathBuf::from("/a/c")]
        );
        tab.end_rectangle(false);
        tab.begin_rectangle(false, false);
        assert!(tab.selected.is_empty());
        tab.install_entries(snapshot(&["new-file"]));
        assert!(!tab.update_rectangle(rect, layout));
        assert!(tab.selected.is_empty());
    }
}
