//! Trusting a workspace's own `.agents` configuration. Antigravity loads the
//! hooks, MCP servers and plugins a workspace carries in `.agents/`, and a
//! hook runs a command with no approval request and cannot be turned off. So
//! Octet starts it only in a workspace whose configuration the user trusted,
//! remembered by a fingerprint of the files, and checks before every prompt
//! that the configuration has not changed since.
use super::{guarded, settings};
use crate::live::Config;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fmt::{Display, Write as _},
    io::{ErrorKind, Read as _},
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
};

/// Most entries (files and folders) read under `.agents/plugins`.
const MAX_FILES: usize = 2000;
/// Deepest folder nesting under `.agents/plugins`.
const MAX_DEPTH: usize = 16;
/// Most bytes of configuration read in all.
const MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Most workspaces the record keeps; the oldest are dropped.
const MAX_TRUSTED: usize = 1024;
/// Most bytes of the record read; a larger one counts as empty.
const MAX_RECORD: u64 = 8 * 1024 * 1024;
/// What the record's folder holds, as a refusal to make it names it.
const RECORD_KEPT: &str = "its record of trusted Antigravity workspaces";
/// The record's name inside `<data_dir>/antigravity`.
const RECORD: &str = "trusted-workspaces.json";
/// How many paths a refusal lists before it says how many more there are.
const LISTED: usize = 8;
/// The files the workspace's configuration is made of, besides the plugins.
const FIXED: [&str; 2] = [".agents/hooks.json", ".agents/mcp_config.json"];
/// The folder of plugins, all of whose files count.
const PLUGINS: &str = ".agents/plugins";

/// A workspace's `.agents` configuration, as found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceConfig {
    /// The files, as paths relative to the workspace, sorted. A control
    /// character in a name is shown as `?`.
    pub files: Vec<String>,
    /// The SHA-256, in lowercase hex, of every file's path and bytes in that
    /// order, each with its length, so no two sets of files share one.
    pub fingerprint: String,
}

/// What `--trust-workspace-config` trusts: the workspace's own vendor
/// configuration as Octet found it when it started, never a later change.
/// Every later connection (a reconnect, `/new`, `/model`, `/mode`, the
/// phone's) accepts only this; `/trust` is the way to accept a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedAtStart {
    /// The fingerprint found at the start; `None` when there was no
    /// configuration, or it could not be read, so only an empty one passes.
    fingerprint: Option<String>,
}

impl TrustedAtStart {
    /// What the workspace `cwd` holds now, for a run started with the flag.
    #[must_use]
    pub fn capture(cwd: &Path) -> Self {
        Self {
            fingerprint: antigravity_workspace_config(cwd)
                .ok()
                .flatten()
                .map(|found| found.fingerprint),
        }
    }

    /// Whether `fingerprint` is the configuration found at the start.
    fn accepts(&self, fingerprint: &str) -> bool {
        self.fingerprint.as_deref() == Some(fingerprint)
    }
}

/// What a path is, links followed.
#[derive(Debug, PartialEq, Eq)]
enum Kind {
    /// Nothing is there, or it is a link to nothing.
    Absent,
    /// A regular file.
    File,
    /// A folder.
    Folder,
    /// A pipe, socket, device or anything else.
    Other,
}

/// `path` as shown to the user: lossy UTF-8, each control character a `?`, so
/// a file name cannot write escape sequences into the terminal.
fn show(path: &[u8]) -> String {
    String::from_utf8_lossy(path)
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// What Octet says when `rel` cannot be read.
fn cannot_read(rel: &[u8], reason: impl Display) -> String {
    format!("Octet cannot read this workspace's {}: {reason}", show(rel))
}

/// The error for a plugins folder past `MAX_FILES` or `MAX_DEPTH`.
fn too_many() -> String {
    format!(
        "Octet cannot check this workspace's {PLUGINS}: more than {MAX_FILES} files, \
         or folders nested deeper than {MAX_DEPTH}"
    )
}

/// The error for configuration past `MAX_BYTES`.
fn too_big() -> String {
    format!(
        "Octet cannot check this workspace's .agents configuration: \
         more than {} MiB of files",
        MAX_BYTES / (1024 * 1024)
    )
}

/// What `rel` is under `cwd`.
fn kind(cwd: &Path, rel: &[u8]) -> Result<Kind, String> {
    match std::fs::metadata(cwd.join(OsStr::from_bytes(rel))) {
        Ok(meta) if meta.is_file() => Ok(Kind::File),
        Ok(meta) if meta.is_dir() => Ok(Kind::Folder),
        Ok(_) => Ok(Kind::Other),
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            Ok(Kind::Absent)
        }
        Err(error) => Err(cannot_read(rel, error)),
    }
}

