//! Octet's own Antigravity settings file. Antigravity reads a `settings.json`
//! from a folder Octet names at launch. The user's own file may say
//! `toolPermission: "always-proceed"`, which would skip every approval
//! request, so Octet launches `agy` with a file of its own, built from the
//! user's with the request-skipping parts removed. The user's file, and
//! everything else under Antigravity's home, is never written.
use crate::live::Mode;
use serde_json::{Map, Value};
use std::{
    fmt::Display,
    io::{ErrorKind, Read as _, Write as _},
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _},
    path::{Path, PathBuf},
};

/// What Octet says when the user's Antigravity settings could not be used.
const USER_SETTINGS_NOTICE: &str = concat!(
    "Octet could not read your Antigravity settings file, ",
    "so this session runs without your own Antigravity settings"
);
/// The most bytes of the user's settings file Octet reads.
const USER_SETTINGS_LIMIT: u64 = 1024 * 1024;
/// The settings file's name, in the user's folder and in Octet's.
const FILE: &str = "settings.json";

/// The folder Octet launches Antigravity with: one for `FullAccess` (the
/// `always-proceed` pin) and one for the other modes, so sessions in
/// different modes never share a file.
pub(super) fn cli_dir(data_dir: &Path, mode: Mode) -> PathBuf {
    let pin = if mode == Mode::FullAccess {
        "proceed"
    } else {
        "review"
    };
    data_dir.join("antigravity").join(pin)
}

/// The JSON text of Octet's settings file for `mode`, built from the user's
/// settings `user`, and a notice when `user` could not be used. The user's
/// request-skipping settings are dropped (`agentMode`, `permissions.allow`)
/// and `toolPermission` is pinned; every other key is kept.
pub(super) fn merged(user: Option<&str>, mode: Mode) -> (String, Option<&'static str>) {
    let (mut settings, notice) = match user.map(serde_json::from_str::<Value>) {
        None => (Map::new(), None),
        Some(Ok(Value::Object(map))) => (map, None),
        Some(_) => (Map::new(), Some(USER_SETTINGS_NOTICE)),
    };
    settings.remove("agentMode");
    match settings.get_mut("permissions") {
        Some(Value::Object(permissions)) => {
            permissions.remove("allow");
        }
        Some(_) => {
            settings.remove("permissions");
        }
        None => {}
    }
    let pin = if mode == Mode::FullAccess {
        "always-proceed"
    } else {
        "request-review"
    };
    settings.insert("toolPermission".into(), pin.into());
    (Value::Object(settings).to_string(), notice)
}

/// Antigravity's own folder, `~/.gemini`.
pub(super) fn vendor_home() -> Option<PathBuf> {
    crate::live::home_dir().map(|home| home.join(".gemini"))
}

/// What reading the user's settings file found.
enum UserFile {
    /// There is no file: no settings, and nothing to say.
    Missing,
    /// The file's text.
    Text(String),
    /// A file that cannot be read, is over the limit, or is not UTF-8.
    Unusable,
}

/// The user's settings file under `vendor_home`, opened read-only and read to
/// at most `USER_SETTINGS_LIMIT` bytes.
fn read_user(vendor_home: &Path) -> UserFile {
    let path = vendor_home.join("antigravity-cli").join(FILE);
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return UserFile::Missing,
        Ok(_) | Err(_) => return UserFile::Unusable,
    }
    let mut bytes = Vec::new();
    let read = std::fs::File::open(&path)
        .and_then(|file| file.take(USER_SETTINGS_LIMIT + 1).read_to_end(&mut bytes));
    match read {
        Ok(_) if bytes.len() as u64 > USER_SETTINGS_LIMIT => UserFile::Unusable,
        Ok(_) => String::from_utf8(bytes).map_or(UserFile::Unusable, UserFile::Text),
        Err(error) if error.kind() == ErrorKind::NotFound => UserFile::Missing,
        Err(_) => UserFile::Unusable,
    }
}

