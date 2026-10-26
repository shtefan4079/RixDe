use anyhow::{ensure, Context, Result};
use std::{fs, process::Command, time::Duration};

const CPU_WEIGHT: u64 = 600;
const IO_WEIGHT: u64 = 300;
const MAX_MEMORY_LOW: u64 = 512 * 1024 * 1024;
const NICE_LIMIT: libc::rlim_t = 25;

#[derive(Debug, PartialEq, Eq)]
struct ScopePolicy {
    memory_low: u64,
}

impl ScopePolicy {
    fn for_memory(total: u64) -> Self {
        Self {
            memory_low: MAX_MEMORY_LOW.min(total / 16),
        }
    }

    fn command(&self, scope: &str) -> Command {
        let mut command = Command::new("/usr/bin/systemctl");
        command
            .args([
                "--system",
                "--runtime",
                "--no-ask-password",
                "set-property",
                scope,
            ])
            .arg(format!("CPUWeight={CPU_WEIGHT}"))
            .arg(format!("MemoryLow={}", self.memory_low))
            .arg(format!("IOWeight={IO_WEIGHT}"));
        command
    }
}

fn memory_total(value: &str) -> Result<u64> {
    let mut fields = value
        .lines()
        .find(|line| line.starts_with("MemTotal:"))
        .context("MemTotal is missing")?
        .split_whitespace();
    ensure!(fields.next() == Some("MemTotal:"), "Invalid memory total");
    let kilobytes = fields.next().context("MemTotal is empty")?.parse::<u64>()?;
    ensure!(fields.next() == Some("kB"), "Unknown MemTotal unit");
    kilobytes.checked_mul(1024).context("MemTotal overflow")
}

pub(super) fn configure_scope(uid: u32, pid: i32, vt: u16, session: &str) -> Result<()> {
    let scope = super::logind::verified_session_scope(uid, pid, vt, "user", session)?;
    let total = fs::read_to_string("/proc/meminfo").context("Cannot read physical memory")?;
    let policy = ScopePolicy::for_memory(memory_total(&total)?);
    ensure!(
        super::bounded_command(&mut policy.command(&scope), Duration::from_secs(2))?,
        "systemd did not apply the session resource policy within two seconds"
    );
    eprintln!(
        "rix-dm: session {session} resource preference applied (CPU weight {CPU_WEIGHT}, memory.low {} bytes)",
        policy.memory_low
    );
    Ok(())
}

pub(super) unsafe fn prepare_child() {
    let limit = libc::rlimit {
        rlim_cur: NICE_LIMIT,
        rlim_max: NICE_LIMIT,
    };
    if libc::setrlimit(libc::RLIMIT_NICE, &limit) != 0 {
        warn_child(b"rix-dm: could not enable modest desktop CPU priority; continuing session\n");
    }

    let descriptor = libc::open(
        c"/proc/self/oom_score_adj".as_ptr(),
        libc::O_WRONLY | libc::O_CLOEXEC,
    );
    if descriptor < 0 {
        warn_child(b"rix-dm: could not open desktop OOM preference; continuing session\n");
        return;
    }
    let value = b"-200\n";
    let written = libc::write(descriptor, value.as_ptr().cast(), value.len());
    libc::close(descriptor);
    if written != value.len() as libc::ssize_t {
        warn_child(b"rix-dm: could not set desktop OOM preference; continuing session\n");
    }
}

unsafe fn warn_child(message: &[u8]) {
    let _ = libc::write(libc::STDERR_FILENO, message.as_ptr().cast(), message.len());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[test]
    fn advisory_memory_budget_is_small_on_small_and_large_systems() {
        assert_eq!(
            ScopePolicy::for_memory(1024 * 1024 * 1024).memory_low,
            64 * 1024 * 1024
        );
        assert_eq!(
            ScopePolicy::for_memory(16 * 1024 * 1024 * 1024).memory_low,
            MAX_MEMORY_LOW
        );
        assert_eq!(ScopePolicy::for_memory(0).memory_low, 0);
        assert_eq!(
            memory_total("MemAvailable: 7 kB\nMemTotal: 8192 kB\n").unwrap(),
            8 * 1024 * 1024
        );
        assert!(memory_total("MemTotal: 8192 MB\n").is_err());
        assert!(memory_total("MemTotal: 18446744073709551615 kB\n").is_err());
        assert!(memory_total("MemAvailable: 8192 kB\n").is_err());
    }

    #[test]
    fn preference_changes_only_the_supplied_scope_at_runtime() {
        let command = ScopePolicy::for_memory(16 * 1024 * 1024 * 1024).command("session-42.scope");
        assert_eq!(command.get_program(), "/usr/bin/systemctl");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "--system",
                "--runtime",
                "--no-ask-password",
                "set-property",
                "session-42.scope",
                "CPUWeight=600",
                "MemoryLow=536870912",
                "IOWeight=300"
            ]
        );
    }

    #[test]
    fn unavailable_child_privileges_do_not_prevent_exec_or_change_the_parent() {
        if unsafe { libc::geteuid() } == 0 {

            return;
        }
        let before_oom = fs::read_to_string("/proc/self/oom_score_adj").unwrap();
        let before_nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
        let mut before_limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NICE, &mut before_limit) },
            0
        );
        let mut command = Command::new("/bin/true");
        unsafe {
            command.pre_exec(|| {
                prepare_child();
                Ok(())
            });
        }
        assert!(command.status().unwrap().success());
        assert_eq!(
            fs::read_to_string("/proc/self/oom_score_adj").unwrap(),
            before_oom
        );
        assert_eq!(
            unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) },
            before_nice
        );
        let mut after_limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NICE, &mut after_limit) },
            0
        );
        assert_eq!(
            (after_limit.rlim_cur, after_limit.rlim_max),
            (before_limit.rlim_cur, before_limit.rlim_max)
        );
    }
}
