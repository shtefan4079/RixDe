use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{collections::BTreeMap, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::{mpsc, watch},
};

#[derive(Debug, Clone, PartialEq)]
pub struct AudioNode {
    pub key: String,
    pub app_id: String,
    pub binary: String,
    pub name: String,
    pub description: String,
    pub icon: String,
    pub kind: String,
    pub volume: u32,
    pub muted: bool,
    pub default: bool,
}
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub nodes: Vec<AudioNode>,
    pub error: String,
}
#[derive(Debug, Clone)]
pub enum Action {
    Volume(String, u32),
    Mute(String, bool),
    Default(String),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hotkey {
    VolumeUp,
    VolumeDown,
    ToggleMute,
    ToggleMicrophoneMute,
}

fn sink_volume(text: &str) -> Result<u32> {
    let values: Vec<u32> = text
        .split_ascii_whitespace()
        .filter_map(|part| part.strip_suffix('%')?.parse().ok())
        .collect();
    if values.is_empty() {
        bail!("Audio device did not report a volume");
    }
    Ok((values.iter().map(|value| u64::from(*value)).sum::<u64>() / values.len() as u64) as u32)
}

fn adjusted_volume(current: u32, delta: i32, maximum: u32) -> u32 {
    (i64::from(current) + i64::from(delta)).clamp(0, i64::from(maximum)) as u32
}

fn apply_volume_steps(current: u32, actions: &[Hotkey], maximum: u32) -> u32 {
    actions.iter().fold(current, |volume, action| {
        adjusted_volume(
            volume,
            if *action == Hotkey::VolumeUp { 5 } else { -5 },
            maximum,
        )
    })
}

async fn change_default_volume(actions: &[Hotkey], maximum: u32) -> Result<()> {
    let current = sink_volume(&pactl(&["get-sink-volume", "@DEFAULT_SINK@"]).await?)?;
    let volume = apply_volume_steps(current, actions, maximum);
    pactl(&["set-sink-volume", "@DEFAULT_SINK@", &format!("{volume}%")]).await?;
    Ok(())
}

pub async fn run_hotkeys(
    mut commands: mpsc::Receiver<Hotkey>,
    config: std::sync::Arc<rix_config::ConfigManager>,
) {
    while let Some(first) = commands.recv().await {
        let mut pending = vec![first];
        while let Ok(action) = commands.try_recv() {
            pending.push(action);
        }
        let maximum = config.get_layout().services.maximum_volume;
        let mut volume_steps = Vec::new();
        for action in pending {
            match action {
                Hotkey::VolumeUp | Hotkey::VolumeDown => volume_steps.push(action),
                Hotkey::ToggleMute | Hotkey::ToggleMicrophoneMute => {
                    if !volume_steps.is_empty() {
                        if let Err(error) = change_default_volume(&volume_steps, maximum).await {
                            tracing::warn!(%error, "Hardware volume control failed");
                        }
                        volume_steps.clear();
                    }
                    let args = if action == Hotkey::ToggleMute {
                        ["set-sink-mute", "@DEFAULT_SINK@", "toggle"]
                    } else {
                        ["set-source-mute", "@DEFAULT_SOURCE@", "toggle"]
                    };
                    if let Err(error) = pactl(&args).await {
                        tracing::warn!(%error, "Hardware mute control failed");
                    }
                }
            }
        }
        if !volume_steps.is_empty() {
            if let Err(error) = change_default_volume(&volume_steps, maximum).await {
                tracing::warn!(%error, "Hardware volume control failed");
            }
        }
    }
}
pub struct Service {
    pub updates: watch::Receiver<Snapshot>,
    pub commands: mpsc::Sender<Action>,
}

async fn pactl(args: &[&str]) -> Result<String> {
    let mut command = Command::new("pactl");
    command
        .args(args)
        .env("LC_ALL", "C")
        .kill_on_drop(true)
        .stdin(Stdio::null());
    let output = tokio::time::timeout(Duration::from_secs(4), command.output())
        .await
        .context("Audio service is not responding")??;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8(output.stdout)?)
}
fn parse_nodes(json: &str, kind: &str, default: &str) -> Result<Vec<AudioNode>> {
    let entries: Vec<Value> = serde_json::from_str(json)?;
    Ok(entries
        .into_iter()
        .filter(|e| {
            kind != "source" || !e["name"].as_str().unwrap_or_default().ends_with(".monitor")
        })
        .filter_map(|e| {
            let index = e["index"].as_u64()?;
            let name = e["name"].as_str().unwrap_or_default();
            let props = &e["properties"];
            let description = e["description"]
                .as_str()
                .or_else(|| props["application.name"].as_str())
                .or_else(|| props["media.name"].as_str())
                .unwrap_or(name);
            let volume = e["volume"]
                .as_object()
                .map(|channels| {
                    let values: Vec<_> = channels
                        .values()
                        .filter_map(|c| c["value"].as_f64())
                        .collect();
                    if values.is_empty() {
                        0
                    } else {
                        (values.iter().sum::<f64>() / values.len() as f64 * 100. / 65536.).round()
                            as u32
                    }
                })
                .unwrap_or_default();
            Some(AudioNode {
                app_id: props["application.id"].as_str().unwrap_or_default().into(),
                binary: props["application.process.binary"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
                key: format!("{kind}:{index}"),
                name: name.into(),
                description: description.into(),
                icon: props["application.icon_name"]
                    .as_str()
                    .unwrap_or(if kind == "source" {
                        "audio-input-microphone"
                    } else {
                        "audio-volume-high"
                    })
                    .into(),
                kind: kind.into(),
                volume,
                muted: e["mute"].as_bool().unwrap_or(false),
                default: name == default && !default.is_empty(),
            })
        })
        .collect())
}
pub async fn snapshot() -> Result<Snapshot> {
    let info: Value = serde_json::from_str(&pactl(&["-f", "json", "info"]).await?)?;
    let (sinks, sources, playback, recording) = tokio::try_join!(
        pactl(&["-f", "json", "list", "sinks"]),
        pactl(&["-f", "json", "list", "sources"]),
        pactl(&["-f", "json", "list", "sink-inputs"]),
        pactl(&["-f", "json", "list", "source-outputs"])
    )?;
    let mut nodes = parse_nodes(
        &sinks,
        "sink",
        info["default_sink_name"].as_str().unwrap_or_default(),
    )?;
    nodes.extend(parse_nodes(
        &sources,
        "source",
        info["default_source_name"].as_str().unwrap_or_default(),
    )?);
    nodes.extend(parse_nodes(&playback, "sink-input", "")?);
    nodes.extend(parse_nodes(&recording, "source-output", "")?);
    Ok(Snapshot {
        nodes,
        error: String::new(),
    })
}
async fn apply(action: Action, maximum_volume: u32) -> Result<()> {
    let key = match &action {
        Action::Volume(key, _) | Action::Mute(key, _) | Action::Default(key) => key,
    };
    let (kind, index) = key.split_once(':').context("Invalid audio node")?;
    if !["sink", "source", "sink-input", "source-output"].contains(&kind)
        || index.parse::<u32>().is_err()
    {
        bail!("Invalid audio node");
    }
    let (operation, value) = match action {
        Action::Volume(_, volume) => (
            format!("set-{kind}-volume"),
            format!("{}%", volume.min(maximum_volume.min(150))),
        ),
        Action::Mute(_, muted) => (
            format!("set-{kind}-mute"),
            if muted { "1" } else { "0" }.into(),
        ),
        Action::Default(_) => {
            if kind != "sink" && kind != "source" {
                bail!("Only devices can be defaults");
            }
            let list = pactl(&[
                "-f",
                "json",
                "list",
                if kind == "sink" { "sinks" } else { "sources" },
            ])
            .await?;
            let entries = parse_nodes(&list, kind, "")?;
            let name = entries
                .iter()
                .find(|node| node.key == format!("{kind}:{index}"))
                .map(|node| node.name.as_str())
                .context("Audio device is disconnected")?;
            pactl(&[&format!("set-default-{kind}"), name]).await?;
            return Ok(());
        }
    };
    pactl(&[&operation, index, &value]).await?;
    Ok(())
}
fn affects_audio_state(event: &str) -> bool {

    [
        "sink",
        "source",
        "sink-input",
        "source-output",
        "server",
        "card",
    ]
    .iter()
    .any(|kind| event.contains(&format!(" on {kind} #")))
}

struct SubscriptionTask(tokio::task::JoinHandle<()>);
impl Drop for SubscriptionTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn subscribe_events(program: std::ffi::OsString, events: mpsc::Sender<()>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if events.is_closed() {
            return;
        }
        let child = Command::new(&program)
            .arg("subscribe")
            .env("LC_ALL", "C")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        if let Ok(mut child) = child {
            let started = tokio::time::Instant::now();
            if let Some(stdout) = child.stdout.take() {
                let mut lines = BufReader::new(stdout).lines();

                let _ = events.try_send(());
                loop {
                    let line = tokio::select! {
                        _ = events.closed() => return,
                        line = lines.next_line() => line,
                    };
                    match line {
                        Ok(Some(event)) if affects_audio_state(&event) => {
                            if matches!(
                                events.try_send(()),
                                Err(mpsc::error::TrySendError::Closed(_))
                            ) {
                                return;
                            }
                        }
                        Ok(Some(_)) => {}
                        _ => break,
                    }
                }
            }
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            if started.elapsed() >= Duration::from_secs(10) {
                backoff = Duration::from_secs(1);
            }
        }
        tokio::select! {
            _ = events.closed() => return,
            _ = tokio::time::sleep(backoff) => {},
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

pub fn start(config: std::sync::Arc<rix_config::ConfigManager>) -> Service {
    let (sender, updates) = watch::channel(Snapshot::default());
    let (commands, mut actions) = mpsc::channel::<Action>(128);
    tokio::spawn(async move {
        let mut poll = tokio::time::interval(Duration::from_secs(5));
        let (events, mut changes) = mpsc::channel(1);
        let _subscriber = SubscriptionTask(tokio::spawn(subscribe_events("pactl".into(), events)));
        let mut error = String::new();
        loop {
            tokio::select! {
                _ = poll.tick() => {},
                event = changes.recv() => {
                    if event.is_none() { break; }

                    tokio::time::sleep(Duration::from_millis(60)).await;
                },
                action = actions.recv() => {
                    let Some(action) = action else { break };
                    let mut pending = vec![action];
                    while let Ok(action) = actions.try_recv() { pending.push(action); }
                    let mut volumes = BTreeMap::new();
                    let maximum_volume = config.get_layout().services.maximum_volume.min(150);
                    for action in pending {
                        if let Action::Volume(key, value) = action { volumes.insert(key, value); }
                        else if let Err(e) = apply(action, maximum_volume).await { error = e.to_string(); }
                    }
                    for (key, value) in volumes { if let Err(e) = apply(Action::Volume(key, value), maximum_volume).await { error = e.to_string(); } }
                }
            }
            let next = match snapshot().await {
                Ok(mut s) => {
                    s.error = std::mem::take(&mut error);
                    s
                }
                Err(e) => Snapshot {
                    error: e.to_string(),
                    ..Default::default()
                },
            };
            sender.send_if_modified(|old| {
                if *old == next {
                    false
                } else {
                    *old = next;
                    true
                }
            });
        }
    });
    Service { updates, commands }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscriber_reconnects_after_eof_and_stops_with_its_receiver() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("pactl-fixture");
        std::fs::write(&program, "#!/bin/sh\nprintf 'start\\n' >> \"${0}.attempts\"\nif [ \"$(wc -l < \"${0}.attempts\")\" -eq 1 ]; then exit 0; fi\nprintf \"Event 'change' on sink #25\\n\"\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (sender, mut receiver) = mpsc::channel(4);
        let task = tokio::spawn(subscribe_events(program.as_os_str().to_owned(), sender));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                receiver.recv().await.unwrap();
                if std::fs::read_to_string(program.with_extension("attempts"))
                    .unwrap_or_default()
                    .lines()
                    .count()
                    >= 2
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        drop(receiver);
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
    }
    #[test]
    fn subscriber_ignores_its_own_query_clients() {
        assert!(!affects_audio_state("Event 'new' on client #25"));
        assert!(!affects_audio_state("Event 'remove' on client #25"));
        assert!(affects_audio_state("Event 'change' on sink #25"));
        assert!(affects_audio_state("Event 'change' on server #4294967295"));
    }
    #[test]
    fn hardware_volume_uses_reported_channels_and_respects_limits() {
        assert_eq!(
            sink_volume(
                "Volume: front-left: 65536 / 80% / -2.0 dB, front-right: 65536 / 60% / -4.0 dB"
            )
            .unwrap(),
            70
        );
        assert!(sink_volume("No such entity").is_err());
        assert_eq!(adjusted_volume(98, 5, 100), 100);
        assert_eq!(adjusted_volume(3, -5, 100), 0);
        assert_eq!(adjusted_volume(100, 5, 150), 105);
        assert_eq!(adjusted_volume(50, -15, 100), 35);
        assert_eq!(
            apply_volume_steps(98, &[Hotkey::VolumeUp, Hotkey::VolumeDown], 100),
            95
        );
        assert_eq!(
            apply_volume_steps(3, &[Hotkey::VolumeDown, Hotkey::VolumeUp], 100),
            5
        );
    }
    #[test]
    fn parses_channel_volume_and_stream_identity() {
        let nodes = parse_nodes(r#"[{"index":42,"mute":true,"volume":{"left":{"value":65536},"right":{"value":32768}},"properties":{"application.name":"Player"}}]"#, "sink-input", "").unwrap();
        assert_eq!(nodes[0].key, "sink-input:42");
        assert_eq!(nodes[0].volume, 75);
        assert!(nodes[0].muted);
        assert_eq!(nodes[0].description, "Player");
    }
}
