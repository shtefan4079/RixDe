pub mod backup;
pub mod prompt;
pub mod ui;
slint::include_modules!();

#[path = "../../../ui/common/skins.rs"]
mod skins;
#[path = "../../../ui/common/bindings.rs"]
mod theme_bindings;

pub fn apply_manager_theme(window: &VaultWindow, settings: &rix_config::ThemeSettings) {
    use slint::ComponentHandle;
    skins::install(window.global::<SkinRenderer>(), window.as_weak());
    theme_bindings::apply_for_app(
        window.global::<Theme>(),
        window.global::<Palette>(),
        settings,
    );
}
pub fn apply_prompt_theme(window: &PasswordWindow, settings: &rix_config::ThemeSettings) {
    use slint::ComponentHandle;
    skins::install(window.global::<SkinRenderer>(), window.as_weak());
    theme_bindings::apply_for_app(
        window.global::<Theme>(),
        window.global::<Palette>(),
        settings,
    );
}
