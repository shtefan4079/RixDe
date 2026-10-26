fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    anyhow::ensure!(
        args.len() == 1 && args[0] == "--prepare-zoom",
        "Usage: rix-app-compat --prepare-zoom"
    );
    rix_services::applications::native_compat::prepare_installed_zoom()?;
    Ok(())
}
