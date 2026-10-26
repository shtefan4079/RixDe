fn main() {
    slint_build::compile_with_config(
        "ui/capture.slint",
        slint_build::CompilerConfiguration::new().with_style("fluent".into()),
    )
    .expect("Could not compile Rix Capture");
}
