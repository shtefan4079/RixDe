use super::{profiles, Settings};
use clavis_client::{Client, Interaction, Secret};
use std::{
    collections::{BTreeMap, HashMap},
    time::Duration,
};
use zbus::zvariant::Value;

const APPLICATION: &str = "org.rixde.NetworkAgent";
const SECRET_KEYS: &[&str] = &[
    "psk",
    "wep-key0",
    "wep-key1",
    "wep-key2",
    "wep-key3",
    "leap-password",
    "password",
    "password-raw",
    "private-key-password",
    "phase2-private-key-password",
    "phase2-password",
    "pin",
    "private-key",
];

fn enabled() -> bool {
    std::env::var("RIX_SECRET_STORE").is_ok_and(|value| value == "clavis")
}
fn uuid(settings: &Settings) -> Option<&str> {
    let value = profiles::str_setting(settings, "connection", "uuid")?;

    (value.len() == 36
        && value.chars().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        }))
    .then_some(value)
}
fn attributes(uuid: &str, group: &str, key: Option<&str>) -> BTreeMap<String, String> {
    let mut attributes = BTreeMap::from([
        ("application".into(), APPLICATION.into()),
        ("type".into(), "network-credential".into()),
        ("connection-uuid".into(), uuid.into()),
        ("setting".into(), group.into()),
    ]);
    if let Some(key) = key {
        attributes.insert("property".into(), key.into());
    }
    attributes
}
fn label(profile: &Settings, group: &str, key: &str) -> String {
    let name = profiles::str_setting(profile, "connection", "id").unwrap_or("Network");
    let mut label = String::new();
    for character in format!("{name} · {group} / {key}").chars() {
        if label.len() + character.len_utf8() > 256 {
            break;
        }
        label.push(character);
    }
    label
}

fn flag_allowed(flags: u32) -> bool {

    flags & 1 != 0 && flags & (2 | 4) == 0
}
fn keys(settings: &Settings, group: &str) -> Vec<String> {
    if group == "vpn" {
        let data: HashMap<String, String> =
            profiles::value_vec(settings, group, "data").unwrap_or_default();
        let mut keys = data
            .iter()
            .filter_map(|(key, value)| {
                let property = key.strip_suffix("-flags")?;
                (property.len() <= 256
                    && !property.is_empty()
                    && value.parse::<u32>().is_ok_and(flag_allowed))
                .then(|| property.to_owned())
            })
            .collect::<Vec<_>>();
        keys.sort();
        keys.truncate(64);
        return keys;
    }
    let Some(values) = settings.get(group) else {
        return Vec::new();
    };
    SECRET_KEYS
        .iter()
        .filter(|key| {
            values
                .get(&format!("{key}-flags"))
                .and_then(|v| u32::try_from(v).ok())
                .is_some_and(flag_allowed)
        })
        .map(|key| (*key).to_owned())
        .collect()
}

pub(super) fn remembered(profile: &Settings, answer: &Settings) -> Settings {
    answer
        .iter()
        .filter_map(|(group, values)| {
            let allowed = keys(profile, group);
            let mut retained = HashMap::new();
            if group == "vpn" {
                let secrets: HashMap<String, String> =
                    profiles::value_vec(answer, group, "secrets").unwrap_or_default();
                let secrets = secrets
                    .into_iter()
                    .filter(|(key, _)| allowed.contains(key))
                    .collect::<HashMap<_, _>>();
                if !secrets.is_empty() {
                    if let Ok(value) = Value::from(secrets).try_to_owned() {
                        retained.insert("secrets".into(), value);
                    }
                }
            } else {
                for (key, value) in values {
                    if allowed.contains(key) {
                        if let Ok(value) = value.try_clone() {
                            retained.insert(key.clone(), value);
                        }
                    }
                }
            }
            (!retained.is_empty()).then(|| (group.clone(), retained))
        })
        .collect()
}

pub(super) fn cached(profile: &Settings, answer: &Settings) -> Settings {
    let mut result = remembered(profile, answer);
    for (group, values) in &mut result {
        let companion = match group.as_str() {
            "802-1x" => "identity",
            "pppoe" | "gsm" => "username",
            _ => continue,
        };
        if let Some(value) = answer.get(group).and_then(|values| values.get(companion)) {
            if let Ok(value) = value.try_clone() {
                values.insert(companion.into(), value);
            }
        }
    }
    result
}

pub(super) fn satisfies(answer: &Settings, group: &str, hints: &[String]) -> bool {
    let Some(values) = answer.get(group) else {
        return false;
    };
    let vpn: HashMap<String, String> = if group == "vpn" {
        profiles::value_vec(answer, group, "secrets").unwrap_or_default()
    } else {
        HashMap::new()
    };
    hints
        .iter()
        .filter(|hint| {
            !hint.starts_with("x-vpn-") && !matches!(hint.as_str(), "username" | "identity")
        })
        .all(|hint| {
            if group == "vpn" {
                vpn.contains_key(hint)
            } else {
                values.contains_key(hint)
            }
        })
}

