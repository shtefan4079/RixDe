use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "Native RixFiles backend for XDG Desktop Portal")]
struct Args {

    #[arg(long)]
    picker: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    anyhow::ensure!(
        unsafe { libc::geteuid() } != 0,
        "The file chooser portal must run as the desktop user"
    );
    let args = Args::parse();
    let sibling = std::env::current_exe()?.with_file_name("rix-files");
    let picker = args.picker.unwrap_or_else(|| {
        if sibling.is_file() {
            sibling
        } else {
            PathBuf::from("rix-files")
        }
    });
    rix_portal::backend::serve(picker).await
}
