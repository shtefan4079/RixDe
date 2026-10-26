use futures_util::StreamExt;
use rix_config::{
    power::{HardwareProfile, SleepAction},
    ConfigManager,
};
use rix_ipc::PowerState;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{mpsc, watch};
use zbus::{zvariant::OwnedValue, Connection, Proxy};
const UPOWER: &str = "org.freedesktop.UPower";
const UPOWER_PATH: &str = "/org/freedesktop/UPower";
const PPD: &str = "org.freedesktop.UPower.PowerProfiles";
const PPD_PATH: &str = "/org/freedesktop/UPower/PowerProfiles";
const LOGIN: &str = "org.freedesktop.login1";
const LOGIN_PATH: &str = "/org/freedesktop/login1";

#[derive(Clone, Debug)]
pub struct SleepRequest {
    pub action: SleepAction,
    pub valid: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

pub async fn run(
    config: Arc<ConfigManager>,
    tx: watch::Sender<PowerState>,
    server: Arc<rix_ipc::IpcServer>,
    mut sleep: mpsc::Receiver<SleepRequest>,
) {
    loop {
        let result = async {
            let conn = zbus::connection::Builder::system()?
                .method_timeout(Duration::from_secs(3))
                .build()
                .await?;
            watch_on(&conn, config.clone(), &tx, &server, &mut sleep).await
        }
        .await;
        if let Err(error) = result {
            publish(
                &tx,
                &server,
                PowerState {
                    native: true,
                    error: error.to_string(),
                    ..Default::default()
                },
            );
        } else {
            return;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}
fn publish(tx: &watch::Sender<PowerState>, server: &rix_ipc::IpcServer, state: PowerState) {
    tx.send_replace(state.clone());
    server.publish_power_state(state);
}
async fn proxy<'a>(
    conn: &'a Connection,
    service: &'a str,
    path: &'a str,
    iface: &'a str,
) -> zbus::Result<Proxy<'a>> {
    zbus::proxy::Builder::new(conn)
        .destination(service)?
        .path(path)?
        .interface(iface)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
}
async fn snapshot(upower: &Proxy<'_>, ppd: &Proxy<'_>, login: &Proxy<'_>) -> PowerState {
    let on_battery = upower.get_property::<bool>("OnBattery").await.ok();
    let profiles = ppd
        .get_property::<Vec<HashMap<String, OwnedValue>>>("Profiles")
        .await
        .unwrap_or_default();
    let blocked = login
        .get_property::<String>("BlockInhibited")
        .await
        .unwrap_or_default();
    let mut state = PowerState {
        native: true,
        on_battery,
        available_profiles: profiles
            .into_iter()
            .filter_map(|mut p| p.remove("Profile").and_then(|v| String::try_from(v).ok()))
            .collect(),
        active_profile: ppd.get_property("ActiveProfile").await.unwrap_or_default(),
        performance_degraded: ppd
            .get_property("PerformanceDegraded")
            .await
            .unwrap_or_default(),
        can_suspend: login
            .call::<_, _, String>("CanSuspend", &())
            .await
            .is_ok_and(|v| matches!(v.as_str(), "yes" | "challenge")),
        can_hibernate: login
            .call::<_, _, String>("CanHibernate", &())
            .await
            .is_ok_and(|v| matches!(v.as_str(), "yes" | "challenge")),
        sleep_inhibited: blocked.split(':').any(|s| s == "sleep" || s == "idle"),
        idle_inhibited: blocked.split(':').any(|s| s == "idle"),
        ..Default::default()
    };
    if state.available_profiles.is_empty() {
        state.error = "Power Profiles service is unavailable".into();
    }
    state
}
async fn apply_profile(
    ppd: &Proxy<'_>,
    state: &PowerState,
    profile: HardwareProfile,
) -> anyhow::Result<()> {
    let Some(id) = profile.id() else {
        return Ok(());
    };
    anyhow::ensure!(
        state.available_profiles.iter().any(|p| p == id),
        "Hardware profile {id} is unavailable"
    );
    if state.active_profile != id {
        ppd.set_property("ActiveProfile", id).await?;
    }
    Ok(())
}
async fn sleep_on(
    login: &Proxy<'_>,
    action: SleepAction,
    state: &PowerState,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !state.sleep_inhibited,
        "Automatic sleep is inhibited by an application"
    );
    let (method, available) = match action {
        SleepAction::Suspend => ("Suspend", state.can_suspend),
        SleepAction::Hibernate => ("Hibernate", state.can_hibernate),
    };
    anyhow::ensure!(available, "{method} is unavailable");