pub(super) async fn load(profile: &Settings, group: &str) -> Option<Settings> {
    if !enabled() {
        return None;
    }
    let uuid = uuid(profile)?;
    let keys = keys(profile, group);
    if keys.is_empty() {
        return None;
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        let client = Client::connect(Interaction::Never).await.ok()?;
        let mut values = HashMap::new();
        let mut vpn = HashMap::new();
        for key in keys {
            let Some(secret) = client
                .get(&attributes(uuid, group, Some(&key)))
                .await
                .ok()?
            else {
                continue;
            };
            if secret.as_bytes().len() > 64 * 1024 {
                continue;
            }
            if group == "vpn" {
                if let Ok(value) = std::str::from_utf8(secret.as_bytes()) {
                    vpn.insert(key, value.to_owned());
                }
            } else if key == "password-raw" {
                if let Ok(value) = Value::from(secret.as_bytes().to_vec()).try_to_owned() {
                    values.insert(key, value);
                }
            } else if let Ok(value) = std::str::from_utf8(secret.as_bytes()) {
                values.insert(key, profiles::string(value));
            }
        }
        if !vpn.is_empty() {
            values.insert("secrets".into(), Value::from(vpn).try_to_owned().ok()?);
        }
        (!values.is_empty()).then(|| HashMap::from([(group.to_owned(), values)]))
    })
    .await
    .ok()
    .flatten()
}

pub(super) async fn save(profile: &Settings, answer: &Settings) {
    if !enabled() {
        return;
    }
    let Some(uuid) = uuid(profile) else {
        return;
    };

    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        let Ok(client) = Client::connect(Interaction::Never).await else {
            return;
        };

        for (group, values) in answer {
            let allowed = keys(profile, group);
            let supplied = if group == "vpn" {
                profiles::value_vec::<HashMap<String, String>>(answer, group, "secrets")
                    .unwrap_or_default()
                    .into_keys()
                    .collect::<Vec<_>>()
            } else {
                values
                    .keys()
                    .filter(|key| SECRET_KEYS.contains(&key.as_str()))
                    .cloned()
                    .collect()
            };
            for key in supplied.into_iter().filter(|key| !allowed.contains(key)) {
                let _ = client.delete(&attributes(uuid, group, Some(&key))).await;
            }
        }
        for (group, values) in remembered(profile, answer) {
            if group == "vpn" {
                let secrets: HashMap<String, String> = values
                    .get("secrets")
                    .and_then(|v| v.try_clone().ok())
                    .and_then(|v| HashMap::try_from(v).ok())
                    .unwrap_or_default();
                for (key, value) in secrets {
                    let _ = client
                        .set(
                            &label(profile, &group, &key),
                            &attributes(uuid, &group, Some(&key)),
                            Secret::text(value),
                        )
                        .await;
                }
            } else {
                for (key, value) in values {
                    let secret = if key == "password-raw" {
                        let Ok(bytes) = Vec::<u8>::try_from(value) else {
                            continue;
                        };
                        Secret::blob(bytes)
                    } else {
                        let Ok(text) = <&str>::try_from(&value) else {
                            continue;
                        };
                        Secret::text(text.to_owned())
                    };
                    let _ = client
                        .set(
                            &label(profile, &group, &key),
                            &attributes(uuid, &group, Some(&key)),
                            secret,
                        )
                        .await;
                }
            }
        }
    })
    .await;
}

