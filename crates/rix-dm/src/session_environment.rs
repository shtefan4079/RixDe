use std::path::Path;

const SYSTEM_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
const USER_BIN_DIRECTORIES: [&str; 4] = [".local/bin", ".opencode/bin", ".cargo/bin", ".bun/bin"];

pub(crate) fn path(home: &Path, user_session: bool) -> String {

    let Some(home) = home.to_str().filter(|home| {
        user_session && Path::new(home).is_absolute() && !home.contains([':', '\0', '\n', '\r'])
    }) else {
        return SYSTEM_PATH.to_owned();
    };
    USER_BIN_DIRECTORIES
        .iter()
        .map(|directory| {
            Path::new(home)
                .join(directory)
                .to_string_lossy()
                .into_owned()
        })
        .chain(std::iter::once(SYSTEM_PATH.to_owned()))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_commands_resolve_in_the_authenticated_home() {
        let home = Path::new("/home/account with spaces");
        let entries: Vec<_> = path(home, true).split(':').map(str::to_owned).collect();
        assert_eq!(
            entries,
            [
                "/home/account with spaces/.local/bin",
                "/home/account with spaces/.opencode/bin",
                "/home/account with spaces/.cargo/bin",
                "/home/account with spaces/.bun/bin",
                "/usr/local/bin",
                "/usr/bin",
                "/bin",
            ]
        );
        assert!(!entries
            .iter()
            .any(|entry| entry.is_empty() || entry.starts_with("/root/")));
    }

    #[test]
    fn greeter_never_searches_an_account_home() {
        assert_eq!(path(Path::new("/var/lib/rix-greeter"), false), SYSTEM_PATH);
    }

    #[test]
    fn unrepresentable_homes_do_not_inject_search_directories() {
        for home in [
            "",
            ".",
            "relative/home",
            "/home/user:/untrusted",
            "/home/user\nname",
        ] {
            assert_eq!(path(Path::new(home), true), SYSTEM_PATH);
        }
    }
}
