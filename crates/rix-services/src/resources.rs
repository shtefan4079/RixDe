use std::{io, os::unix::process::CommandExt, process::Command, time::Duration};

pub fn enabled() -> bool {
    std::env::var("RIX_RESOURCE_POLICY").as_deref() == Ok("1")
}

pub fn desktop_priority() {
    if !enabled() {
        return;
    }
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, -5) } != 0 {
        tracing::warn!(error = %io::Error::last_os_error(), "Desktop CPU priority unavailable");
    }
    std::env::set_var("RIX_RESOURCE_GENERATION", std::process::id().to_string());
}

fn reset_application_priority(background: bool) -> io::Result<()> {

    let inherited = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
    let priority = inherited.max(if background { 10 } else { 0 });
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, priority) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_NICE, &limit) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let fd = unsafe {
        libc::open(
            c"/proc/self/oom_score_adj".as_ptr(),
            libc::O_WRONLY | libc::O_CLOEXEC,
        )
    };
    if fd >= 0 {
        let result = unsafe { libc::write(fd, b"0".as_ptr().cast(), 1) };
        let error = io::Error::last_os_error();
        unsafe {
            libc::close(fd);
        }
        if result != 1 {
            return Err(error);
        }
    }
    Ok(())
}

pub fn ordinary_command(command: &mut Command) {
    if enabled() {
        unsafe {
            command.pre_exec(|| reset_application_priority(false));
        }
    }
}

pub fn apply_cursor_environment(command: &mut Command, theme: &str, size: u32) {
    command
        .env("XCURSOR_THEME", theme)
        .env("XCURSOR_SIZE", size.to_string());
}

pub fn prepare_application(background: bool) -> io::Result<()> {
    if enabled() || background {
        reset_application_priority(background)?;
    }
    Ok(())
}

fn generation() -> Option<String> {
    std::env::var("RIX_RESOURCE_GENERATION")
        .ok()
        .filter(|value| {
            !value.is_empty() && value.len() <= 10 && value.bytes().all(|b| b.is_ascii_digit())
        })
}

pub fn memory_high(total: u64) -> u64 {

    let reserve = (total / 10).clamp(256 * 1024 * 1024, 2 * 1024 * 1024 * 1024);
    total.saturating_sub(reserve).max(total / 2)
}

fn physical_memory() -> Option<u64> {
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    (pages > 0 && size > 0).then(|| (pages as u64).saturating_mul(size as u64))
}

pub async fn application_scope(background: bool) {
    if !enabled() {
        return;
    }
    let Some(generation) = generation() else {
        return;
    };
    let work = enter_scope(&generation, background);
    match tokio::time::timeout(Duration::from_secs(2), work).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            eprintln!(
                "rix-resources: application scope unavailable; using ordinary priority: {error:#}"
            );
        }
        Err(_) => eprintln!("rix-resources: application scope timed out; using ordinary priority"),
    }
}

async fn enter_scope(generation: &str, background: bool) -> anyhow::Result<()> {
    use zbus::zvariant::{OwnedObjectPath, Value};
    let uid = unsafe { libc::geteuid() };
    let address = format!("unix:path=/run/user/{uid}/bus");
    let connection = zbus::connection::Builder::address(address.as_str())?
        .method_timeout(Duration::from_millis(750))
        .build()
        .await?;
    let manager = zbus::Proxy::new(
        &connection,
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
    )
    .await?;
    let slice = format!("app-rix{generation}.slice");
    let aux: Vec<(&str, Vec<(&str, Value<'_>)>)> = Vec::new();
    let props = vec![
        ("Description", Value::from("Rix session applications")),
        ("CPUWeight", Value::from(100u64)),
        (
            "MemoryHigh",
            Value::from(memory_high(
                physical_memory().unwrap_or(4 * 1024 * 1024 * 1024),
            )),
        ),
    ];

    let exists = manager
        .call::<_, _, OwnedObjectPath>("GetUnit", &(slice.as_str(),))
        .await
        .is_ok();
    if !exists {
        let started: zbus::Result<OwnedObjectPath> = manager
            .call(
                "StartTransientUnit",
                &(slice.as_str(), "fail", &props, &aux),
            )
            .await;
        if let Err(error) = started {
            manager
                .call::<_, _, OwnedObjectPath>("GetUnit", &(slice.as_str(),))
                .await
                .map_err(|_| error)?;
        }
    }
    let class = if background {
        "background"
    } else {
        "foreground"
    };
    let scope = format!("app-rix{generation}-{class}.scope");
    let pids = vec![std::process::id()];
    if manager
        .call::<_, _, OwnedObjectPath>("GetUnit", &(scope.as_str(),))
        .await
        .is_ok()
    {
        manager
            .call::<_, _, ()>("AttachProcessesToUnit", &(scope.as_str(), "", &pids))
            .await?;
    } else {
        let props = vec![
            ("Description", Value::from("Rix application processes")),
            ("Slice", Value::from(slice.as_str())),
            (
                "CPUWeight",
                Value::from(if background { 25u64 } else { 100u64 }),
            ),
            (
                "IOWeight",
                Value::from(if background { 25u64 } else { 100u64 }),
            ),
            ("CollectMode", Value::from("inactive-or-failed")),

            ("Delegate", Value::from(true)),
            ("TimeoutStopUSec", Value::from(2_000_000u64)),
            ("SendSIGKILL", Value::from(true)),
            ("PIDs", Value::from(pids.clone())),
        ];
        let started: zbus::Result<OwnedObjectPath> = manager
            .call(
                "StartTransientUnit",
                &(scope.as_str(), "fail", &props, &aux),
            )
            .await;
        if let Err(error) = started {
            manager
                .call::<_, _, ()>("AttachProcessesToUnit", &(scope.as_str(), "", &pids))
                .await
                .map_err(|_| error)?;
        }
    }

    let expected = format!("/{scope}");
    loop {
        let cgroups = std::fs::read_to_string("/proc/self/cgroup")?;
        if cgroups
            .lines()
            .any(|line| line.starts_with("0::") && line.ends_with(&expected))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn memory_pressure_budget_scales_without_starving_small_machines() {
        let mib = 1024 * 1024;
        assert_eq!(memory_high(512 * mib), 256 * mib);
        assert_eq!(
            memory_high(8 * 1024 * mib),
            8 * 1024 * mib - 8 * 1024 * mib / 10
        );
        assert_eq!(memory_high(64 * 1024 * mib), 62 * 1024 * mib);
    }
    #[test]
    fn application_child_drops_the_desktop_scheduling_allowance() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "cat /proc/self/oom_score_adj; grep 'Max nice priority' /proc/self/limits",
        ]);
        unsafe {
            command.pre_exec(|| reset_application_priority(true));
        }
        let output = command.output().unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.starts_with("0\n"));
        assert!(text
            .lines()
            .last()
            .unwrap()
            .split_whitespace()
            .skip(3)
            .take(2)
            .all(|v| v == "0"));
    }
}
