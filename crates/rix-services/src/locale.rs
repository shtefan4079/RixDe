use std::process::Command;

pub fn application_environment(preference: &str) -> Vec<(&'static str, &'static str)> {
    let (locale, language) = match preference {
        "en" => ("en_US.UTF-8", "en:en_US"),
        _ => return Vec::new(),
    };

    vec![
        ("LANG", locale),
        ("LC_MESSAGES", locale),
        ("LANGUAGE", language),

        ("LC_ALL", locale),
    ]
}

pub fn activation_environment(preference: &str) -> Vec<(String, String)> {
    if preference == "system" || preference != "en" {
        let lang = std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".into());
        let messages = std::env::var("LC_MESSAGES")
            .or_else(|_| std::env::var("LC_ALL"))
            .unwrap_or_else(|_| lang.clone());
        let language = std::env::var("LANGUAGE").unwrap_or_else(|_| language_candidates(&messages));
        let mut values = vec![
            ("LANG".to_string(), lang),
            ("LC_MESSAGES".to_string(), messages),
            ("LANGUAGE".to_string(), language),
        ];
        if let Ok(locale) = std::env::var("LC_ALL") {
            values.push(("LC_ALL".to_string(), locale));
        }
        return values;
    }

    let mut values = application_environment(preference)
        .into_iter()
        .filter(|(name, _)| *name != "LC_ALL" || std::env::var_os("LC_ALL").is_some())
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect::<Vec<_>>();
    values.sort_by(|left, right| left.0.cmp(&right.0));
    values
}

pub fn apply_to_command(command: &mut Command, preference: &str) {
    for (name, value) in application_environment(preference) {
        command.env(name, value);
    }
}

fn language_candidates(locale: &str) -> String {
    let language = locale
        .split(['_', '-', '.', '@'])
        .next()
        .filter(|part| !part.is_empty() && *part != "C" && *part != "POSIX")
        .unwrap_or("en");
    let territory = locale
        .split_once('_')
        .and_then(|(_, rest)| rest.split(['.', '@']).next())
        .filter(|part| !part.is_empty());
    territory.map_or_else(
        || language.to_string(),
        |territory| format!("{language}:{language}_{territory}"),
    )
}

#[cfg(test)]
mod tests {
    use super::{activation_environment, application_environment, apply_to_command};
    use std::process::Command;

    #[test]
    fn explicit_application_language_sets_locale_and_catalog_language() {
        assert_eq!(
            application_environment("en"),
            vec![
                ("LANG", "en_US.UTF-8"),
                ("LC_MESSAGES", "en_US.UTF-8"),
                ("LANGUAGE", "en:en_US"),
                ("LC_ALL", "en_US.UTF-8"),
            ]
        );
        assert!(application_environment("system").is_empty());
        assert!(application_environment("fr").is_empty());
    }

    #[test]
    fn command_locale_is_added_without_mutating_the_parent_environment() {
        let mut command = Command::new("true");
        apply_to_command(&mut command, "en");
        let values = command
            .get_envs()
            .filter_map(|(name, value)| Some((name.to_str()?, value?.to_str()?.to_string())))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(values.get("LANG").map(String::as_str), Some("en_US.UTF-8"));
        assert_eq!(values.get("LANGUAGE").map(String::as_str), Some("en:en_US"));
    }

    #[test]
    fn system_activation_restores_login_locale_after_an_explicit_choice() {
        let values = activation_environment("system");
        let lang = std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".into());
        let messages = std::env::var("LC_MESSAGES")
            .or_else(|_| std::env::var("LC_ALL"))
            .unwrap_or_else(|_| lang.clone());
        assert!(values.contains(&("LANG".into(), lang)));
        assert!(values.contains(&("LC_MESSAGES".into(), messages)));
        assert!(values.iter().any(|(name, _)| name == "LANGUAGE"));
    }
}
