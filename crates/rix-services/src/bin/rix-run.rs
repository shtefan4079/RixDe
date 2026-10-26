use std::os::unix::process::CommandExt;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1).peekable();
    if args
        .peek()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        println!("Usage: rix-run [--background] -- command [arguments…]\nRun heavy builds with --background to keep interactive applications responsive.");
        return Ok(());
    }
    let background = args.peek().is_some_and(|arg| arg == "--background");
    if background {
        args.next();
    }
    if args.peek().is_some_and(|arg| arg == "--") {
        args.next();
    }
    let Some(program) = args.next() else {
        anyhow::bail!("Usage: rix-run [--background] -- command [arguments…]");
    };
    rix_services::applications::native_compat::prepare(std::path::Path::new(&program))?;
    rix_services::resources::prepare_application(background)?;
    rix_services::resources::application_scope(background).await;
    let error = std::process::Command::new(program).args(args).exec();
    Err(error.into())
}
