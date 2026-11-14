//! How Antigravity is launched: its mode IDs, the arguments that name
//! Octet's settings folder (and a probe's scratch home), and what runs before
//! `agy` starts.
use super::{settings, trust};
use crate::live::{Config, Mode, Prepared};
use std::{
    ffi::OsString,
    io::ErrorKind,
    os::unix::fs::DirBuilderExt as _,
    path::{Path, PathBuf},
    time::Duration,
};

/// Why a session cannot start with plugins or their hooks switched off.
const NO_PLUGIN_SWITCH: &str = concat!(
    "Antigravity has no switch to turn plugins or their hooks off; ",
    "start without --no-plugins and --no-plugin-hooks"
);

/// Why a session cannot start without Octet's data directory.
const NO_DATA_DIR: &str = "Antigravity needs Octet's data directory to keep its settings in";

/// Why a probe cannot start while another of this process runs.
const PROBE_BUSY: &str = "Octet is already asking Antigravity for its models";

/// How old a probe's home must be before a later probe removes it: one left
/// by a process that died.
const PROBE_STALE: Duration = Duration::from_hours(1);

/// Antigravity's ID for `mode`. Full access is `default` under the
/// `always-proceed` settings.
pub(super) fn mode_id(mode: Mode) -> &'static str {
    match mode {
        Mode::Ask | Mode::FullAccess => "default",
        Mode::AcceptEdits => "accept-edits",
        Mode::Auto => "auto",
    }
}

/// The mode Antigravity's mode `id` carries in a session launched for
/// `pinned`: `default` is full access under that pin, else ask. `None` for
/// `plan` or a mode Octet does not know.
pub(super) fn mode_of(id: &str, pinned: Mode) -> Option<Mode> {
    match id {
        "default" if pinned == Mode::FullAccess => Some(Mode::FullAccess),
        "default" => Some(Mode::Ask),
        "accept-edits" => Some(Mode::AcceptEdits),
        "auto" => Some(Mode::Auto),
        _ => None,
    }
}

/// How many `../` start the name of Octet's settings folder (see
/// [`cli_dir_arg`]). `agy` joins `--cli_dir_name` under its app data folder
/// (`<gemini dir>/antigravity-cli`), so the name must climb out of it to the
/// root first. 32 is more than any such folder is deep, on purpose: a `..`
/// at the root stays at the root, so the name does not depend on where the
/// vendor home is, on `--gemini-dir`, or on whether `agy` resolves `..` by
/// name or through the file system.
const CLIMB: usize = 32;

/// The `--cli_dir_name` value that names `dir` to `agy`: [`CLIMB`] `../`,
/// then `dir`'s absolute path without its leading `/`. An absolute name
/// would not do: `agy` 1.5.5 joins it under its app data folder (dropping
/// the `/`) and, finding no settings there, runs on its defaults. A
/// relative `dir` is made absolute first (from the working directory), or
/// used as given if that fails.
fn cli_dir_arg(dir: &Path) -> OsString {
    let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_owned());
    let mut name = OsString::from("../".repeat(CLIMB));
    name.push(dir.strip_prefix("/").unwrap_or(&dir));
    name
}

/// The arguments that start `agy acp` with Octet's settings folder for the
/// mode (named by [`cli_dir_arg`]), and a probe in its scratch home. Without
/// a data directory it is `acp` alone, which never launches: `prepare`
/// fails first.
pub(super) fn launch_args(config: &Config) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["acp".into()];
    let Some(data_dir) = &config.data_dir else {
        return args;
    };
    args.push("--cli_dir_name".into());
    args.push(cli_dir_arg(&settings::cli_dir(data_dir, config.mode)));
    if config.catalog_only {
        args.push("--gemini-dir".into());
        args.push(settings::probe_home(data_dir).into_os_string());
    }
    args
}

/// The row's `prepare`: [`antigravity_prepare`] with Antigravity's own folder.
pub(super) fn prepare(config: &Config) -> Result<Prepared, String> {
    antigravity_prepare(config, settings::vendor_home().as_deref())
}

