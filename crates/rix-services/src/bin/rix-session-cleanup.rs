use std::time::Duration;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--new-token"] {
        println!("{}", rix_services::session_cleanup::new_token()?);
        return Ok(());
    }
    let generation = match args.as_slice() {
        [] => None,
        [flag, pid] if flag == "--generation" => Some(pid.parse::<u32>()?),
        _ => anyhow::bail!("Usage: rix-session-cleanup [--new-token | --generation PID]"),
    };
    let token = std::env::var(rix_services::session_cleanup::TOKEN_VARIABLE)?;
    rix_services::session_cleanup::cleanup(&token, generation, Duration::from_millis(700))?;
    Ok(())
}
