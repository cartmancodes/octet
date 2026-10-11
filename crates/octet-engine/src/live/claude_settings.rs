//! Which Claude plugins are enabled for a folder, read as Claude reads them:
//! the `enabledPlugins` maps of the user's, the project's and the project's
//! local settings, later files overriding earlier ones. Nothing else in them
//! is read: Octet keeps the rest of Claude's settings off.
use serde_json::Value;
use std::{collections::BTreeMap, path::Path};

/// The enabled plugin names (sorted), and a warning for each file that exists
/// but could not be read as settings.
pub(crate) fn enabled_plugins(home: Option<&Path>, cwd: &Path) -> (Vec<String>, Vec<String>) {
    let files = home
        .map(|home| home.join(".claude/settings.json"))
        .into_iter()
        .chain([
            cwd.join(".claude/settings.json"),
            cwd.join(".claude/settings.local.json"),
        ]);
    let mut enabled = BTreeMap::new();
    let mut warnings = Vec::new();
    for file in files {
        let Ok(bytes) = std::fs::read(&file) else {
            continue;
        };
        let map = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|settings| match settings.get("enabledPlugins") {
                None => Some(serde_json::Map::new()),
                Some(Value::Object(map)) => Some(map.clone()),
                Some(_) => None,
            });
        match map {
            Some(map) => {
                for (name, on) in map {
                    if let Some(on) = on.as_bool() {
                        enabled.insert(name, on);
                    }
                }
            }
            None => warnings.push(format!(
                "Claude plugin settings in {} could not be read; its plugins are skipped",
                file.display()
            )),
        }
    }
    let names = enabled
        .into_iter()
        .filter(|(_, on)| *on)
        .map(|(name, _)| name)
        .collect();
    (names, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn dir(name: &str) -> octet_testkit::TempDir {
        let d = octet_testkit::TempDir::new(name);
        std::fs::create_dir_all(d.path().join(".claude")).unwrap();
        d
    }
    #[test]
    fn local_overrides_project_overrides_user() {
        let home = dir("octet-cs-home");
        let work = dir("octet-cs-work");
        std::fs::write(
            home.path().join(".claude/settings.json"),
            r#"{"enabledPlugins":{"a@m":true,"b@m":true,"c@m":false}}"#,
        )
        .unwrap();
        std::fs::write(
            work.path().join(".claude/settings.json"),
            r#"{"enabledPlugins":{"b@m":false,"c@m":true}}"#,
        )
        .unwrap();
        std::fs::write(
            work.path().join(".claude/settings.local.json"),
            r#"{"enabledPlugins":{"c@m":false,"d@m":true}}"#,
        )
        .unwrap();
        let (names, warnings) = enabled_plugins(Some(home.path()), work.path());
        assert_eq!(names, ["a@m", "d@m"]);
        assert!(warnings.is_empty());
    }
    #[test]
    fn missing_and_malformed_files_are_skipped_with_a_warning() {
        let home = dir("octet-cs-bad");
        let work = dir("octet-cs-bad-work");
        std::fs::write(home.path().join(".claude/settings.json"), "not json").unwrap();
        std::fs::write(
            work.path().join(".claude/settings.json"),
            r#"{"enabledPlugins":["not","an","object"]}"#,
        )
        .unwrap();
        let (names, warnings) = enabled_plugins(Some(home.path()), work.path());
        assert!(names.is_empty());
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("settings.json"));
        assert!(enabled_plugins(None, work.path()).0.is_empty());
    }
}