/// What runs before `agy` starts, in order: Octet's data directory must be
/// known; plugins and their hooks cannot be switched off (except for a
/// probe, which loads none); the workspace's `.agents` configuration must be
/// trusted; Octet's settings file for the mode is written from the user's in
/// `vendor_home`. A probe also removes the probe homes last changed over an
/// hour ago (a process that died left them), and makes its own. `Prepared`
/// carries the settings' notices, the probe's home and the fingerprint of the
/// configuration accepted.
///
/// # Errors
///
/// Why the session cannot start, in words to show.
pub fn antigravity_prepare(
    config: &Config,
    vendor_home: Option<&Path>,
) -> Result<Prepared, String> {
    let data_dir = config
        .data_dir
        .as_deref()
        .ok_or_else(|| NO_DATA_DIR.to_owned())?;
    let switched_off = !config.plugins.load || !config.plugins.hooks;
    if switched_off && !config.catalog_only {
        return Err(NO_PLUGIN_SWITCH.to_owned());
    }
    let workspace = trust::check(config, data_dir)?;
    let notices = settings::write(data_dir, config.mode, vendor_home)?;
    let scratch = if config.catalog_only {
        Some(probe_folder(data_dir)?)
    } else {
        None
    };
    Ok(Prepared {
        notices,
        scratch,
        workspace,
    })
}

/// Makes this process's probe home, owner-only, after removing the stale
/// ones. It is made exclusively: one already there belongs to a probe that
/// is running, which keeps it.
fn probe_folder(data_dir: &Path) -> Result<PathBuf, String> {
    remove_stale_probes(&data_dir.join("antigravity"));
    let home = settings::probe_home(data_dir);
    match std::fs::DirBuilder::new().mode(0o700).create(&home) {
        Ok(()) => Ok(home),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Err(PROBE_BUSY.to_owned()),
        Err(error) => Err(format!(
            "Octet could not make a home for Antigravity's model list ({}): {error}",
            home.display()
        )),
    }
}

