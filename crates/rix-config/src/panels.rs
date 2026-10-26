use crate::LayoutConfig;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PanelTarget {
    pub panel: String,
    pub output: String,
}

pub fn validate_panels(layout: &LayoutConfig) -> Result<(), crate::ConfigError> {
    let mut ids = std::collections::HashSet::new();
    if layout.panels.len() > 16
        || layout
            .panels
            .iter()
            .any(|p| p.id.is_empty() || p.id.len() > 256 || !ids.insert(&p.id))
    {
        return Err(crate::ConfigError::InvalidValue(
            "Panels need unique non-empty IDs (maximum 16 panels)".into(),
        ));
    }
    Ok(())
}

pub fn panel_targets(layout: &LayoutConfig, outputs: &[String]) -> Vec<PanelTarget> {
    let mut targets = Vec::new();
    for panel in &layout.panels {
        for (index, output) in outputs.iter().enumerate() {
            let selected = match panel.screen.as_str() {
                "all" => true,

                "focused" | "primary" | "" => index == 0,
                name => name == output,
            };
            if selected {
                let target = PanelTarget {
                    panel: panel.id.clone(),
                    output: output.clone(),
                };
                if !targets.contains(&target) && targets.len() < 64 {
                    targets.push(target);
                }
            }
        }
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_panel_resolves_independently_and_missing_outputs_wait() {
        let mut layout = LayoutConfig::default_layout();
        let mut all = layout.panels[0].clone();
        all.id = "secondary".into();
        all.screen = "all".into();
        layout.panels.push(all);
        let mut specific = layout.panels[0].clone();
        specific.id = "dock".into();
        specific.screen = "DP-test".into();
        layout.panels.push(specific);
        let names = vec!["HDMI-test".into(), "DP-test".into()];
        let targets = panel_targets(&layout, &names);
        assert_eq!(
            targets
                .iter()
                .map(|t| (&*t.panel, &*t.output))
                .collect::<Vec<_>>(),
            [
                ("main-panel", "HDMI-test"),
                ("secondary", "HDMI-test"),
                ("secondary", "DP-test"),
                ("dock", "DP-test")
            ]
        );
        assert_eq!(panel_targets(&layout, &names[..1]).len(), 2);
        assert!(panel_targets(&layout, &[]).is_empty());
    }
}