pub(super) async fn delete(profile: &Settings) {
    if !enabled() {
        return;
    }
    let Some(uuid) = uuid(profile) else {
        return;
    };
    let mut query = attributes(uuid, "", None);
    query.remove("setting");
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        if let Ok(client) = Client::connect(Interaction::Never).await {
            let _ = client.delete(&query).await;
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::OwnedValue;
    fn profile() -> Settings {
        HashMap::from([
            (
                "connection".into(),
                HashMap::from([(
                    "uuid".into(),
                    profiles::string("01234567-89ab-cdef-0123-456789abcdef"),
                )]),
            ),
            (
                "802-1x".into(),
                HashMap::from([
                    ("password-flags".into(), OwnedValue::from(1u32)),
                    ("private-key-password-flags".into(), OwnedValue::from(3u32)),
                ]),
            ),
        ])
    }
    #[test]
    fn flags_are_applied_to_each_secret_and_not_saved_is_never_retained() {
        let profile = profile();
        let answer = HashMap::from([(
            "802-1x".into(),
            HashMap::from([
                ("password".into(), profiles::string("fixture-password")),
                (
                    "private-key-password".into(),
                    profiles::string("temporary-fixture"),
                ),
                ("identity".into(), profiles::string("fixture-user")),
            ]),
        )]);
        let remembered = remembered(&profile, &answer);
        assert_eq!(remembered["802-1x"].len(), 1);
        assert!(remembered["802-1x"].contains_key("password"));
        for flags in [0, 2, 3, 4, 5, 6, 7] {
            assert!(!flag_allowed(flags));
        }
        assert!(flag_allowed(1));
    }
    #[test]
    fn cache_retains_required_nonsecret_identity_only_with_remembered_credentials() {
        let mut profile = profile();
        let answer = HashMap::from([(
            "802-1x".into(),
            HashMap::from([
                ("password".into(), profiles::string("fixture")),
                ("identity".into(), profiles::string("fixture-user")),
                ("private-key-password".into(), profiles::string("temporary")),
            ]),
        )]);
        let cached = cached(&profile, &answer);
        assert_eq!(
            profiles::str_setting(&cached, "802-1x", "identity"),
            Some("fixture-user")
        );
        assert!(!cached["802-1x"].contains_key("private-key-password"));
        profile
            .get_mut("802-1x")
            .unwrap()
            .insert("password-flags".into(), OwnedValue::from(3u32));
        assert!(super::cached(&profile, &answer).is_empty());
    }
    #[test]
    fn partial_cache_cannot_skip_a_temporary_credential_request() {
        let answer = HashMap::from([(
            "802-1x".into(),
            HashMap::from([("password".into(), profiles::string("fixture"))]),
        )]);
        assert!(satisfies(&answer, "802-1x", &["password".into()]));
        assert!(!satisfies(
            &answer,
            "802-1x",
            &["private-key-password".into()]
        ));
    }
    #[test]
    fn uuid_keys_are_stable_and_missing_identity_cannot_persist() {
        assert_eq!(
            uuid(&profile()),
            Some("01234567-89ab-cdef-0123-456789abcdef")
        );
        assert!(uuid(&Settings::new()).is_none());
        let query = attributes(
            "01234567-89ab-cdef-0123-456789abcdef",
            "802-1x",
            Some("password"),
        );
        assert_eq!(query["application"], APPLICATION);
        assert!(!query.values().any(|v| v.contains("fixture-password")));
    }
    #[test]
    fn vpn_flags_filter_individual_secrets() {
        let profile = HashMap::from([(
            "vpn".into(),
            HashMap::from([(
                "data".into(),
                Value::from(HashMap::from([
                    ("password-flags".to_owned(), "1".to_owned()),
                    ("otp-flags".to_owned(), "3".to_owned()),
                    ("system-secret-flags".to_owned(), "0".to_owned()),
                ]))
                .try_to_owned()
                .unwrap(),
            )]),
        )]);
        let answer = HashMap::from([(
            "vpn".into(),
            HashMap::from([(
                "secrets".into(),
                Value::from(HashMap::from([
                    ("password".to_owned(), "fixture".to_owned()),
                    ("otp".to_owned(), "temporary".to_owned()),
                    ("system-secret".to_owned(), "system".to_owned()),
                ]))
                .try_to_owned()
                .unwrap(),
            )]),
        )]);
        let filtered = remembered(&profile, &answer);
        let secrets: HashMap<String, String> =
            profiles::value_vec(&filtered, "vpn", "secrets").unwrap();
        assert_eq!(
            secrets,
            HashMap::from([("password".into(), "fixture".into())])
        );
    }
    #[tokio::test]
    #[ignore = "requires an isolated Clavis fixture with an unlocked default collection; never use the host bus"]
    async fn private_bus_clavis_persistence_respects_flags_uuid_and_lock() {
        assert_eq!(std::env::var("CLAVIS_TEST_ONLY").as_deref(), Ok("1"));
        assert_eq!(std::env::var("RIX_SECRET_STORE").as_deref(), Ok("clavis"));
        let mut profile = profile();
        let answer = HashMap::from([(
            "802-1x".into(),
            HashMap::from([
                (
                    "password".into(),
                    profiles::string("persistent-fixture-password"),
                ),
                (
                    "private-key-password".into(),
                    profiles::string("temporary-fixture-password"),
                ),
            ]),
        )]);
        save(&profile, &answer).await;
        let loaded = load(&profile, "802-1x")
            .await
            .expect("unlocked fixture must load saved credential");
        assert_eq!(
            profiles::str_setting(&loaded, "802-1x", "password"),
            Some("persistent-fixture-password")
        );
        assert!(!loaded["802-1x"].contains_key("private-key-password"));
        let client = Client::connect(Interaction::Never).await.unwrap();
        assert!(client
            .get(&attributes(
                uuid(&profile).unwrap(),
                "802-1x",
                Some("private-key-password")
            ))
            .await
            .unwrap()
            .is_none());

        profile
            .get_mut("802-1x")
            .unwrap()
            .insert("password-flags".into(), OwnedValue::from(3u32));
        save(&profile, &answer).await;
        assert!(client
            .get(&attributes(
                uuid(&profile).unwrap(),
                "802-1x",
                Some("password")
            ))
            .await
            .unwrap()
            .is_none());
        profile
            .get_mut("802-1x")
            .unwrap()
            .insert("password-flags".into(), OwnedValue::from(1u32));
        save(&profile, &answer).await;

        profile
            .get_mut("connection")
            .unwrap()
            .insert("id".into(), profiles::string("renamed fixture"));
        assert!(load(&profile, "802-1x").await.is_some());
        delete(&profile).await;
        assert!(load(&profile, "802-1x").await.is_none());
        save(&profile, &answer).await;
        client.lock_all().await.unwrap();
        assert!(
            load(&profile, "802-1x").await.is_none(),
            "locked vault must fall back without an unlock prompt"
        );
    }
}