    login.call::<_, _, ()>(method, &(false)).await?;
    Ok(())
}
async fn watch_on(
    conn: &Connection,
    config: Arc<ConfigManager>,
    tx: &watch::Sender<PowerState>,
    server: &rix_ipc::IpcServer,
    sleep: &mut mpsc::Receiver<SleepRequest>,
) -> anyhow::Result<()> {
    let upower = proxy(conn, UPOWER, UPOWER_PATH, UPOWER).await?;
    let ppd = proxy(conn, PPD, PPD_PATH, PPD).await?;
    let login = proxy(conn, LOGIN, LOGIN_PATH, "org.freedesktop.login1.Manager").await?;
    let upower_props = proxy(conn, UPOWER, UPOWER_PATH, "org.freedesktop.DBus.Properties").await?;
    let ppd_props = proxy(conn, PPD, PPD_PATH, "org.freedesktop.DBus.Properties").await?;
    let login_props = proxy(conn, LOGIN, LOGIN_PATH, "org.freedesktop.DBus.Properties").await?;
    let mut upower_events = upower_props.receive_signal("PropertiesChanged").await?;
    let mut ppd_events = ppd_props.receive_signal("PropertiesChanged").await?;
    let mut login_events = login_props.receive_signal("PropertiesChanged").await?;
    let dbus = zbus::fdo::DBusProxy::new(conn).await?;
    let mut owners = dbus.receive_name_owner_changed().await?;
    let mut changes = config.subscribe_desktop();
    let mut last_plan = None;
    let mut operation_error = String::new();
    let mut retry = tokio::time::interval(Duration::from_secs(30));
    loop {
        let mut state = snapshot(&upower, &ppd, &login).await;
        let desktop = config.get_desktop();
        let desired = desktop
            .power
            .as_ref()
            .map(|power| power.active(state.on_battery).profile);

        let selection = (desktop.power.clone(), state.on_battery);
        if last_plan.as_ref() != Some(&selection) {
            operation_error.clear();
            if let Some(profile) = desired {
                match apply_profile(&ppd, &state, profile).await {
                    Ok(()) => {
                        if let Some(id) = profile.id() {
                            state.active_profile = ppd
                                .get_property("ActiveProfile")
                                .await
                                .unwrap_or_else(|_| id.into());
                        }
                        last_plan = Some(selection);
                    }
                    Err(error) => {
                        state.error = error.to_string();
                    }
                }
            } else {
                last_plan = Some(selection);
            }
        }
        if !operation_error.is_empty() {
            state.error = operation_error.clone();
        }
        publish(tx, server, state.clone());
        tokio::select! {
            event = upower_events.next() => { if event.is_none() { anyhow::bail!("UPower signal stream closed"); } },
            event = ppd_events.next() => { if event.is_none() { anyhow::bail!("Power Profiles signal stream closed"); } },
            event = login_events.next() => { if event.is_none() { anyhow::bail!("logind signal stream closed"); } },
            event = owners.next() => {
                if let Some(event) = event {
                    let args = event.args()?;
                    if matches!(args.name().as_str(), UPOWER | PPD | LOGIN) { last_plan = None; }
                } else { anyhow::bail!("System bus closed"); }
            },
            _ = changes.recv() => {},
            _ = retry.tick() => {},
            request = sleep.recv() => {
                let Some(request) = request else { return Ok(()) };
                let current = snapshot(&upower, &ppd, &login).await;
                if !request.valid.load(std::sync::atomic::Ordering::Acquire) { continue; }
                if let Err(error) = sleep_on(&login, request.action, &current).await {
                    operation_error = error.to_string();
                    let mut current = current; current.error = operation_error.clone(); publish(tx, server, current);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    };
    struct Profiles {
        active: Mutex<String>,
        writes: Arc<Mutex<Vec<String>>>,
    }
    #[zbus::interface(name = "org.freedesktop.UPower.PowerProfiles")]
    impl Profiles {
        #[zbus(property)]
        fn active_profile(&self) -> String {
            self.active.lock().unwrap().clone()
        }
        #[zbus(property)]
        fn set_active_profile(&self, value: String) {
            self.writes.lock().unwrap().push(value.clone());
            *self.active.lock().unwrap() = value;
        }
        #[zbus(property)]
        fn profiles(&self) -> Vec<HashMap<String, OwnedValue>> {
            ["power-saver", "balanced"]
                .into_iter()
                .map(|id| {
                    HashMap::from([(
                        "Profile".into(),
                        OwnedValue::from(zbus::zvariant::Str::from(id)),
                    )])
                })
                .collect()
        }
        #[zbus(property)]
        fn performance_degraded(&self) -> &str {
            ""
        }
    }
    struct Supply(Arc<AtomicBool>);
    #[zbus::interface(name = "org.freedesktop.UPower")]
    impl Supply {
        #[zbus(property)]
        fn on_battery(&self) -> bool {
            self.0.load(Ordering::Acquire)
        }
    }
    struct Login {
        calls: Arc<Mutex<Vec<bool>>>,
        blocked: Arc<AtomicBool>,
    }
    #[zbus::interface(name = "org.freedesktop.login1.Manager")]
    impl Login {
        fn can_suspend(&self) -> &str {
            "yes"
        }
        fn can_hibernate(&self) -> &str {
            "no"
        }
        fn suspend(&self, interactive: bool) {
            self.calls.lock().unwrap().push(interactive);
        }
        #[zbus(property)]
        fn block_inhibited(&self) -> &str {
            if self.blocked.load(Ordering::Acquire) {
                "sleep"
            } else {
                ""
            }
        }
    }
    #[tokio::test]
    #[ignore = "requires isolated dbus-run-session; never writes to system bus"]
    async fn private_bus_power_profiles_supply_and_sleep_capabilities_are_real() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let battery = Arc::new(AtomicBool::new(false));
        let blocked = Arc::new(AtomicBool::new(false));
        let connection = zbus::connection::Builder::session()
            .unwrap()
            .name(PPD)
            .unwrap()
            .name(UPOWER)
            .unwrap()
            .name(LOGIN)
            .unwrap()
            .serve_at(
                PPD_PATH,
                Profiles {
                    active: Mutex::new("balanced".into()),
                    writes: writes.clone(),
                },
            )
            .unwrap()
            .serve_at(UPOWER_PATH, Supply(battery.clone()))
            .unwrap()
            .serve_at(
                LOGIN_PATH,
                Login {
                    calls: calls.clone(),
                    blocked: blocked.clone(),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let client = Connection::session().await.unwrap();
        let upower = proxy(&client, UPOWER, UPOWER_PATH, UPOWER).await.unwrap();
        let ppd = proxy(&client, PPD, PPD_PATH, PPD).await.unwrap();
        let login = proxy(&client, LOGIN, LOGIN_PATH, "org.freedesktop.login1.Manager")
            .await
            .unwrap();
        let state = snapshot(&upower, &ppd, &login).await;
        assert_eq!(state.on_battery, Some(false));
        assert!(state.can_suspend && !state.can_hibernate);
        assert_eq!(state.available_profiles, ["power-saver", "balanced"]);
        apply_profile(&ppd, &state, HardwareProfile::Unchanged)
            .await
            .unwrap();
        assert!(writes.lock().unwrap().is_empty());
        assert!(apply_profile(&ppd, &state, HardwareProfile::Performance)
            .await
            .is_err());
        apply_profile(&ppd, &state, HardwareProfile::PowerSaver)
            .await
            .unwrap();
        assert_eq!(*writes.lock().unwrap(), ["power-saver"]);
        battery.store(true, Ordering::Release);
        let emitter = zbus::object_server::SignalEmitter::new(&connection, UPOWER_PATH).unwrap();
        let props = proxy(
            &client,
            UPOWER,
            UPOWER_PATH,
            "org.freedesktop.DBus.Properties",
        )
        .await
        .unwrap();
        let mut events = props.receive_signal("PropertiesChanged").await.unwrap();
        connection
            .object_server()
            .interface::<_, Supply>(UPOWER_PATH)
            .await
            .unwrap()
            .get()
            .await
            .on_battery_changed(&emitter)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), events.next())
            .await
            .unwrap()
            .unwrap();
        let state = snapshot(&upower, &ppd, &login).await;
        assert_eq!(state.on_battery, Some(true));
        let plans = rix_config::power::PowerSettings {
            automatic_battery_plan: true,
            ..Default::default()
        };
        assert_eq!(
            plans.active(state.on_battery).profile,
            HardwareProfile::PowerSaver
        );
        assert!(sleep_on(&login, SleepAction::Hibernate, &state)
            .await
            .is_err());
        sleep_on(&login, SleepAction::Suspend, &state)
            .await
            .unwrap();
        assert_eq!(*calls.lock().unwrap(), [false]);
        blocked.store(true, Ordering::Release);
        let state = snapshot(&upower, &ppd, &login).await;
        assert!(sleep_on(&login, SleepAction::Suspend, &state)
            .await
            .is_err());
        assert_eq!(calls.lock().unwrap().len(), 1);

        blocked.store(false, Ordering::Release);
        battery.store(false, Ordering::Release);
        let directory = tempfile::tempdir().unwrap();
        let config = Arc::new(ConfigManager::init_in(directory.path().join("config")).unwrap());
        let mut desktop = config.get_desktop();
        desktop.power = Some(plans);
        config.save_desktop(desktop).unwrap();
        let server = Arc::new(rix_ipc::IpcServer::new_with_path(
            config.clone(),
            directory.path().join("ipc.sock"),
        ));
        let (tx, mut rx) = watch::channel(PowerState::default());
        let (sleep_tx, mut sleep_rx) = mpsc::channel(4);
        let worker_conn = client.clone();
        let worker = tokio::spawn(async move {
            watch_on(&worker_conn, config, &tx, &server, &mut sleep_rx).await
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                rx.changed().await.unwrap();
                if rx.borrow().on_battery == Some(false) && rx.borrow().active_profile == "balanced"
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        battery.store(true, Ordering::Release);
        connection
            .object_server()
            .interface::<_, Supply>(UPOWER_PATH)
            .await
            .unwrap()
            .get()
            .await
            .on_battery_changed(&emitter)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                rx.changed().await.unwrap();
                if rx.borrow().on_battery == Some(true)
                    && rx.borrow().active_profile == "power-saver"
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        sleep_tx
            .send(SleepRequest {
                action: SleepAction::Suspend,
                valid: Arc::new(AtomicBool::new(false)),
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(calls.lock().unwrap().len(), 1);
        worker.abort();
        let _ = worker.await;
    }
}
