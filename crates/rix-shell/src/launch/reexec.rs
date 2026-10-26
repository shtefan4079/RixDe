use std::{io, process::Command};

pub(crate) fn command() -> io::Result<Command> {
    #[cfg(target_os = "linux")]
    let executable = "/proc/self/exe";
    #[cfg(not(target_os = "linux"))]
    let executable = std::env::current_exe()?;
    Ok(Command::new(executable))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    const FIXTURE_STAGE: &str = "RIX_TEST_REEXEC_STAGE";
    const FIXTURE_BINARY: &str = "RIX_TEST_REEXEC_BINARY";
    const FIXTURE_MARKER: &str = "RIX_TEST_REEXEC_MARKER";
    static FIXTURE_LOCK: Mutex<()> = Mutex::new(());

    struct FixtureDirectory(PathBuf);

    impl Drop for FixtureDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture_name() -> String {

        let (_, module) = module_path!().split_once("::").unwrap();
        format!("{module}::reexec_fixture")
    }

    fn assert_reexec(stage: &str) {

        let _guard = FIXTURE_LOCK.lock().unwrap();
        let directory = FixtureDirectory(std::env::temp_dir().join(format!(
            "rix-shell-reexec-{}-{}-{stage}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
        fs::create_dir(&directory.0).unwrap();
        let binary = directory.0.join("running-shell");
        let marker = directory.0.join("launched");
        fs::copy(std::env::current_exe().unwrap(), &binary).unwrap();
        let result = Command::new(&binary)
            .args(["--exact", &fixture_name(), "--nocapture"])
            .env(FIXTURE_STAGE, stage)
            .env(FIXTURE_BINARY, &binary)
            .env(FIXTURE_MARKER, &marker)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "fixture failed: {}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(fs::read_to_string(marker).unwrap(), "original image");
    }

    #[test]
    fn reexec_survives_atomic_replacement() {
        assert_reexec("replace");
    }

    #[test]
    fn reexec_survives_unlink() {
        assert_reexec("unlink");
    }

    #[test]
    fn reexec_fixture() {
        let Some(stage) = std::env::var_os(FIXTURE_STAGE) else {
            return;
        };
        let marker = PathBuf::from(std::env::var_os(FIXTURE_MARKER).unwrap());
        if stage == "reexecuted" {
            fs::write(marker, "original image").unwrap();
            return;
        }
        let binary = PathBuf::from(std::env::var_os(FIXTURE_BINARY).unwrap());
        if stage == "replace" {
            let replacement = binary.with_file_name("replacement-shell");
            fs::write(&replacement, "#!/bin/sh\nexit 97\n").unwrap();
            fs::set_permissions(&replacement, fs::Permissions::from_mode(0o700)).unwrap();
            fs::rename(replacement, &binary).unwrap();
        } else {
            assert_eq!(stage, "unlink");
            fs::remove_file(binary).unwrap();
        }
        assert!(fs::read_link("/proc/self/exe")
            .unwrap()
            .to_string_lossy()
            .ends_with(" (deleted)"));
        let result = command()
            .unwrap()
            .args(["--exact", &fixture_name(), "--nocapture"])
            .env(FIXTURE_STAGE, "reexecuted")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "reexec failed: {}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
