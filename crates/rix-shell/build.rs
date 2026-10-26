fn main() {
    println!("cargo:rerun-if-env-changed=RIX_SHELL_UI");
    let entry = std::env::var_os("RIX_SHELL_UI")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "ui/main.slint".into());
    let config = slint_build::CompilerConfiguration::new()
        .with_bundled_translations("../../translations")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config(&entry, config)
        .expect("Failed to compile shell skin; see docs/SKIN_API.md");
}