/// `path` with its links resolved. A path that does not exist yet is
/// resolved through its deepest folder that does, the rest appended
/// unchanged.
fn resolve(path: &Path) -> std::io::Result<PathBuf> {
    let mut rest = Vec::new();
    let mut at = path;
    loop {
        match at.canonicalize() {
            Ok(mut resolved) => {
                resolved.extend(rest.iter().rev());
                return Ok(resolved);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let (Some(name), Some(parent)) = (at.file_name(), at.parent()) else {
            return Err(ErrorKind::NotFound.into());
        };
        rest.push(name);
        at = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
    }
}

/// The text Octet fails a launch with when `path` cannot be written.
fn write_error(path: &Path, error: impl Display) -> String {
    format!(
        "Octet could not write its Antigravity settings ({}): {error}",
        path.display()
    )
}

/// Refuses `folder` when it is, or is under, the resolved vendor home
/// `home` (links resolved). Nothing is created.
fn keep_out_of(folder: &Path, home: Option<&Path>) -> Result<(), String> {
    let resolved = resolve(folder).map_err(|error| write_error(folder, error))?;
    match home {
        Some(home) if resolved.starts_with(home) => Err(format!(
            "Octet will not keep its Antigravity settings inside Antigravity's own folder ({}); choose another --journal-dir",
            resolved.display()
        )),
        _ => Ok(()),
    }
}

/// Reads the user's settings under `vendor_home`, writes Octet's file for
/// `mode` to `<cli_dir>/settings.json`, and returns the notices to show.
///
/// # Errors
///
/// When the folder or file cannot be written, or the folder is inside
/// `vendor_home` (by a link or by `--journal-dir`).
pub(super) fn write(
    data_dir: &Path,
    mode: Mode,
    vendor_home: Option<&Path>,
) -> Result<Vec<String>, String> {
    // A vendor home that does not exist forbids nothing.
    let home = match vendor_home.map(Path::canonicalize) {
        None => None,
        Some(Ok(home)) => Some(home),
        Some(Err(error)) if error.kind() == ErrorKind::NotFound => None,
        Some(Err(error)) => return Err(write_error(data_dir, error)),
    };
    let dir = cli_dir(data_dir, mode);
    // Everything is checked before anything is created.
    keep_out_of(data_dir, home.as_deref())?;
    keep_out_of(&dir, home.as_deref())?;
    let mut notices = Vec::new();
    let user = match vendor_home.map_or(UserFile::Missing, read_user) {
        UserFile::Text(text) => Some(text),
        UserFile::Missing => None,
        UserFile::Unusable => {
            notices.push(USER_SETTINGS_NOTICE.to_owned());
            None
        }
    };
    let (text, notice) = merged(user.as_deref(), mode);
    notices.extend(notice.map(str::to_owned));
    // Only folders Octet creates are owner-only.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|error| write_error(&dir, error))?;
    // A link made since the first check is caught here.
    keep_out_of(&dir, home.as_deref())?;
    let file = dir.join(FILE);
    let temp = dir.join(format!("{FILE}.{}.tmp", std::process::id()));
    let written = write_new(&temp, text.as_bytes())
        .or_else(|error| {
            if error.kind() != ErrorKind::AlreadyExists {
                return Err(error);
            }
            // Left by a run that crashed.
            std::fs::remove_file(&temp)?;
            write_new(&temp, text.as_bytes())
        })
        .and_then(|()| std::fs::rename(&temp, &file));
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(write_error(&file, error));
    }
    Ok(notices)
}

/// Writes `bytes` to a file `path` that does not exist yet, owner-only.
fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use octet_testkit::TempDir;
    use serde_json::{Value, json};
    use std::os::unix::fs::symlink;

    fn parsed(text: &str) -> Value {
        serde_json::from_str(text).expect("merged settings are JSON")
    }

    #[test]
    fn the_asking_modes_pin_request_review_and_full_access_proceeds() {
        for mode in [Mode::Ask, Mode::AcceptEdits, Mode::Auto] {
            let (text, notice) = merged(None, mode);
            assert_eq!(parsed(&text)["toolPermission"], "request-review");
            assert_eq!(notice, None);
        }
        let (text, notice) = merged(None, Mode::FullAccess);
        assert_eq!(parsed(&text)["toolPermission"], "always-proceed");
        assert_eq!(notice, None);
    }

    #[test]
    fn the_users_way_of_skipping_requests_is_dropped_and_the_rest_kept() {
        let user = r#"{"toolPermission":"always-proceed","agentMode":"accept-edits",
            "permissions":{"allow":["command(*)"],"deny":["command(rm)"],"ask":["mcp(*)"]},
            "modelProvider":"x"}"#;
        let (text, notice) = merged(Some(user), Mode::Ask);
        let settings = parsed(&text);
        assert_eq!(settings["toolPermission"], "request-review");
        assert!(settings.get("agentMode").is_none());
        assert!(settings["permissions"].get("allow").is_none());
        assert_eq!(settings["permissions"]["deny"], json!(["command(rm)"]));
        assert_eq!(settings["permissions"]["ask"], json!(["mcp(*)"]));
        assert_eq!(settings["modelProvider"], "x");
        assert_eq!(notice, None);
    }

    #[test]
    fn settings_that_are_not_an_object_are_replaced_with_a_notice() {
        for user in ["[1]", "{broken"] {
            let (text, notice) = merged(Some(user), Mode::Ask);
            assert_eq!(parsed(&text), json!({"toolPermission": "request-review"}));
            assert_eq!(notice, Some(USER_SETTINGS_NOTICE));
        }
    }

    #[test]
    fn permissions_that_are_not_an_object_are_dropped() {
        let (text, notice) = merged(Some(r#"{"permissions":"all"}"#), Mode::Ask);
        assert!(parsed(&text).get("permissions").is_none());
        assert_eq!(notice, None);
    }

    #[test]
    fn the_two_pins_live_in_different_folders() {
        let data = Path::new("/data");
        assert_eq!(cli_dir(data, Mode::Ask), cli_dir(data, Mode::Auto));
        assert_eq!(cli_dir(data, Mode::Ask), cli_dir(data, Mode::AcceptEdits));
        assert_ne!(cli_dir(data, Mode::Ask), cli_dir(data, Mode::FullAccess));
        for mode in Mode::ALL {
            assert!(cli_dir(data, mode).starts_with("/data/antigravity"));
        }
    }

    /// A vendor home holding `.gemini/antigravity-cli/settings.json` with `user`.
    fn vendor_with(root: &Path, user: &str) -> PathBuf {
        let home = root.join("home/.gemini");
        std::fs::create_dir_all(home.join("antigravity-cli")).unwrap();
        std::fs::write(home.join("antigravity-cli/settings.json"), user).unwrap();
        home
    }

    #[test]
    fn the_file_is_written_fresh_and_the_users_is_untouched() {
        let root = TempDir::new("octet-agy-settings-fresh");
        let user = r#"{"toolPermission":"always-proceed"}"#;
        let home = vendor_with(root.path(), user);
        let data = root.path().join("data");
        let dir = cli_dir(&data, Mode::Ask);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings.json"), r#"{"stale":true}"#).unwrap();
        let notices = write(&data, Mode::Ask, Some(&home)).unwrap();
        assert!(notices.is_empty());
        let ours = parsed(&std::fs::read_to_string(dir.join("settings.json")).unwrap());
        assert_eq!(ours["toolPermission"], "request-review");
        assert!(ours.get("stale").is_none());
        assert_eq!(
            std::fs::read_to_string(home.join("antigravity-cli/settings.json")).unwrap(),
            user
        );
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temporary file is left behind");
    }

    #[test]
    fn an_oversized_or_unreadable_users_file_gives_the_notice_and_the_pin() {
        let root = TempDir::new("octet-agy-settings-unusable");
        let big = "a".repeat(usize::try_from(USER_SETTINGS_LIMIT).unwrap() + 1);
        let home = vendor_with(root.path(), &big);
        let data = root.path().join("data-big");
        let notices = write(&data, Mode::FullAccess, Some(&home)).unwrap();
        assert_eq!(notices, [USER_SETTINGS_NOTICE]);
        let ours = parsed(
            &std::fs::read_to_string(cli_dir(&data, Mode::FullAccess).join("settings.json"))
                .unwrap(),
        );
        assert_eq!(ours, json!({"toolPermission": "always-proceed"}));
        // A directory where the file should be.
        let home = root.path().join("home2/.gemini");
        std::fs::create_dir_all(home.join("antigravity-cli/settings.json")).unwrap();
        let data = root.path().join("data-dir");
        let notices = write(&data, Mode::Ask, Some(&home)).unwrap();
        assert_eq!(notices, [USER_SETTINGS_NOTICE]);
        let ours = parsed(
            &std::fs::read_to_string(cli_dir(&data, Mode::Ask).join("settings.json")).unwrap(),
        );
        assert_eq!(ours, json!({"toolPermission": "request-review"}));
        // A file that is not UTF-8.
        let home = vendor_with(root.path(), "");
        std::fs::write(home.join("antigravity-cli/settings.json"), [0xff, 0xfe]).unwrap();
        let data = root.path().join("data-bytes");
        assert_eq!(
            write(&data, Mode::Ask, Some(&home)).unwrap(),
            [USER_SETTINGS_NOTICE]
        );
        // A missing file is no settings and no notice.
        let data = root.path().join("data-none");
        assert!(write(&data, Mode::Ask, None).unwrap().is_empty());
    }

    #[test]
    fn a_folder_that_cannot_be_written_stops_the_launch() {
        let root = TempDir::new("octet-agy-settings-blocked");
        std::fs::create_dir_all(root.path()).unwrap();
        let data = root.path().join("data");
        std::fs::write(&data, "not a folder").unwrap();
        let error = write(&data, Mode::Ask, None).unwrap_err();
        assert!(
            error.starts_with("Octet could not write its Antigravity settings"),
            "{error}"
        );
    }

    #[test]
    fn a_settings_folder_that_is_a_link_into_the_vendors_home_is_refused() {
        let root = TempDir::new("octet-agy-settings-link");
        let user = r#"{"toolPermission":"always-proceed"}"#;
        let home = vendor_with(root.path(), user);
        let data = root.path().join("data");
        std::fs::create_dir_all(data.join("antigravity")).unwrap();
        symlink(home.join("antigravity-cli"), cli_dir(&data, Mode::Ask)).unwrap();
        let error = write(&data, Mode::Ask, Some(&home)).unwrap_err();
        assert!(
            error.starts_with("Octet will not keep its Antigravity settings inside"),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(home.join("antigravity-cli/settings.json")).unwrap(),
            user
        );
        assert_eq!(
            std::fs::read_dir(home.join("antigravity-cli"))
                .unwrap()
                .count(),
            1,
            "nothing was written beside the user's file"
        );
    }

    #[test]
    fn a_data_directory_inside_the_vendors_home_is_refused_and_nothing_is_created() {
        let root = TempDir::new("octet-agy-settings-inside");
        let home = vendor_with(root.path(), "{}");
        let data = home.join("octet");
        let error = write(&data, Mode::Ask, Some(&home)).unwrap_err();
        assert!(
            error.starts_with("Octet will not keep its Antigravity settings inside"),
            "{error}"
        );
        assert!(!data.join("antigravity").exists());
        assert!(!data.exists());
    }
}
