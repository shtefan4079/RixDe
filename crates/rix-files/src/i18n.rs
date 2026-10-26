#[allow(dead_code)]
#[path = "../../../ui/common/i18n.rs"]
mod common;
include!(concat!(env!("OUT_DIR"), "/translations.rs"));
thread_local! {
    static LANGUAGE: std::cell::Cell<&'static str> = const { std::cell::Cell::new("en") };
}
pub fn select_language(preference: &str) -> Result<(), String> {
    let language = common::language_code(preference);
    slint::select_bundled_translation(language).map_err(|e| e.to_string())?;
    LANGUAGE.with(|value| value.set(language));
    Ok(())
}
pub fn tr(message: &str) -> slint::SharedString {
    LANGUAGE.with(|language| lookup(language.get(), message).unwrap_or(message).into())
}
