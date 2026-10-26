fn main() {

    println!("cargo:rustc-env=SLINT_EMIT_DEBUG_INFO=1");
    use rspolib::TranslatedEntry;
    use std::{fmt::Write, path::PathBuf};
    let mut translations = String::from("fn lookup(language: &str, message: &str) -> Option<&'static str> { match (language, message) {\n");
    for language in ["en"] {
        let path = PathBuf::from(format!(
            "../../translations/{language}/LC_MESSAGES/rix-files.po"
        ));
        println!("cargo:rerun-if-changed={}", path.display());
        let catalog =
            rspolib::pofile(path.as_path()).expect("Invalid Rix Files translation catalog");
        for entry in catalog.entries.iter().filter(|entry| {
            entry.translated()
                && entry.msgctxt.as_deref().is_none_or(str::is_empty)
                && entry.msgid_plural.is_none()
        }) {
            if let Some(message) = &entry.msgstr {
                writeln!(
                    translations,
                    "({language:?}, {:?}) => Some({message:?}),",
                    entry.msgid
                )
                .unwrap();
            }
        }
    }
    translations.push_str("_ => None, } }\n");
    std::fs::write(
        PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("translations.rs"),
        translations,
    )
    .unwrap();
    let config = slint_build::CompilerConfiguration::new()
        .with_bundled_translations("../../translations")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("ui/files.slint", config)
        .expect("Failed to compile Rix Files UI");
}
