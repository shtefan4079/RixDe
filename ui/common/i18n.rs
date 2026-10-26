use slint::SharedString;

pub fn language_code(_preference: &str) -> &'static str {
    "en"
}

pub fn select_language(preference: &str) -> Result<(), String> {
    slint::select_bundled_translation(language_code(preference)).map_err(|error| error.to_string())
}

pub fn tr(message: &str) -> SharedString {
    slint::private_unstable_api::translate(
        message.into(),
        "".into(),
        env!("CARGO_PKG_NAME").into(),
        slint::private_unstable_api::re_exports::Slice::from(&[][..]),
        0,
        "".into(),
    )
}