/// Adds the files under the folder `rel` (at nesting `depth`) to `found`,
/// counting every entry in `seen`.
fn walk(
    cwd: &Path,
    rel: &[u8],
    depth: usize,
    seen: &mut usize,
    found: &mut Vec<Vec<u8>>,
) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err(too_many());
    }
    let entries = std::fs::read_dir(cwd.join(OsStr::from_bytes(rel)))
        .map_err(|error| cannot_read(rel, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| cannot_read(rel, error))?;
        // Folders count too, so links back up the tree cannot make the walk
        // endless or exponential.
        *seen += 1;
        if *seen > MAX_FILES {
            return Err(too_many());
        }
        let mut child = rel.to_vec();
        child.push(b'/');
        child.extend_from_slice(entry.file_name().as_bytes());
        match kind(cwd, &child)? {
            Kind::Absent => {}
            Kind::File => found.push(child),
            Kind::Folder => walk(cwd, &child, depth + 1, seen, found)?,
            Kind::Other => return Err(cannot_read(&child, "not a regular file")),
        }
    }
    Ok(())
}

/// The paths of the configuration files in `cwd`, relative to it and sorted.
fn collect(cwd: &Path) -> Result<Vec<Vec<u8>>, String> {
    let mut found = Vec::new();
    for rel in FIXED {
        match kind(cwd, rel.as_bytes())? {
            Kind::Absent => {}
            Kind::File => found.push(rel.as_bytes().to_vec()),
            Kind::Folder | Kind::Other => {
                return Err(cannot_read(rel.as_bytes(), "not a regular file"));
            }
        }
    }
    match kind(cwd, PLUGINS.as_bytes())? {
        Kind::Absent => {}
        Kind::Folder => walk(cwd, PLUGINS.as_bytes(), 0, &mut 0, &mut found)?,
        Kind::File | Kind::Other => {
            return Err(cannot_read(PLUGINS.as_bytes(), "not a folder"));
        }
    }
    found.sort();
    Ok(found)
}

/// The workspace's Antigravity configuration: `.agents/hooks.json`,
/// `.agents/mcp_config.json` and every file under `.agents/plugins/`, links
/// followed. `None` when there are none.
///
/// # Errors
///
/// When a configuration file exists but cannot be read (it is not a regular
/// file, or the system refuses), or there are more than 2000 entries, folders
/// nested deeper than 16, or more than 64 MiB of files.
pub fn antigravity_workspace_config(cwd: &Path) -> Result<Option<WorkspaceConfig>, String> {
    let found = collect(cwd)?;
    if found.is_empty() {
        return Ok(None);
    }
    let mut hasher = Sha256::new();
    let mut remaining = MAX_BYTES;
    for rel in &found {
        // The type is looked at again, and before opening: a pipe would block.
        if kind(cwd, rel)? != Kind::File {
            return Err(cannot_read(rel, "not a regular file"));
        }
        let mut bytes = Vec::new();
        std::fs::File::open(cwd.join(OsStr::from_bytes(rel)))
            // No read can pass what the total still allows.
            .and_then(|file| file.take(remaining + 1).read_to_end(&mut bytes))
            .map_err(|error| cannot_read(rel, error))?;
        let len = bytes.len() as u64;
        if len > remaining {
            return Err(too_big());
        }
        remaining -= len;
        hasher.update((rel.len() as u64).to_le_bytes());
        hasher.update(rel);
        hasher.update(len.to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(Some(WorkspaceConfig {
        files: found.iter().map(|rel| show(rel)).collect(),
        fingerprint: hasher
            .finalize()
            .iter()
            .fold(String::new(), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            }),
    }))
}

/// `files` as a refusal lists them: the first eight, then how many more.
fn listed(files: &[String]) -> String {
    let mut text = files
        .iter()
        .take(LISTED)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if files.len() > LISTED {
        let _ = write!(text, " and {} more", files.len() - LISTED);
    }
    text
}

/// The key a workspace is remembered by: its canonical path.
fn workspace_key(cwd: &Path) -> Result<String, String> {
    let canonical = cwd.canonicalize().map_err(|error| error.to_string())?;
    canonical
        .into_os_string()
        .into_string()
        .map_err(|_| "the workspace's path is not valid UTF-8".to_owned())
}

/// `<data_dir>/antigravity/trusted-workspaces.json`.
fn record_path(data_dir: &Path) -> PathBuf {
    data_dir.join("antigravity").join(RECORD)
}

/// The record's entries, `(workspace, fingerprint)`, oldest first. A record
/// that is missing, over `MAX_RECORD`, or not an array counts as empty.
fn read_record(data_dir: &Path) -> Vec<(String, String)> {
    let path = record_path(data_dir);
    // Looked at before opening, so nothing there can block Octet.
    if !std::fs::metadata(&path).is_ok_and(|meta| meta.is_file()) {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    let read = std::fs::File::open(&path)
        .and_then(|file| file.take(MAX_RECORD + 1).read_to_end(&mut bytes));
    if read.is_err() || bytes.len() as u64 > MAX_RECORD {
        return Vec::new();
    }
    let Ok(Value::Array(items)) = serde_json::from_slice(&bytes) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            Some((
                item.get("workspace")?.as_str()?.to_owned(),
                item.get("fingerprint")?.as_str()?.to_owned(),
            ))
        })
        .collect()
}

