use crate::state::RixState;
use rix_ipc::SwitcherState;
#[derive(Default)]
pub struct Switcher {
    pub target: Option<rix_config::PanelTarget>,
    pub mru: Vec<u64>,
    order: Vec<u64>,
    selected: usize,
    generation: u64,
}
impl Switcher {
    pub fn active(&self) -> bool {
        !self.order.is_empty()
    }
    pub fn focused(&mut self, id: u64) {
        self.mru.retain(|v| *v != id);
        self.mru.insert(0, id);
    }
    fn step(&mut self, candidates: &[u64], reverse: bool) {
        if !self.active() {
            self.order = self
                .mru
                .iter()
                .chain(candidates)
                .filter(|id| candidates.contains(id))
                .copied()
                .collect();
            let mut seen = std::collections::HashSet::new();
            self.order.retain(|id| seen.insert(*id));
            self.selected = 0;
            self.generation = self.generation.wrapping_add(1);
        }
        if !self.order.is_empty() {
            self.selected =
                (self.selected + if reverse { self.order.len() - 1 } else { 1 }) % self.order.len();
        }
    }
    fn prune(&mut self, live: &[u64]) -> bool {
        self.mru.retain(|id| live.contains(id));
        let selected = self.order.get(self.selected).copied();
        let length = self.order.len();
        self.order.retain(|id| live.contains(id));
        self.selected = selected
            .and_then(|id| self.order.iter().position(|other| *other == id))
            .unwrap_or(self.selected.min(self.order.len().saturating_sub(1)));
        length != self.order.len()
    }
    fn finish(&mut self, commit: bool) -> Option<u64> {
        let id = commit
            .then(|| self.order.get(self.selected).copied())
            .flatten();
        self.order.clear();
        self.selected = 0;
        self.generation = self.generation.wrapping_add(1);
        id
    }
}
impl RixState {
    pub fn switcher_snapshot(&self) -> SwitcherState {
        let windows = self.list_windows();
        SwitcherState {
            target: self.switcher.target.clone(),
            generation: self.switcher.generation,
            selected: self.switcher.selected as u32,
            windows: self
                .switcher
                .order
                .iter()
                .filter_map(|id| windows.iter().find(|w| w.id == *id).cloned())
                .collect(),
        }
    }
    fn publish_switcher(&self) {
        if let Some(sender) = &self.ipc_event_sender {
            if let Some(target) = &self.switcher.target {
                let _ = sender.send(rix_ipc::IpcEvent::Shell {
                    target: target.clone(),
                    event: rix_ipc::ShellEvent::SwitcherChanged(self.switcher_snapshot()),
                });
            } else {
                let _ = sender.send(rix_ipc::IpcEvent::SwitcherChanged(self.switcher_snapshot()));
            }
        }
    }
    pub fn step_switcher(&mut self, reverse: bool) {
        let candidates: Vec<_> = self
            .list_windows()
            .into_iter()
            .filter(|w| self.switcher_settings.include_minimized || !w.is_minimized)
            .map(|w| w.id)
            .collect();
        if !self.switcher.active() {
            self.switcher.target = self
                .shell_target(self.shell_output().as_deref(), false)
                .cloned();
            if let Some(sender) = &self.ipc_event_sender {
                let _ = sender.send(rix_ipc::IpcEvent::ShellPopupsDismissed);
            }
        }
        self.switcher.step(&candidates, reverse);
        self.publish_switcher();
    }
    pub fn finish_switcher(&mut self, commit: bool) {
        let id = self.switcher.finish(commit);
        self.publish_switcher();
        if let Some(id) = id {
            let _ = self.apply_window_action(rix_ipc::WindowAction::Focus(id));
        }
    }
    pub fn choose_switcher(&mut self, id: Option<u64>) -> Result<(), String> {
        if let Some(id) = id {
            self.switcher.selected = self
                .switcher
                .order
                .iter()
                .position(|v| *v == id)
                .ok_or("Window is not in the switcher")?;
        }
        self.finish_switcher(id.is_some());
        Ok(())
    }
    pub fn reconcile_switcher(&mut self) {
        let live: Vec<_> = self.windows.entries.iter().map(|(id, _)| *id).collect();
        if self.switcher.prune(&live) {
            self.publish_switcher();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mru_cycle_cancel_minimized_candidates_and_close_selected() {
        let mut switcher = Switcher::default();
        for id in [1, 2, 3] {
            switcher.focused(id);
        }
        switcher.step(&[1, 2, 3], false);
        assert_eq!(switcher.order, vec![3, 2, 1]);
        assert_eq!(switcher.selected, 1);
        switcher.step(&[1, 2, 3], true);
        assert_eq!(switcher.selected, 0);
        assert_eq!(switcher.finish(false), None);
        assert_eq!(switcher.mru, vec![3, 2, 1]);
        switcher.step(&[1, 2, 3], false);
        assert!(switcher.prune(&[1, 3]));
        assert_eq!(switcher.finish(true), Some(1));
        switcher.step(&[3], true);
        assert_eq!(switcher.finish(true), Some(3));
        switcher.step(&[], false);
        assert!(!switcher.active());
    }
}
