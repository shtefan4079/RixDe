use rix_config::ConfigManager;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};
use zbus::{fdo, object_server::SignalEmitter, zvariant::OwnedValue};

const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.rix";
const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const APPEARANCE_NAMESPACE: &str = "org.freedesktop.appearance";
const COLOR_SCHEME_KEY: &str = "color-scheme";
const PREFER_DARK: u32 = 1;
const PREFER_LIGHT: u32 = 2;

type SettingsMap = HashMap<String, HashMap<String, OwnedValue>>;

struct SettingsBackend {
    color_scheme: Arc<AtomicU32>,
}

impl SettingsBackend {
    fn settings_read_all(&self, namespaces: Vec<String>) -> SettingsMap {
        if !namespace_requested(&namespaces, APPEARANCE_NAMESPACE) {
            return SettingsMap::new();
        }

        HashMap::from([(
            APPEARANCE_NAMESPACE.to_string(),
            HashMap::from([(
                COLOR_SCHEME_KEY.to_string(),
                OwnedValue::from(self.color_scheme.load(Ordering::Relaxed)),
            )]),
        )])
    }

    fn settings_read(&self, namespace: &str, key: &str) -> fdo::Result<OwnedValue> {
        if namespace != APPEARANCE_NAMESPACE || key != COLOR_SCHEME_KEY {
            return Err(fdo::Error::Failed(format!(
                "Unknown desktop setting: {namespace}.{key}"
            )));
        }
        Ok(OwnedValue::from(self.color_scheme.load(Ordering::Relaxed)))
    }
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Settings")]
impl SettingsBackend {
    fn read_all(&self, namespaces: Vec<String>) -> SettingsMap {
        self.settings_read_all(namespaces)
    }

    fn read(&self, namespace: &str, key: &str) -> fdo::Result<OwnedValue> {
        self.settings_read(namespace, key)
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }

    #[zbus(signal)]
    async fn setting_changed(
        emitter: &SignalEmitter<'_>,
        namespace: &str,
        key: &str,
        value: OwnedValue,
    ) -> zbus::Result<()>;
}

pub async fn serve(config: Arc<ConfigManager>) -> anyhow::Result<()> {
    let mut changes = config.subscribe_theme();
    let scheme_value = Arc::new(AtomicU32::new(color_scheme(&config.get_theme())));
    let backend = SettingsBackend {
        color_scheme: scheme_value.clone(),
    };
    let connection = zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, backend)?
        .build()
        .await?;
    tracing::info!("XDG Settings portal is publishing the active Rix color scheme");

    loop {
        let theme = match changes.recv().await {
            Ok(theme) => theme,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => config.get_theme(),
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
        };
        let next = color_scheme(&theme);
        if scheme_value.swap(next, Ordering::Relaxed) == next {
            continue;
        }

        let emitter = SignalEmitter::new(&connection, OBJECT_PATH)?;
        SettingsBackend::setting_changed(
            &emitter,
            APPEARANCE_NAMESPACE,
            COLOR_SCHEME_KEY,
            OwnedValue::from(next),
        )
        .await?;
    }
}

fn color_scheme(theme: &rix_config::ThemeSettings) -> u32 {
    if theme.appearance.prefers_dark() {
        PREFER_DARK
    } else {
        PREFER_LIGHT
    }
}

fn namespace_requested(namespaces: &[String], namespace: &str) -> bool {
    namespaces.is_empty()
        || namespaces.iter().any(|pattern| {
            pattern.is_empty()
                || pattern == namespace
                || pattern
                    .strip_suffix(".*")
                    .is_some_and(|prefix| namespace.starts_with(prefix))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_only_requested_standard_appearance_namespace() {
        let backend = SettingsBackend {
            color_scheme: Arc::new(AtomicU32::new(PREFER_DARK)),
        };
        assert_eq!(backend.settings_read_all(Vec::new()).len(), 1);
        assert_eq!(backend.settings_read_all(vec![String::new()]).len(), 1);
        assert_eq!(
            backend
                .settings_read_all(vec!["org.freedesktop.*".into()])
                .len(),
            1
        );
        assert!(backend
            .settings_read_all(vec!["org.example.appearance".into()])
            .is_empty());
        assert!(backend
            .settings_read("org.freedesktop.appearance", "unknown")
            .is_err());
    }

    #[test]
    fn portal_color_scheme_tracks_the_active_theme_palette() {
        let mut theme = rix_config::ThemeSettings::plasma_glass();
        assert_eq!(color_scheme(&theme), PREFER_DARK);
        theme
            .appearance
            .palette
            .insert("background".into(), "#f2f6fc".into());
        assert_eq!(color_scheme(&theme), PREFER_LIGHT);
    }

    #[tokio::test]
    #[ignore = "requires an isolated dbus-run-session"]
    async fn exported_settings_interface_returns_the_color_scheme_variant() {
        use futures_util::StreamExt;
        use std::time::Duration;
        use zbus::{connection::Builder, object_server::SignalEmitter, Connection, Proxy};

        let service = Builder::session()
            .unwrap()
            .name(BUS_NAME)
            .unwrap()
            .serve_at(
                OBJECT_PATH,
                SettingsBackend {
                    color_scheme: Arc::new(AtomicU32::new(PREFER_DARK)),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let client = Connection::session().await.unwrap();
        let proxy = Proxy::new(
            &client,
            BUS_NAME,
            OBJECT_PATH,
            "org.freedesktop.impl.portal.Settings",
        )
        .await
        .unwrap();

        let value: OwnedValue = proxy
            .call("Read", &(APPEARANCE_NAMESPACE, COLOR_SCHEME_KEY))
            .await
            .unwrap();
        assert_eq!(u32::try_from(value).unwrap(), PREFER_DARK);
        let all: SettingsMap = proxy
            .call("ReadAll", &(Vec::<String>::new(),))
            .await
            .unwrap();
        assert_eq!(
            u32::try_from(all[APPEARANCE_NAMESPACE][COLOR_SCHEME_KEY].clone()).unwrap(),
            PREFER_DARK
        );
        let version: u32 = proxy.get_property("version").await.unwrap();
        assert_eq!(version, 2);

        let mut changes = proxy.receive_signal("SettingChanged").await.unwrap();
        SettingsBackend::setting_changed(
            &SignalEmitter::new(&service, OBJECT_PATH).unwrap(),
            APPEARANCE_NAMESPACE,
            COLOR_SCHEME_KEY,
            OwnedValue::from(PREFER_LIGHT),
        )
        .await
        .unwrap();
        let changed = tokio::time::timeout(Duration::from_secs(1), changes.next())
            .await
            .unwrap()
            .unwrap();
        let (namespace, key, value): (String, String, OwnedValue) =
            changed.body().deserialize().unwrap();
        assert_eq!(namespace, APPEARANCE_NAMESPACE);
        assert_eq!(key, COLOR_SCHEME_KEY);
        assert_eq!(u32::try_from(value).unwrap(), PREFER_LIGHT);

        drop(proxy);
        drop(client);
        drop(service);
    }
}
