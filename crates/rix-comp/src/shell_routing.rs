use crate::state::RixState;
use rix_ipc::{IpcEvent, ShellEvent};

impl RixState {
    pub(crate) fn refresh_shell_targets(&mut self, layout: &rix_config::LayoutConfig) {
        self.shell_targets = rix_config::panel_targets(
            layout,
            &self.space.outputs().map(|o| o.name()).collect::<Vec<_>>(),
        );
        self.launcher_panels = layout
            .panels
            .iter()
            .filter(|p| {
                p.widgets
                    .iter()
                    .any(|w| w.kind == rix_config::WidgetKind::Launcher)
            })
            .map(|p| p.id.clone())
            .collect();
    }
    pub(crate) fn shell_output(&self) -> Option<String> {
        let pointer = self.seat.get_pointer().map(|p| p.current_location());
        self.space
            .outputs()
            .find(|o| {
                pointer.is_some_and(|p| {
                    self.space
                        .output_geometry(o)
                        .is_some_and(|g| g.to_f64().contains(p))
                })
            })
            .or_else(|| self.space.outputs().next())
            .map(|o| o.name())
    }
    pub(crate) fn publish_shell(&self, output: Option<&str>, event: ShellEvent) {
        let Some(sender) = &self.ipc_event_sender else {
            return;
        };
        if self.shell_targets.is_empty() {

            let event = match event {
                ShellEvent::LauncherToggle => IpcEvent::LauncherToggleRequested,
                ShellEvent::LauncherSet(open) => IpcEvent::LauncherToggled(open),
                ShellEvent::DesktopContextMenu { x, y } => IpcEvent::DesktopContextMenu { x, y },
                ShellEvent::SwitcherChanged(state) => IpcEvent::SwitcherChanged(state),
                ShellEvent::PowerAction(_) => return,
            };
            let _ = sender.send(event);
            return;
        }
        if let Some(target) = self.shell_target(
            output,
            matches!(
                event,
                ShellEvent::LauncherToggle | ShellEvent::LauncherSet(true)
            ),
        ) {
            let _ = sender.send(IpcEvent::Shell {
                target: target.clone(),
                event,
            });
        }
    }
    pub(crate) fn shell_target(
        &self,
        output: Option<&str>,
        launcher: bool,
    ) -> Option<&rix_config::PanelTarget> {
        let eligible = |target: &&rix_config::PanelTarget| {
            !launcher || self.launcher_panels.contains(&target.panel)
        };
        self.shell_targets
            .iter()
            .filter(eligible)
            .find(|t| Some(t.output.as_str()) == output)
            .or_else(|| self.shell_targets.iter().find(eligible))
    }
    pub(crate) fn toggle_launcher(&self) {
        self.publish_shell(self.shell_output().as_deref(), ShellEvent::LauncherToggle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::output::{Output, PhysicalProperties, Subpixel};
    #[test]
    fn launcher_chooses_one_panel_and_context_coordinates_remain_local() {
        let mut display = wayland_server::Display::<RixState>::new().unwrap();
        let mut state = RixState::new(&mut display, "test".into());
        for name in ["DP-test", "HDMI-test"] {
            let output = Output::new(
                name.into(),
                PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: Subpixel::Unknown,
                    make: "test".into(),
                    model: "test".into(),
                },
            );
            state.space.map_output(&output, (0, 0));
        }
        let mut layout = rix_config::LayoutConfig::default_layout();
        layout.panels[0].screen = "all".into();
        let mut second = layout.panels[0].clone();
        second.id = "second".into();
        layout.panels.push(second);
        state.refresh_shell_targets(&layout);
        let (sender, mut receiver) = tokio::sync::broadcast::channel(8);
        state.ipc_event_sender = Some(sender);
        state.publish_shell(Some("HDMI-test"), ShellEvent::LauncherToggle);
        let IpcEvent::Shell { target, event } = receiver.try_recv().unwrap() else {
            panic!("Untargeted event")
        };
        assert_eq!(
            (target.panel.as_str(), target.output.as_str()),
            ("main-panel", "HDMI-test")
        );
        assert_eq!(event, ShellEvent::LauncherToggle);
        assert!(
            receiver.try_recv().is_err(),
            "One action must not open every panel"
        );
        state.publish_shell(
            Some("DP-test"),
            ShellEvent::DesktopContextMenu { x: 30., y: 40. },
        );
        let IpcEvent::Shell { target, event } = receiver.try_recv().unwrap() else {
            panic!("Untargeted menu")
        };
        assert_eq!(target.output, "DP-test");
        assert_eq!(event, ShellEvent::DesktopContextMenu { x: 30., y: 40. });
    }
}
