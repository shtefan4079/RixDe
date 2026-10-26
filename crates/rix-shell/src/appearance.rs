use crate::{RixPanelWindow, Theme};
use rix_config::ThemeSettings;
use slint::ComponentHandle;
#[path = "../../../ui/common/bindings.rs"]
mod bindings;
pub fn apply_shell_theme(window: &RixPanelWindow, settings: &ThemeSettings) {
    crate::skins::install(window.global::<crate::SkinRenderer>(), window.as_weak());
    bindings::apply(
        window.global::<Theme>(),
        window.global::<crate::Palette>(),
        settings,
    );
    crate::popup::refresh_geometry(window);
    crate::layout::reflow(window);
}
