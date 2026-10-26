use std::{
    collections::HashSet,
    fs::{self, File},
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{fs::MetadataExt, fs::OpenOptionsExt},
    },
    time::{Duration, Instant},
};

pub const TOKEN_VARIABLE: &str = "RIX_SESSION_TOKEN";
pub const GENERATION_VARIABLE: &str = "RIX_SESSION_GENERATION";

pub fn new_token() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn valid_token(token: &str) -> bool {
    token.len() == 32 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn environment_matches(environment: &[u8], token: &str, generation: Option<u32>) -> bool {
    let token = format!("{TOKEN_VARIABLE}={token}");
    let generation = generation.map(|value| format!("{GENERATION_VARIABLE}={value}"));
    let mut found_token = false;
    let mut found_generation = generation.is_none();
    for item in environment.split(|byte| *byte == 0) {
        found_token |= item == token.as_bytes();
        found_generation |= generation
            .as_ref()
            .is_some_and(|value| item == value.as_bytes());
    }
    found_token && found_generation
}

fn parent(pid: u32) -> Option<u32> {

    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn ancestors() -> HashSet<u32> {
    let mut protected = HashSet::new();
    let mut pid = std::process::id();
    while pid > 0 && protected.insert(pid) {
        let Some(next) = parent(pid) else { break };
        pid = next;
    }
    protected
}

fn open_pid(pid: u32) -> io::Result<OwnedFd> {

    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) as i32 };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn signal(fd: &OwnedFd, value: i32) -> io::Result<()> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            value,
            std::ptr::null::<libc::siginfo_t>(),
            0u32,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn finished(fd: &OwnedFd) -> bool {
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut poll, 1, 0) > 0 }
}

fn candidates(
    token: &str,
    generation: Option<u32>,
    protected: &HashSet<u32>,
) -> Vec<(u32, OwnedFd)> {
    let uid = unsafe { libc::geteuid() };
    let mut processes = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return processes;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid <= 1 || protected.contains(&pid) {
            continue;
        }
        let Ok(fd) = open_pid(pid) else { continue };
        let path = entry.path();
        if !fs::metadata(&path).is_ok_and(|metadata| metadata.uid() == uid) {
            continue;
        }
        let Ok(file) = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path.join("environ"))
        else {
            continue;
        };
        let mut environment = Vec::new();

        const MAX_ENVIRONMENT: u64 = 8 * 1024 * 1024;
        if file
            .take(MAX_ENVIRONMENT + 1)
            .read_to_end(&mut environment)
            .is_err()
            || environment.len() as u64 > MAX_ENVIRONMENT
        {
            continue;
        }
        if environment_matches(&environment, token, generation) {
            processes.push((pid, fd));
        }
    }
    processes
}

pub fn cleanup(token: &str, generation: Option<u32>, grace: Duration) -> io::Result<usize> {
    if !valid_token(token) || generation == Some(0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid Rix session attribution",
        ));
    }
    if unsafe { libc::geteuid() } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Rix process cleanup must run as the desktop user",
        ));
    }
    let protected = ancestors();
    let mut processes = candidates(token, generation, &protected);
    for (_, fd) in &processes {
        let _ = signal(fd, libc::SIGTERM);
    }
    let deadline = Instant::now() + grace.min(Duration::from_secs(2));
    while Instant::now() < deadline {

        processes.retain(|(_, fd)| !finished(fd));
        if processes.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let mut known = protected;
    known.extend(processes.iter().map(|(pid, _)| *pid));
    let more = candidates(token, generation, &known);
    processes.extend(more);
    let mut killed = 0;
    for (_, fd) in &processes {
        if signal(fd, libc::SIGKILL).is_ok() {
            killed += 1;
        }
    }
    Ok(killed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process::{Command, Stdio},
        thread,
    };

    #[test]
    fn markers_are_exact_and_generation_scoped() {
        let token = new_token().unwrap();
        assert!(valid_token(&token));
        let environment = format!("{TOKEN_VARIABLE}={token}\0{GENERATION_VARIABLE}=123\0");
        assert!(environment_matches(environment.as_bytes(), &token, None));
        assert!(environment_matches(
            environment.as_bytes(),
            &token,
            Some(123)
        ));
        assert!(!environment_matches(
            environment.as_bytes(),
            &token,
            Some(12)
        ));
        assert!(!environment_matches(
            format!("X{TOKEN_VARIABLE}={token}\0").as_bytes(),
            &token,
            None
        ));
        assert!(!valid_token("../other-session"));
        assert!(!valid_token(""));
    }

    #[test]
    fn exited_process_handles_cannot_signal_a_later_child() {
        let mut ended = Command::new("sleep").arg("30").spawn().unwrap();
        let handle = open_pid(ended.id()).unwrap();
        ended.kill().unwrap();
        ended.wait().unwrap();
        let mut later = Command::new("sleep").arg("30").spawn().unwrap();
        assert!(finished(&handle));
        assert_eq!(
            signal(&handle, libc::SIGKILL).unwrap_err().raw_os_error(),
            Some(libc::ESRCH)
        );
        assert!(later.try_wait().unwrap().is_none());
        later.kill().unwrap();
        later.wait().unwrap();
    }

    #[test]
    fn large_valid_build_environments_still_have_login_attribution() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let token = new_token().unwrap();
        let mut command = Command::new("sleep");
        command.arg("30").env_clear().env(TOKEN_VARIABLE, &token);
        for index in 0..16 {
            command.env(format!("RIX_BUILD_FIXTURE_{index}"), "x".repeat(96 * 1024));
        }
        let mut child = command.spawn().unwrap();
        cleanup(&token, None, Duration::from_millis(20)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut ended = child.try_wait().unwrap();
        while ended.is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
            ended = child.try_wait().unwrap();
        }
        if ended.is_none() {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        assert!(
            ended.is_some(),
            "A large compiler environment escaped login cleanup"
        );
    }

    #[test]
    fn detached_and_stubborn_clients_end_without_touching_another_login() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let token = new_token().unwrap();
        let spawn = |marker: &str, generation: &str, stubborn: bool| {
            Command::new("setsid")
                .args([
                    "sh",
                    "-c",
                    if stubborn {
                        "trap '' TERM; exec sleep 30"
                    } else {
                        "exec sleep 30"
                    },
                ])
                .env(TOKEN_VARIABLE, marker)
                .env(GENERATION_VARIABLE, generation)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        };
        let mut ordinary = spawn(&token, "123", false);
        let mut stubborn = spawn(&token, "123", true);
        let mut newer = spawn(&token, "456", true);
        let mut other = spawn(&new_token().unwrap(), "123", true);

        thread::sleep(Duration::from_millis(80));
        cleanup(&token, Some(123), Duration::from_millis(80)).unwrap();
        assert!(!ordinary.wait().unwrap().success());
        assert!(!stubborn.wait().unwrap().success());
        assert!(newer.try_wait().unwrap().is_none());
        assert!(other.try_wait().unwrap().is_none());
        cleanup(&token, None, Duration::from_millis(20)).unwrap();
        assert!(!newer.wait().unwrap().success());
        assert!(other.try_wait().unwrap().is_none());
        other.kill().unwrap();
        other.wait().unwrap();
    }
}
