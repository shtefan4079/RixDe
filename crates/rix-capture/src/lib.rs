pub mod control;
pub mod editor;
pub mod encoding;
pub mod handoff;
pub mod text;
slint::include_modules!();

#[path = "../../../ui/common/skins.rs"]
mod skins;
#[path = "../../../ui/common/bindings.rs"]
mod theme_bindings;

pub fn apply_theme(window: &CaptureWindow, settings: &rix_config::ThemeSettings) {
    use slint::ComponentHandle;
    skins::install(window.global::<SkinRenderer>(), window.as_weak());
    theme_bindings::apply_for_app(
        window.global::<Theme>(),
        window.global::<Palette>(),
        settings,
    );
}