/// Removes the probe homes in `folder` last modified over [`PROBE_STALE`]
/// ago: folders (never links) whose name starts with `probe-`. One whose
/// age cannot be read is left alone, and a failure to remove one ignored.
fn remove_stale_probes(folder: &Path) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name
            .as_encoded_bytes()
            .starts_with(settings::PROBE_PREFIX.as_bytes())
        {
            continue;
        }
        let path = entry.path();
        let stale = std::fs::symlink_metadata(&path).is_ok_and(|meta| {
            meta.is_dir()
                && meta
                    .modified()
                    .ok()
                    .and_then(|at| at.elapsed().ok())
                    .is_some_and(|age| age > PROBE_STALE)
        });
        if stale {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{Engine, antigravity::antigravity_workspace_config};
    use octet_testkit::TempDir;

    #[test]
    fn every_mode_has_a_mode_id_and_back() {
        for mode in Mode::ALL {
            assert_eq!(mode_of(mode_id(mode), mode), Some(mode), "{mode:?}");
        }
        assert_eq!(mode_of("default", Mode::Ask), Some(Mode::Ask));
        assert_eq!(mode_of("default", Mode::Auto), Some(Mode::Ask));
        assert_eq!(mode_of("plan", Mode::Ask), None);
    }

    fn data_config(mode: Mode) -> Config {
        let mut config = Config::new(Engine::ANTIGRAVITY, "agy", "/w");
        config.data_dir = Some("/d".into());
        config.mode = mode;
        config
    }

    /// `name` resolved as `agy` 1.5.5 resolves its `--cli_dir_name`: joined
    /// under `app_data` with any leading `/` removed, `.` and `..` folded by
    /// name, a `..` at the root dropped.
    fn resolved_by_agy(app_data: &Path, name: &OsString) -> PathBuf {
        let name = Path::new(name);
        let joined = app_data.join(name.strip_prefix("/").unwrap_or(name));
        let mut folded = PathBuf::from("/");
        for part in joined.components() {
            match part {
                std::path::Component::Normal(part) => folded.push(part),
                std::path::Component::ParentDir => {
                    folded.pop();
                }
                _ => {}
            }
        }
        folded
    }

    #[test]
    fn the_launch_names_octets_settings_folder() {
        let climb = "../".repeat(CLIMB);
        assert_eq!(
            launch_args(&data_config(Mode::Ask)),
            [
                OsString::from("acp"),
                "--cli_dir_name".into(),
                format!("{climb}d/antigravity/review").into()
            ]
        );
        assert_eq!(
            launch_args(&data_config(Mode::FullAccess))[2],
            OsString::from(format!("{climb}d/antigravity/proceed"))
        );
        let mut bare = data_config(Mode::Ask);
        bare.data_dir = None;
        assert_eq!(launch_args(&bare), ["acp"]);
    }

    #[test]
    fn agy_resolves_the_settings_folder_to_octets_wherever_its_home_is() {
        let folder = Path::new("/Users/me/.local/share/octet/antigravity/review");
        let name = cli_dir_arg(folder);
        let shallow = Path::new("/h/.gemini/antigravity-cli");
        let deep: PathBuf = ["/"]
            .into_iter()
            .chain(std::iter::repeat_n("level", 24))
            .chain([".gemini", "antigravity-cli"])
            .collect();
        for app_data in [shallow, deep.as_path()] {
            assert_eq!(
                resolved_by_agy(app_data, &name),
                folder,
                "{}",
                app_data.display()
            );
        }
        // The old, absolute name lands inside the vendor home.
        let absolute = OsString::from(folder);
        assert!(resolved_by_agy(shallow, &absolute).starts_with(shallow));
    }

    #[test]
    fn a_relative_folder_is_named_from_the_working_directory() {
        let name = cli_dir_arg(Path::new("data/antigravity/review"));
        let expected = std::path::absolute("data/antigravity/review").unwrap();
        assert_eq!(
            resolved_by_agy(Path::new("/h/.gemini/antigravity-cli"), &name),
            expected
        );
    }

    #[test]
    fn a_probe_runs_in_a_scratch_home() {
        let mut config = data_config(Mode::Ask);
        config.catalog_only = true;
        let args = launch_args(&config);
        let home = format!("/d/antigravity/probe-{}", std::process::id());
        assert_eq!(args[3..], [OsString::from("--gemini-dir"), home.into()]);
    }

    #[test]
    fn preparing_a_probe_removes_only_old_probe_folders() {
        let root = TempDir::new("octet-agy-probe-clean");
        let cwd = root.path().join("ws");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut config = Config::new(Engine::ANTIGRAVITY, "agy", &cwd);
        config.data_dir = Some(root.path().join("data"));
        config.catalog_only = true;
        let folder = root.path().join("data/antigravity");
        let (old, new) = (folder.join("probe-1"), folder.join("probe-2"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        let two_hours_ago = std::time::SystemTime::now() - Duration::from_hours(2);
        std::fs::File::open(&old)
            .unwrap()
            .set_modified(two_hours_ago)
            .unwrap();
        // An old link named like a probe is not followed or removed.
        let target = root.path().join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, folder.join("probe-3")).unwrap();
        let prepared = antigravity_prepare(&config, None).unwrap();
        assert!(!old.exists(), "the old probe's folder is gone");
        assert!(new.is_dir(), "a recent probe keeps its folder");
        assert!(folder.join("probe-3").is_symlink());
        assert!(target.is_dir());
        let scratch = prepared.scratch.unwrap();
        assert!(scratch.is_dir());
        assert_eq!(scratch, settings::probe_home(&root.path().join("data")));
    }

    #[test]
    fn a_probe_ignores_the_plugin_switches_and_a_session_does_not() {
        let root = TempDir::new("octet-agy-prepare-plugins");
        let cwd = root.path().join("ws");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut config = Config::new(Engine::ANTIGRAVITY, "agy", &cwd);
        config.data_dir = Some(root.path().join("data"));
        config.plugins.hooks = false;
        assert_eq!(
            antigravity_prepare(&config, None),
            Err(NO_PLUGIN_SWITCH.into())
        );
        config.catalog_only = true;
        let prepared = antigravity_prepare(&config, None).unwrap();
        assert!(prepared.scratch.is_some());
        assert_eq!(prepared.workspace, None);
    }

    #[test]
    fn the_accepted_fingerprint_is_handed_on() {
        let root = TempDir::new("octet-agy-prepare-fingerprint");
        let cwd = root.path().join("ws");
        std::fs::create_dir_all(cwd.join(".agents")).unwrap();
        std::fs::write(cwd.join(".agents/hooks.json"), "{}").unwrap();
        let mut config = Config::new(Engine::ANTIGRAVITY, "agy", &cwd);
        config.data_dir = Some(root.path().join("data"));
        config.trust_workspace = Some(super::trust::TrustedAtStart::capture(&cwd));
        let prepared = antigravity_prepare(&config, None).unwrap();
        let found = antigravity_workspace_config(&cwd).unwrap().unwrap();
        assert_eq!(prepared.workspace, Some(found.fingerprint));
        assert_eq!(prepared.scratch, None);
    }
}
