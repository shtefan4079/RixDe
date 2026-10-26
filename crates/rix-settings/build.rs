fn main() {

    println!("cargo:rustc-env=SLINT_EMIT_DEBUG_INFO=1");
    let config = slint_build::CompilerConfiguration::new()
        .with_bundled_translations("../../translations")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("ui/settings.slint", config)
        .expect("Failed to compile Slint UI files for rix-settings");
}
