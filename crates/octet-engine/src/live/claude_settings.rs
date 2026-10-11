//! Which Claude plugins are enabled for a folder, read as Claude reads them:
//! the `enabledPlugins` maps of the user's, the project's and the project's
//! local settings, later files overriding earlier ones. Nothing else in them
//! is read: Octet keeps the rest of Claude's settings off.
use serde_json::Value;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};

/// Claude's user configuration folder, as Claude finds it:
/// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub(crate) fn user_dir() -> Option<PathBuf> {
    config_dir(
        std::env::var_os("CLAUDE_CONFIG_DIR"),
        std::env::var_os("HOME"),
    )
}

/// `user_dir` from the two variables' values; an empty one is unset.
fn config_dir(claude: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let set = |value: Option<OsString>| value.filter(|value| !value.is_empty());
    set(claude)
        .map(PathBuf::from)
        .or_else(|| set(home).map(|home| PathBuf::from(home).join(".claude")))
}

/// The enabled plugin names (sorted), and a warning for each file that exists
/// but could not be read as settings. `user` is Claude's user folder.
pub(crate) fn enabled_plugins(user: Option<&Path>, cwd: &Path) -> (Vec<String>, Vec<String>) {
    let files = user
        .map(|user| user.join("settings.json"))
        .into_iter()
        .chain([
            cwd.join(".claude/settings.json"),
            cwd.join(".claude/settings.local.json"),
        ]);
    let mut enabled = BTreeMap::new();
    let mut warnings = Vec::new();
    for file in files {
        // A missing file is no plugins; one that cannot be read gets a notice.
        let bytes = match std::fs::read(&file) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => None,
        };
        let map = bytes
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .filter(Value::is_object)
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
        let (names, warnings) = enabled_plugins(Some(&home.path().join(".claude")), work.path());
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
        let (names, warnings) = enabled_plugins(Some(&home.path().join(".claude")), work.path());
        assert!(names.is_empty());
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("settings.json"));
        assert!(enabled_plugins(None, work.path()).0.is_empty());
    }
    #[test]
    fn an_unreadable_or_non_object_file_gets_a_warning() {
        let home = dir("octet-cs-unreadable");
        let work = dir("octet-cs-unreadable-work");
        // A directory where the file should be: it exists, but cannot be read.
        std::fs::create_dir_all(home.path().join(".claude/settings.json")).unwrap();
        std::fs::write(work.path().join(".claude/settings.json"), "[1, 2]").unwrap();
        let (_, warnings) = enabled_plugins(Some(&home.path().join(".claude")), work.path());
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }
    #[test]
    fn claude_config_dir_wins_over_home() {
        assert_eq!(
            config_dir(Some("/cfg".into()), Some("/home/me".into())),
            Some(std::path::PathBuf::from("/cfg"))
        );
        assert_eq!(
            config_dir(None, Some("/home/me".into())),
            Some(std::path::PathBuf::from("/home/me/.claude"))
        );
        assert_eq!(config_dir(Some("".into()), None), None);
    }
}
