fn main() {
    println!("cargo:rustc-env=SLINT_EMIT_DEBUG_INFO=1");
    slint_build::compile_with_config(
        "ui/clavis.slint",
        slint_build::CompilerConfiguration::new().with_debug_info(true),
    )
    .expect("Could not compile Clavis UI");
}