/// What `antigravity_trust` fails with when the record cannot be kept.
fn not_recorded(path: &Path, reason: impl Display) -> String {
    format!(
        "Octet could not record this workspace as trusted ({}): {reason}",
        path.display()
    )
}

/// Whether the user trusted `found` in the workspace `cwd`. A workspace whose
/// path cannot be resolved is not trusted.
#[must_use]
pub fn antigravity_trusted(data_dir: &Path, cwd: &Path, found: &WorkspaceConfig) -> bool {
    let Ok(key) = workspace_key(cwd) else {
        return false;
    };
    read_record(data_dir)
        .iter()
        .any(|(workspace, fingerprint)| *workspace == key && *fingerprint == found.fingerprint)
}

/// Remembers that the user trusted `found` in the workspace `cwd`, replacing
/// what was trusted there before.
///
/// # Errors
///
/// When the record cannot be written, or would be inside Antigravity's own
/// folder.
pub fn antigravity_trust(
    data_dir: &Path,
    cwd: &Path,
    found: &WorkspaceConfig,
) -> Result<(), String> {
    trust_in(data_dir, cwd, found, settings::vendor_home().as_deref())
}

/// [`antigravity_trust`] with Antigravity's folder named.
fn trust_in(
    data_dir: &Path,
    cwd: &Path,
    found: &WorkspaceConfig,
    vendor_home: Option<&Path>,
) -> Result<(), String> {
    let key = workspace_key(cwd).map_err(|reason| not_recorded(&record_path(data_dir), reason))?;
    remember(data_dir, &key, &found.fingerprint, vendor_home)
}

/// Writes the record with `workspace` and `fingerprint` as its newest entry
/// and no older entry for `workspace`, keeping at most `MAX_TRUSTED`.
fn remember(
    data_dir: &Path,
    workspace: &str,
    fingerprint: &str,
    vendor_home: Option<&Path>,
) -> Result<(), String> {
    let dir = guarded::folder(data_dir, Path::new("antigravity"), vendor_home, RECORD_KEPT)
        .map_err(|failure| failure.into_text(|path, reason| not_recorded(path, reason)))?;
    let mut entries = read_record(data_dir);
    entries.retain(|(known, _)| known != workspace);
    entries.push((workspace.to_owned(), fingerprint.to_owned()));
    entries.drain(..entries.len().saturating_sub(MAX_TRUSTED));
    let text = Value::Array(
        entries
            .iter()
            .map(|(workspace, fingerprint)| {
                json!({"workspace": workspace, "fingerprint": fingerprint})
            })
            .collect(),
    )
    .to_string();
    guarded::write_private(&dir, RECORD, text.as_bytes())
        .map_err(|error| not_recorded(&dir.join(RECORD), error))
}

/// The check before Antigravity starts: the fingerprint of the configuration
/// that was accepted (`None` when the workspace has none), for [`unchanged`]
/// to compare with.
///
/// # Errors
///
/// When the configuration cannot be checked, or is neither trusted nor what
/// `config.trust_workspace` trusted when Octet started.
pub(super) fn check(config: &Config, data_dir: &Path) -> Result<Option<String>, String> {
    let Some(found) = antigravity_workspace_config(&config.cwd)? else {
        return Ok(None);
    };
    let flagged = config
        .trust_workspace
        .as_ref()
        .is_some_and(|start| start.accepts(&found.fingerprint));
    if flagged || antigravity_trusted(data_dir, &config.cwd, &found) {
        return Ok(Some(found.fingerprint));
    }
    if config.trust_workspace.is_some() {
        return Err(format!(
            "This workspace's .agents configuration changed since Octet started; \
             --trust-workspace-config trusts only what was there then: {}. \
             Review it, then type /trust in Octet's terminal interface",
            listed(&found.files)
        ));
    }
    Err(format!(
        "Antigravity would load this workspace's .agents configuration, which can run \
         commands without asking: {}. Review it, then type /trust in Octet's terminal \
         interface, or pass --trust-workspace-config",
        listed(&found.files)
    ))
}

/// The check before every prompt: the configuration is what it was when the
/// session started (`started_with`, the fingerprint found then; `None` when
/// nothing was).
///
/// # Errors
///
/// When the configuration differs, or cannot be checked (the error says why).
pub(super) fn unchanged(cwd: &Path, started_with: Option<&str>) -> Result<(), String> {
    match antigravity_workspace_config(cwd)? {
        Some(now) if started_with != Some(now.fingerprint.as_str()) => Err(format!(
            "This workspace's .agents configuration changed since this session started: {}. \
             Review it, then type /trust in Octet's terminal interface, \
             or start again with --trust-workspace-config",
            listed(&now.files)
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octet_testkit::TempDir;
    use std::{
        os::unix::fs::{PermissionsExt as _, symlink},
        sync::mpsc,
        time::Duration,
    };

    /// Writes `text` to `path` under `root`, making the folders.
    fn put(root: &Path, path: &str, text: &str) {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, text).unwrap();
    }

    /// A workspace folder inside `root` with `.agents/hooks.json`.
    fn workspace(root: &Path, name: &str) -> PathBuf {
        let cwd = root.join(name);
        put(&cwd, ".agents/hooks.json", "{}");
        cwd
    }

    fn found(cwd: &Path) -> WorkspaceConfig {
        antigravity_workspace_config(cwd).unwrap().unwrap()
    }

    fn config(cwd: &Path, trust_workspace: bool) -> Config {
        let mut config = crate::live::Config::new(crate::live::Engine::ANTIGRAVITY, "agy", cwd);
        config.trust_workspace = trust_workspace.then(|| TrustedAtStart::capture(cwd));
        config
    }

    #[test]
    fn the_flag_trusts_only_what_was_there_when_octet_started() {
        let root = TempDir::new("octet-agy-trust-start");
        let data = root.path().join("data");
        let cwd = workspace(root.path(), "w");
        let start = config(&cwd, true);
        let fingerprint = found(&cwd).fingerprint;
        assert_eq!(check(&start, &data), Ok(Some(fingerprint.clone())));
        put(&cwd, ".agents/hooks.json", r#"{"hooks":[]}"#);
        let error = check(&start, &data).unwrap_err();
        assert_eq!(
            error,
            "This workspace's .agents configuration changed since Octet started; \
             --trust-workspace-config trusts only what was there then: .agents/hooks.json. \
             Review it, then type /trust in Octet's terminal interface"
        );
        // `/trust` accepts the change.
        trust_in(&data, &cwd, &found(&cwd), None).unwrap();
        assert!(check(&start, &data).is_ok());
        put(&cwd, ".agents/hooks.json", "{}");
        assert_eq!(check(&start, &data), Ok(Some(fingerprint)));
    }

    #[test]
    fn the_flag_with_nothing_at_the_start_accepts_only_nothing_later() {
        let root = TempDir::new("octet-agy-trust-start-none");
        let data = root.path().join("data");
        let cwd = root.path().join("w");
        std::fs::create_dir_all(&cwd).unwrap();
        let start = config(&cwd, true);
        assert_eq!(
            start.trust_workspace,
            Some(TrustedAtStart { fingerprint: None })
        );
        assert_eq!(check(&start, &data), Ok(None));
        put(&cwd, ".agents/hooks.json", "{}");
        assert!(check(&start, &data).is_err());
        // Unreadable at the start counts as nothing.
        let odd = root.path().join("odd");
        std::fs::create_dir_all(odd.join(".agents/hooks.json")).unwrap();
        assert_eq!(
            TrustedAtStart::capture(&odd),
            TrustedAtStart { fingerprint: None }
        );
    }

    #[test]
    fn a_workspace_without_agents_configuration_needs_no_trust() {
        let root = TempDir::new("octet-agy-trust-none");
        let cwd = root.path().join("w");
        std::fs::create_dir_all(&cwd).unwrap();
        assert_eq!(antigravity_workspace_config(&cwd), Ok(None));
        put(&cwd, ".agents/notes.md", "notes");
        assert_eq!(antigravity_workspace_config(&cwd), Ok(None));
        std::fs::create_dir_all(cwd.join(".agents/plugins")).unwrap();
        assert_eq!(antigravity_workspace_config(&cwd), Ok(None));
    }

    #[test]
    fn hooks_mcp_servers_and_plugins_are_found_and_listed_in_order() {
        let root = TempDir::new("octet-agy-trust-list");
        let cwd = root.path().join("w");
        put(&cwd, ".agents/plugins/p/run.sh", "echo");
        put(&cwd, ".agents/mcp_config.json", "{}");
        put(&cwd, ".agents/hooks.json", "{}");
        let config = found(&cwd);
        assert_eq!(
            config.files,
            [
                ".agents/hooks.json",
                ".agents/mcp_config.json",
                ".agents/plugins/p/run.sh"
            ]
        );
        assert_eq!(config.fingerprint.len(), 64);
        assert!(
            config
                .fingerprint
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
    }

    #[test]
    fn trust_is_remembered_for_the_workspace_and_its_contents() {
        let root = TempDir::new("octet-agy-trust-remember");
        let data = root.path().join("data");
        let (one, two) = (workspace(root.path(), "one"), workspace(root.path(), "two"));
        let config = found(&one);
        assert!(!antigravity_trusted(&data, &one, &config));
        trust_in(&data, &one, &config, None).unwrap();
        assert!(antigravity_trusted(&data, &one, &config));
        assert_eq!(found(&two), config, "the same files");
        assert!(!antigravity_trusted(&data, &two, &found(&two)));
        // Only the record file, owner-only, is there.
        let names: Vec<_> = std::fs::read_dir(data.join("antigravity"))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].file_name(), RECORD);
        let mode = std::fs::metadata(record_path(&data))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_changed_file_must_be_trusted_again() {
        let root = TempDir::new("octet-agy-trust-changed");
        let data = root.path().join("data");
        let cwd = workspace(root.path(), "w");
        trust_in(&data, &cwd, &found(&cwd), None).unwrap();
        put(&cwd, ".agents/hooks.json", r#"{"hooks":[]}"#);
        assert!(!antigravity_trusted(&data, &cwd, &found(&cwd)));
    }

    #[test]
    fn an_added_plugin_file_must_be_trusted_again() {
        let root = TempDir::new("octet-agy-trust-added");
        let data = root.path().join("data");
        let cwd = workspace(root.path(), "w");
        trust_in(&data, &cwd, &found(&cwd), None).unwrap();
        put(&cwd, ".agents/plugins/new/run.sh", "echo");
        assert!(!antigravity_trusted(&data, &cwd, &found(&cwd)));
    }

    #[test]
    fn a_change_behind_a_link_is_seen() {
        let root = TempDir::new("octet-agy-trust-link");
        let cwd = root.path().join("w");
        put(&cwd, ".agents/hooks.json", "{}");
        put(&cwd, "shared/run.sh", "echo one");
        symlink("../shared", cwd.join(".agents/plugins")).unwrap();
        let before = found(&cwd);
        assert_eq!(
            before.files,
            [".agents/hooks.json", ".agents/plugins/run.sh"]
        );
        put(&cwd, "shared/run.sh", "echo two");
        assert_ne!(found(&cwd).fingerprint, before.fingerprint);
    }

    #[test]
    fn a_link_pointed_elsewhere_is_seen() {
        let root = TempDir::new("octet-agy-trust-relink");
        let cwd = root.path().join("w");
        put(&cwd, "a.json", "{}");
        put(&cwd, "b.json", r#"{"hooks":1}"#);
        std::fs::create_dir_all(cwd.join(".agents")).unwrap();
        symlink("../a.json", cwd.join(".agents/hooks.json")).unwrap();
        let before = found(&cwd);
        std::fs::remove_file(cwd.join(".agents/hooks.json")).unwrap();
        symlink("../b.json", cwd.join(".agents/hooks.json")).unwrap();
        assert_ne!(found(&cwd).fingerprint, before.fingerprint);
    }

    #[test]
    fn a_plugins_folder_too_deep_or_too_full_cannot_be_trusted() {
        let root = TempDir::new("octet-agy-trust-bounds");
        let deep = root.path().join("deep");
        let nested = (1..=17)
            .map(|n| format!("d{n}"))
            .collect::<Vec<_>>()
            .join("/");
        put(&deep, &format!(".agents/plugins/{nested}/run.sh"), "echo");
        let error = antigravity_workspace_config(&deep).unwrap_err();
        assert_eq!(
            error,
            "Octet cannot check this workspace's .agents/plugins: \
             more than 2000 files, or folders nested deeper than 16"
        );
        // Sixteen is allowed.
        let ok = root.path().join("ok");
        let nested = (1..=16)
            .map(|n| format!("d{n}"))
            .collect::<Vec<_>>()
            .join("/");
        put(&ok, &format!(".agents/plugins/{nested}/run.sh"), "echo");
        assert!(antigravity_workspace_config(&ok).unwrap().is_some());
        // Too many files.
        let full = root.path().join("full");
        std::fs::create_dir_all(full.join(".agents/plugins")).unwrap();
        for n in 0..=MAX_FILES {
            std::fs::write(full.join(format!(".agents/plugins/f{n}")), "x").unwrap();
        }
        assert_eq!(antigravity_workspace_config(&full).unwrap_err(), error);
    }

    #[test]
    fn more_than_the_byte_limit_cannot_be_trusted() {
        let root = TempDir::new("octet-agy-trust-big");
        let cwd = root.path().join("w");
        put(&cwd, ".agents/plugins/big", "");
        let big = std::fs::OpenOptions::new()
            .write(true)
            .open(cwd.join(".agents/plugins/big"))
            .unwrap();
        big.set_len(MAX_BYTES + 1).unwrap();
        let error = antigravity_workspace_config(&cwd).unwrap_err();
        assert!(
            error.starts_with("Octet cannot check this workspace's .agents"),
            "{error}"
        );
        assert!(error.contains("64 MiB"), "{error}");
        big.set_len(MAX_BYTES).unwrap();
        assert!(antigravity_workspace_config(&cwd).unwrap().is_some());
    }

    #[test]
    fn trusting_again_keeps_one_entry_and_the_list_stays_bounded() {
        let root = TempDir::new("octet-agy-trust-bounded");
        let data = root.path().join("data");
        for n in 0..=MAX_TRUSTED {
            remember(&data, &format!("/fake/{n}"), "fp", None).unwrap();
        }
        let entries = read_record(&data);
        assert_eq!(entries.len(), MAX_TRUSTED);
        assert!(!entries.iter().any(|(w, _)| w == "/fake/0"));
        assert_eq!(entries.last().unwrap().0, format!("/fake/{MAX_TRUSTED}"));
        // Again: one entry, now the newest.
        remember(&data, "/fake/500", "fp2", None).unwrap();
        let entries = read_record(&data);
        assert_eq!(entries.len(), MAX_TRUSTED);
        assert_eq!(entries.iter().filter(|(w, _)| w == "/fake/500").count(), 1);
        assert_eq!(
            entries.last().unwrap(),
            &("/fake/500".to_owned(), "fp2".to_owned())
        );
    }

    #[test]
    fn an_untrusted_workspace_is_refused_naming_its_files() {
        let root = TempDir::new("octet-agy-trust-check");
        let data = root.path().join("data");
        let bare = root.path().join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        assert_eq!(check(&config(&bare, false), &data), Ok(None));
        let cwd = workspace(root.path(), "w");
        let error = check(&config(&cwd, false), &data).unwrap_err();
        assert_eq!(
            error,
            "Antigravity would load this workspace's .agents configuration, which can run \
             commands without asking: .agents/hooks.json. Review it, then type /trust in \
             Octet's terminal interface, or pass --trust-workspace-config"
        );
        let fingerprint = found(&cwd).fingerprint;
        assert_eq!(
            check(&config(&cwd, true), &data),
            Ok(Some(fingerprint.clone()))
        );
        assert!(!data.exists(), "a run-only trust records nothing");
        trust_in(&data, &cwd, &found(&cwd), None).unwrap();
        assert_eq!(check(&config(&cwd, false), &data), Ok(Some(fingerprint)));
    }

    #[test]
    fn more_than_eight_files_are_summarised() {
        let root = TempDir::new("octet-agy-trust-many");
        let cwd = root.path().join("w");
        for n in 0..10 {
            put(&cwd, &format!(".agents/plugins/p{n}.sh"), "echo");
        }
        let error = check(&config(&cwd, false), &root.path().join("data")).unwrap_err();
        assert!(
            error.contains(".agents/plugins/p7.sh and 2 more. Review it"),
            "{error}"
        );
        assert!(!error.contains("p8.sh"), "{error}");
    }

    #[test]
    fn configuration_that_changes_or_appears_after_the_start_is_reported() {
        let root = TempDir::new("octet-agy-trust-unchanged");
        let cwd = root.path().join("w");
        std::fs::create_dir_all(&cwd).unwrap();
        assert_eq!(unchanged(&cwd, None), Ok(()));
        put(&cwd, ".agents/hooks.json", "{}");
        let error = unchanged(&cwd, None).unwrap_err();
        assert_eq!(
            error,
            "This workspace's .agents configuration changed since this session started: \
             .agents/hooks.json. Review it, then type /trust in Octet's terminal interface, \
             or start again with --trust-workspace-config"
        );
        let first = found(&cwd).fingerprint;
        assert_eq!(unchanged(&cwd, Some(&first)), Ok(()));
        put(&cwd, ".agents/hooks.json", r#"{"hooks":[]}"#);
        let error = unchanged(&cwd, Some(&first)).unwrap_err();
        assert!(
            error.starts_with("This workspace's .agents configuration changed"),
            "{error}"
        );
        std::fs::remove_dir_all(cwd.join(".agents")).unwrap();
        assert_eq!(unchanged(&cwd, Some(&first)), Ok(()));
    }

    #[test]
    fn the_trust_record_is_never_written_inside_the_vendors_home() {
        let root = TempDir::new("octet-agy-trust-vendor");
        let home = root.path().join("home/.gemini");
        std::fs::create_dir_all(&home).unwrap();
        let data = home.join("octet");
        let cwd = workspace(root.path(), "w");
        let error = trust_in(&data, &cwd, &found(&cwd), Some(&home)).unwrap_err();
        assert!(
            error.starts_with(
                "Octet will not keep its record of trusted Antigravity workspaces inside"
            ),
            "{error}"
        );
        assert!(!record_path(&data).exists());
        assert!(!data.exists());
    }

    #[test]
    fn a_record_that_cannot_be_written_is_reported_as_such() {
        let root = TempDir::new("octet-agy-trust-blocked");
        let data = root.path().join("data");
        std::fs::create_dir_all(root.path()).unwrap();
        std::fs::write(&data, "not a folder").unwrap();
        let cwd = workspace(root.path(), "w");
        let error = trust_in(&data, &cwd, &found(&cwd), None).unwrap_err();
        assert!(
            error.starts_with("Octet could not record this workspace as trusted ("),
            "{error}"
        );
    }

    #[test]
    fn a_record_that_cannot_be_parsed_or_is_too_big_counts_as_empty() {
        let root = TempDir::new("octet-agy-trust-bad");
        let data = root.path().join("data");
        let cwd = workspace(root.path(), "w");
        let config = found(&cwd);
        let key = workspace_key(&cwd).unwrap();
        let entry = format!(
            r#"[{{"workspace":{key:?},"fingerprint":"{}"}}"#,
            config.fingerprint
        );
        put(
            &data,
            "antigravity/trusted-workspaces.json",
            &format!("{entry}]"),
        );
        assert!(antigravity_trusted(&data, &cwd, &config));
        put(&data, "antigravity/trusted-workspaces.json", &entry);
        assert!(!antigravity_trusted(&data, &cwd, &config), "cut off");
        put(
            &data,
            "antigravity/trusted-workspaces.json",
            r#"{"workspace":1}"#,
        );
        assert!(!antigravity_trusted(&data, &cwd, &config), "not an array");
        // Valid and matching, but one byte over the limit: padded with spaces.
        let padding = usize::try_from(MAX_RECORD).unwrap() + 1 - entry.len() - 1;
        put(
            &data,
            "antigravity/trusted-workspaces.json",
            &format!("{entry}{}]", " ".repeat(padding)),
        );
        assert!(!antigravity_trusted(&data, &cwd, &config), "too big");
    }

    #[test]
    fn a_workspace_that_cannot_be_resolved_is_not_trusted_and_not_recorded() {
        let root = TempDir::new("octet-agy-trust-gone");
        let data = root.path().join("data");
        let cwd = workspace(root.path(), "w");
        let config = found(&cwd);
        let gone = root.path().join("gone");
        assert!(!antigravity_trusted(&data, &gone, &config));
        let error = trust_in(&data, &gone, &config, None).unwrap_err();
        assert!(
            error.starts_with("Octet could not record this workspace as trusted ("),
            "{error}"
        );
        assert!(!record_path(&data).exists());
    }

    #[test]
    fn a_directory_where_a_file_belongs_is_refused_and_a_dangling_link_is_absent() {
        let root = TempDir::new("octet-agy-trust-kinds");
        let cwd = root.path().join("w");
        std::fs::create_dir_all(cwd.join(".agents/hooks.json")).unwrap();
        let error = antigravity_workspace_config(&cwd).unwrap_err();
        assert_eq!(
            error,
            "Octet cannot read this workspace's .agents/hooks.json: not a regular file"
        );
        std::fs::remove_dir(cwd.join(".agents/hooks.json")).unwrap();
        symlink("nowhere.json", cwd.join(".agents/hooks.json")).unwrap();
        symlink("nowhere", cwd.join(".agents/plugins")).unwrap();
        assert_eq!(antigravity_workspace_config(&cwd), Ok(None));
        // A directory under plugins is a folder to walk, a link to nothing is skipped.
        std::fs::remove_file(cwd.join(".agents/plugins")).unwrap();
        std::fs::create_dir_all(cwd.join(".agents/plugins")).unwrap();
        symlink("nowhere", cwd.join(".agents/plugins/dead")).unwrap();
        assert_eq!(antigravity_workspace_config(&cwd), Ok(None));
    }

    #[test]
    fn the_same_bytes_split_between_a_name_and_its_content_differ() {
        let root = TempDir::new("octet-agy-trust-split");
        let (one, two) = (root.path().join("one"), root.path().join("two"));
        put(&one, ".agents/plugins/ab", "c");
        put(&two, ".agents/plugins/a", "bc");
        assert_ne!(found(&one).fingerprint, found(&two).fingerprint);
    }

    #[test]
    fn a_control_character_in_a_name_is_shown_as_a_question_mark() {
        let root = TempDir::new("octet-agy-trust-escape");
        let cwd = root.path().join("w");
        put(&cwd, ".agents/plugins/a\u{1b}[2Jb", "echo");
        let config = found(&cwd);
        assert_eq!(config.files, [".agents/plugins/a?[2Jb"]);
        let error = check(&self::config(&cwd, false), &root.path().join("data")).unwrap_err();
        assert!(
            error.contains("a?[2Jb") && !error.contains('\u{1b}'),
            "{error}"
        );
        let error = unchanged(&cwd, None).unwrap_err();
        assert!(
            error.contains("a?[2Jb") && !error.contains('\u{1b}'),
            "{error}"
        );
    }

    #[test]
    fn a_configuration_that_cannot_be_checked_is_reported_as_that_when_unchanged_is_asked() {
        let root = TempDir::new("octet-agy-trust-unchecked");
        let cwd = root.path().join("w");
        std::fs::create_dir_all(cwd.join(".agents/hooks.json")).unwrap();
        let error = unchanged(&cwd, Some("abc")).unwrap_err();
        assert!(
            error.starts_with("Octet cannot read this workspace's .agents/hooks.json"),
            "{error}"
        );
    }

    /// Runs `call` on a thread and fails the test if it has not returned in
    /// five seconds, so a regression fails instead of hanging the suite.
    fn within_five_seconds<T: Send + 'static>(call: impl FnOnce() -> T + Send + 'static) -> T {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(call());
        });
        receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("the call waited on a named pipe")
    }

    fn mkfifo(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn a_named_pipe_where_a_file_should_be_is_refused_without_waiting() {
        let root = TempDir::new("octet-agy-trust-fifo");
        for rel in [".agents/hooks.json", ".agents/plugins/p/run.sh"] {
            let cwd = root.path().join(rel.replace('/', "_"));
            mkfifo(&cwd.join(rel));
            let seen = cwd.clone();
            let error =
                within_five_seconds(move || antigravity_workspace_config(&seen)).unwrap_err();
            assert!(
                error.starts_with(&format!("Octet cannot read this workspace's {rel}")),
                "{error}"
            );
            let error = within_five_seconds(move || unchanged(&cwd, None)).unwrap_err();
            assert!(
                error.starts_with(&format!("Octet cannot read this workspace's {rel}")),
                "{error}"
            );
        }
    }
}
