//! Who may drive a run, who may write its work tree, and which processes are the run's.
//!
//! Two OS locks (`File::try_lock`, which die with the process) make one Octet drive a run and one
//! run write a work tree: [`lock`] takes the run's `lock` file, then the work tree's
//! `<data_dir>/fleet/worktrees/<hash>.lock`. The work tree's [`WorktreeRecord`] lists every run
//! with unresolved ownership of it; it is changed only while a [`Held`] proves the lock, and
//! [`admit`] decides from it whether a run may write. [`list_processes`] and [`check`] tell which
//! live processes are a run's, and [`kill_verified`] stops only those.
//!
//! A process is the run's when it carries the attempt's `OCTET_FLEET_ATTEMPT` marker in its
//! environment or matches a registered `(pid, start time)` identity, or when its parent is one
//! of the run's (see [`check`]: a live verified parent really forked it, and an orphan's parent
//! is init, never a reused PID). A process in a registered group with none of these is only ever
//! reported, never signalled.
//!
//! On macOS `ps` shows no environment for Apple's own platform binaries (`/bin/sh`, `sleep`,
//! `git`, ...), so the marker is invisible for them: the gate, which is `/bin/sh` until it
//! `exec`s the vendor, is verified by its registered identity, and an Apple-binary descendant
//! by its ancestry. An orphan of such a binary that left its parent is seen only as a member of
//! a registered group, so it is reported as unverified (a pause, never a signal). Linux reads
//! every environment, so the marker is always visible there.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use octet_store::{create_private, create_private_dir, sync_directory};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::time::Instant;

use crate::record::{AttemptId, Identity};
use crate::recover::ProcessCheck;
use crate::snapshot::{SnapshotError, command, exec};
use crate::store::RunDir;

/// The environment variable the gate exports: `<run>/<attempt>`, inherited by everything the
/// attempt starts.
const MARKER_VAR: &str = "OCTET_FLEET_ATTEMPT";

/// How long [`kill_verified`] waits after SIGTERM before SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(5);

/// How long [`kill_verified`] waits after SIGKILL for the processes to be gone.
const KILL_GRACE: Duration = Duration::from_secs(1);

/// How often [`kill_verified`] looks.
const LOOK: Duration = Duration::from_millis(100);

/// Why ownership could not be taken, read or checked.
#[derive(Debug, thiserror::Error)]
pub enum OwnershipError {
    /// Another Octet is driving this run.
    #[error("another Octet is driving this run")]
    RunBusy,
    /// Another run holds the work tree.
    #[error("another run holds this work tree{}", holder_note(.holder.as_deref()))]
    WorktreeBusy {
        /// The run that holds it, when it could be read from the lock file.
        holder: Option<String>,
    },
    /// The lock held is for another work tree than the record being written.
    #[error("the lock held is for work tree {held}, not {asked}")]
    WrongWorktree {
        /// The work tree the held lock is for.
        held: String,
        /// The work tree the record is for.
        asked: String,
    },
    /// Another run still has unresolved ownership of the work tree.
    #[error("run {run} still has unresolved ownership of this work tree; resume or stop it first")]
    OtherRunUnresolved {
        /// The run that must be resumed or stopped.
        run: String,
    },
    /// A lock, the record or the process list could not be read or written.
    #[error("ownership I/O: {0}")]
    Io(#[from] io::Error),
    /// The worktree record is not valid JSON of the expected shape.
    #[error("the worktree record is unreadable: {0}")]
    Json(#[from] serde_json::Error),
    /// Git could not hash the work tree's path.
    #[error("{0}")]
    Git(#[from] SnapshotError),
    /// `ps` could not list the processes.
    #[error("cannot list processes: {0}")]
    Ps(String),
}

fn holder_note(holder: Option<&str>) -> String {
    holder.map_or_else(String::new, |run| format!(" (run {run})"))
}

/// The run lock and the work tree lock, both held. Dropping it empties the run lock file (so
/// [`run_holder`] no longer names this process) and releases both.
#[derive(Debug)]
pub struct Held {
    run: File,
    _worktree: File,
    /// The work tree the lock is for.
    hash: String,
}

impl Drop for Held {
    fn drop(&mut self) {
        let _ = self.run.set_len(0);
    }
}

/// Takes the locks for driving run `run`, whose ID is the name of its directory, on the work
/// tree named by `worktree_hash` (see [`worktree_hash`]). The run lock goes first, then the
/// work tree's, whose file then holds the run's ID so a refused run can name it. Once both are
/// held, the run lock file holds this process's PID and start time, which [`run_holder`] reads
/// without locking. The run's log is never opened here.
///
/// # Errors
///
/// [`OwnershipError::RunBusy`] when another Octet drives the run;
/// [`OwnershipError::WorktreeBusy`] when another run holds the work tree (the run lock is then
/// released again); [`OwnershipError::Io`] otherwise.
pub async fn lock(
    data_dir: &Path,
    run: &RunDir,
    worktree_hash: &str,
) -> Result<Held, OwnershipError> {
    let run_id = run
        .root
        .file_name()
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "the run directory has no name")
        })?
        .to_string_lossy()
        .into_owned();
    let dir = worktrees_dir(data_dir);
    create_private_dir(&dir).await?;
    let run_file = open_lock(&run.root.join("lock"))?;
    try_lock(&run_file).map_err(|e| e.unwrap_or(OwnershipError::RunBusy))?;
    let path = dir.join(format!("{worktree_hash}.lock"));
    let worktree_file = open_lock(&path)?;
    try_lock(&worktree_file).map_err(|e| {
        e.unwrap_or_else(|| OwnershipError::WorktreeBusy {
            holder: read_holder(&path),
        })
    })?;
    // Truncate and write after the lock is ours: a refused run reads the previous holder's name
    // only while that holder still holds the lock.
    worktree_file.set_len(0)?;
    (&worktree_file).write_all(run_id.as_bytes())?;
    worktree_file.sync_all()?;
    let held = Held {
        run: run_file,
        _worktree: worktree_file,
        hash: worktree_hash.to_string(),
    };
    // Unreadable, our own start time leaves the file empty: the run is then never reported
    // as driven, which only `status` reads.
    let pid = std::process::id();
    held.run.set_len(0)?;
    if let Some(start) = start_time(pid).await {
        (&held.run).write_all(format!("{pid} {start}").as_bytes())?;
        held.run.sync_all()?;
    }
    Ok(held)
}

/// The process driving the run whose directory is `run`: the PID [`lock`] wrote into the run
/// lock file, while a process with that PID and the start time written beside it is alive.
/// Takes no lock: an empty, missing or stale file is no driver.
pub async fn run_holder(run: &RunDir) -> Option<u32> {
    let text = tokio::fs::read_to_string(run.root.join("lock"))
        .await
        .ok()?;
    let (pid, start) = text.trim().split_once(' ')?;
    let pid: u32 = pid.parse().ok()?;
    (start_time(pid).await.as_deref() == Some(start)).then_some(pid)
}

/// Opens (creating, owner-only) a lock file without changing its contents.
fn open_lock(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
}

/// Tries the exclusive lock: `Err(None)` when someone else holds it.
fn try_lock(file: &File) -> Result<(), Option<OwnershipError>> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(None),
        Err(TryLockError::Error(e)) => Err(Some(e.into())),
    }
}

/// The run ID the holder wrote into the lock file; `None` when it is absent or unreadable.
fn read_holder(path: &Path) -> Option<String> {
    let mut text = String::new();
    File::open(path).ok()?.read_to_string(&mut text).ok()?;
    let id = text.trim();
    (!id.is_empty()).then(|| id.to_string())
}

fn worktrees_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("fleet").join("worktrees")
}

/// The hash that names a work tree: git's hash of its canonical top-level path.
///
/// # Errors
///
/// [`OwnershipError::Io`] when `top` cannot be canonicalized; [`OwnershipError::Git`] when git
/// cannot hash it.
pub async fn worktree_hash(top: &Path) -> Result<String, OwnershipError> {
    let canonical = tokio::fs::canonicalize(top).await?;
    let cmd = command(&canonical, ["hash-object", "--stdin"]);
    let out = exec(cmd, canonical.as_os_str().as_bytes()).await?;
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

/// The runs with unresolved ownership of one work tree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRecord {
    /// Run ID to what the run has not resolved.
    pub runs: BTreeMap<String, Unresolved>,
}

/// What one run has not resolved: its last write attempt and the processes registered for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unresolved {
    /// The attempt that may still be writing.
    pub attempt: AttemptId,
    /// The processes registered for it, not yet shown to be gone.
    pub identities: Vec<Identity>,
}

fn record_path(data_dir: &Path, hash: &str) -> PathBuf {
    worktrees_dir(data_dir).join(format!("{hash}.json"))
}

/// Reads the work tree's record; a work tree with none has an empty one.
///
/// # Errors
///
/// [`OwnershipError::Io`] when the file cannot be read; [`OwnershipError::Json`] when it is not a
/// record. An unparsable file is never taken for an empty one.
pub async fn load(data_dir: &Path, hash: &str) -> Result<WorktreeRecord, OwnershipError> {
    match tokio::fs::read(record_path(data_dir, hash)).await {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(WorktreeRecord::default()),
        Err(e) => Err(e.into()),
    }
}

/// Replaces the work tree's record, durably: written aside, synced, renamed over it, and the
/// directory synced. `held` proves the work tree lock is ours; the record changes only under it.
///
/// # Errors
///
/// [`OwnershipError::WrongWorktree`] when `held` is the lock of another work tree than `hash`;
/// [`OwnershipError::Io`] or [`OwnershipError::Json`] when it cannot be written.
pub async fn save(
    held: &Held,
    data_dir: &Path,
    hash: &str,
    record: &WorktreeRecord,
) -> Result<(), OwnershipError> {
    if held.hash != hash {
        return Err(OwnershipError::WrongWorktree {
            held: held.hash.clone(),
            asked: hash.to_string(),
        });
    }
    let dir = worktrees_dir(data_dir);
    create_private_dir(&dir).await?;
    let path = record_path(data_dir, hash);
    let tmp = dir.join(format!("{hash}.json.tmp"));
    // A leftover from a crash would fail the exclusive create.
    match tokio::fs::remove_file(&tmp).await {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let mut file = create_private(&tmp).await?;
    file.write_all(&serde_json::to_vec_pretty(record)?).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&tmp, &path).await?;
    sync_directory(&dir).await?;
    Ok(())
}

/// Whether `driving` may write the work tree: no run other than it is listed.
///
/// # Errors
///
/// [`OwnershipError::OtherRunUnresolved`] naming the first other listed run.
pub fn admit(record: &WorktreeRecord, driving: &str) -> Result<(), OwnershipError> {
    match record.runs.keys().find(|run| run.as_str() != driving) {
        Some(run) => Err(OwnershipError::OtherRunUnresolved { run: run.clone() }),
        None => Ok(()),
    }
}

/// One live process, as the platform lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    /// The process ID.
    pub pid: u32,
    /// Its parent: `1` (or a subreaper) once the real parent has exited.
    pub ppid: u32,
    /// Its process group.
    pub pgid: u32,
    /// Its start time, in the form [`start_time`] reports and an [`Identity`] stores.
    pub start: String,
    /// The `OCTET_FLEET_ATTEMPT` value in its environment, if any.
    pub marker: Option<String>,
    /// Its command line.
    pub command: String,
}

/// Lists every process the platform shows, with its environment marker where readable.
///
/// # Errors
///
/// [`OwnershipError::Ps`] when `ps` fails (macOS); [`OwnershipError::Io`] when `/proc` cannot be
/// scanned (Linux).
pub async fn list_processes() -> Result<Vec<ProcessInfo>, OwnershipError> {
    platform::list().await
}

/// The start time of `pid` exactly as [`list_processes`] reports it; `None` when it is gone.
pub async fn start_time(pid: u32) -> Option<String> {
    platform::start(pid).await
}

/// The command line of `pid`, without its environment, as a pause lists it; `None` when it is
/// gone or cannot be read.
pub async fn command_of(pid: u32) -> Option<String> {
    platform::command(pid).await
}

#[cfg(target_os = "macos")]
mod platform {
    use tokio::process::Command;

    use super::{MARKER_VAR, OwnershipError, ProcessInfo};

    /// `ps` with the fixed-width-free `lstart` form `Www Mmm dd hh:mm:ss yyyy`, in a process
    /// group of its own: a terminal's Ctrl+C is for Octet, not for its listing.
    fn ps() -> Command {
        let mut cmd = Command::new("ps");
        cmd.env("LC_ALL", "C").process_group(0).kill_on_drop(true);
        cmd
    }

    pub(super) async fn list() -> Result<Vec<ProcessInfo>, OwnershipError> {
        let out = ps()
            .args(["-A", "-E", "-ww", "-o", "pid=,ppid=,pgid=,lstart=,command="])
            .output()
            .await?;
        if !out.status.success() {
            return Err(OwnershipError::Ps(format!(
                "{}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(parse_line)
            .collect())
    }

    pub(super) async fn start(pid: u32) -> Option<String> {
        let out = ps()
            .args(["-ww", "-o", "lstart=", "-p", &pid.to_string()])
            .output()
            .await
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let start = text.split_whitespace().collect::<Vec<_>>().join(" ");
        (out.status.success() && !start.is_empty()).then_some(start)
    }

    pub(super) async fn command(pid: u32) -> Option<String> {
        let out = ps()
            .args(["-ww", "-o", "command=", "-p", &pid.to_string()])
            .output()
            .await
            .ok()?;
        let command = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !command.is_empty()).then_some(command)
    }

    /// The value of `OCTET_FLEET_ATTEMPT=`, which `ps -E` appends to the command, among its
    /// whitespace-separated tokens: the last one, since the environment follows the
    /// arguments (an argument that merely spells a marker comes before the real one).
    pub(super) fn marker_in(text: &str) -> Option<String> {
        let prefix = format!("{MARKER_VAR}=");
        text.split_whitespace()
            .rev()
            .find_map(|token| token.strip_prefix(prefix.as_str()))
            .map(str::to_string)
    }

    /// Takes the next whitespace-delimited token off `rest`.
    fn token<'a>(rest: &mut &'a str) -> Option<&'a str> {
        let trimmed = rest.trim_start();
        let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        if end == 0 {
            return None;
        }
        let (token, tail) = trimmed.split_at(end);
        *rest = tail;
        Some(token)
    }

    /// One `pid ppid pgid Www Mmm dd hh:mm:ss yyyy command…` line.
    pub(super) fn parse_line(line: &str) -> Option<ProcessInfo> {
        let mut rest = line;
        let pid = token(&mut rest)?.parse().ok()?;
        let ppid = token(&mut rest)?.parse().ok()?;
        let pgid = token(&mut rest)?.parse().ok()?;
        let mut start = Vec::with_capacity(5);
        for _ in 0..5 {
            start.push(token(&mut rest)?);
        }
        let command = rest.trim().to_string();
        Some(ProcessInfo {
            pid,
            ppid,
            pgid,
            start: start.join(" "),
            marker: marker_in(&command),
            command,
        })
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::fs;

    use super::{MARKER_VAR, OwnershipError, ProcessInfo};

    pub(super) async fn list() -> Result<Vec<ProcessInfo>, OwnershipError> {
        tokio::task::spawn_blocking(scan)
            .await
            .map_err(|e| OwnershipError::Ps(e.to_string()))?
    }

    pub(super) async fn start(pid: u32) -> Option<String> {
        tokio::task::spawn_blocking(move || stat(pid).map(|(_, _, start)| start))
            .await
            .ok()
            .flatten()
    }

    pub(super) async fn command(pid: u32) -> Option<String> {
        tokio::task::spawn_blocking(move || {
            let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
            let command = command_text(&cmdline);
            (!command.is_empty()).then_some(command)
        })
        .await
        .ok()
        .flatten()
    }

    fn scan() -> Result<Vec<ProcessInfo>, OwnershipError> {
        let mut found = Vec::new();
        for entry in fs::read_dir("/proc")? {
            let Ok(entry) = entry else { continue };
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            // A process that vanished or hides itself mid-scan is skipped.
            if let Some(info) = read_process(pid) {
                found.push(info);
            }
        }
        Ok(found)
    }

    fn read_process(pid: u32) -> Option<ProcessInfo> {
        let (ppid, pgid, start) = stat(pid)?;
        let marker = fs::read(format!("/proc/{pid}/environ"))
            .ok()
            .and_then(|environ| marker_in(&environ));
        let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        Some(ProcessInfo {
            pid,
            ppid,
            pgid,
            start,
            marker,
            command: command_text(&cmdline),
        })
    }

    /// The parent, group and start time (clock ticks since boot, as decimal text) from
    /// `/proc/<pid>/stat`.
    fn stat(pid: u32) -> Option<(u32, u32, String)> {
        parse_stat(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
    }

    /// Fields after the last `)` (the command name may hold anything): the state is field 3, so
    /// the parent, field 4, is the second, the group, field 5, the third and the start time,
    /// field 22, the twentieth.
    pub(super) fn parse_stat(stat: &str) -> Option<(u32, u32, String)> {
        let fields: Vec<&str> = stat[stat.rfind(')')? + 1..].split_whitespace().collect();
        Some((
            fields.get(1)?.parse().ok()?,
            fields.get(2)?.parse().ok()?,
            (*fields.get(19)?).to_string(),
        ))
    }

    /// The value of `OCTET_FLEET_ATTEMPT=` in a NUL-separated environment.
    pub(super) fn marker_in(environ: &[u8]) -> Option<String> {
        let prefix = format!("{MARKER_VAR}=");
        environ
            .split(|byte| *byte == 0)
            .find_map(|entry| entry.strip_prefix(prefix.as_bytes()))
            .map(|value| String::from_utf8_lossy(value).into_owned())
    }

    /// A NUL-separated command line as text.
    pub(super) fn command_text(cmdline: &[u8]) -> String {
        let text = String::from_utf8_lossy(cmdline);
        text.trim_end_matches('\0').replace('\0', " ")
    }
}

/// Which live processes are the attempt's: `Verified` when any carries `attempt_marker`, matches
/// a `registered` identity, or is a descendant of one that does; else `Unverified` when any is in
/// one of the registered `groups`; else `Clear`. A process whose parent is verified (and alive:
/// it is in the list) was forked by it, and an orphan's parent is init, never a reused PID, so
/// ancestry cannot adopt a stranger. Octet's own process and the `released` PIDs are dropped
/// first, so neither is ever counted nor confers verification on its children. The PIDs are
/// sorted.
pub fn check(
    processes: &[ProcessInfo],
    attempt_marker: &str,
    registered: &[Identity],
    groups: &[u32],
    released: &[u32],
) -> ProcessCheck {
    let own = std::process::id();
    let live: Vec<&ProcessInfo> = processes
        .iter()
        .filter(|process| process.pid != own && !released.contains(&process.pid))
        .collect();
    let mut verified: BTreeSet<u32> = live
        .iter()
        .filter(|process| {
            process.marker.as_deref() == Some(attempt_marker)
                || registered
                    .iter()
                    .any(|identity| identity.pid == process.pid && identity.start == process.start)
        })
        .map(|process| process.pid)
        .collect();
    // Descendants to a fixed point: each pass adopts the children of what is verified so far.
    loop {
        let before = verified.len();
        verified.extend(
            live.iter()
                .filter(|process| verified.contains(&process.ppid))
                .map(|process| process.pid)
                .collect::<Vec<_>>(),
        );
        if verified.len() == before {
            break;
        }
    }
    let unverified: BTreeSet<u32> = live
        .iter()
        .filter(|process| !verified.contains(&process.pid) && groups.contains(&process.pgid))
        .map(|process| process.pid)
        .collect();
    if !verified.is_empty() {
        ProcessCheck::Verified(verified.into_iter().collect())
    } else if !unverified.is_empty() {
        ProcessCheck::Unverified(unverified.into_iter().collect())
    } else {
        ProcessCheck::Clear
    }
}

/// Stops `pids`, which the caller verified as the run's: SIGTERM, then SIGKILL for those still
/// alive after five seconds. Returns the PIDs still alive after that (a process killed by SIGKILL that
/// is a zombie awaiting its parent counts as alive); the caller re-lists and checks again. A PID
/// that exited and was reused between the caller's listing and this call would be signalled, a
/// window the caller keeps short by checking just before.
pub async fn kill_verified(pids: &[u32]) -> Vec<u32> {
    for pid in pids {
        octet_proc::signal::terminate(*pid);
    }
    wait_gone(pids, TERM_GRACE).await;
    for pid in pids {
        if octet_proc::signal::alive(*pid) {
            octet_proc::signal::kill(*pid);
        }
    }
    wait_gone(pids, KILL_GRACE).await
}

/// Waits up to `limit` for all of `pids` to be gone; those that are not.
async fn wait_gone(pids: &[u32], limit: Duration) -> Vec<u32> {
    let deadline = Instant::now() + limit;
    loop {
        let alive: Vec<u32> = pids
            .iter()
            .copied()
            .filter(|pid| octet_proc::signal::alive(*pid))
            .collect();
        if alive.is_empty() || Instant::now() >= deadline {
            return alive;
        }
        tokio::time::sleep(LOOK).await;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use octet_testkit::TempDir;

    use super::*;
    use crate::store::RunDir;

    /// What a helper process does; see [`helper_process`].
    const HELPER_VAR: &str = "OCTET_OWNERSHIP_HELPER";

    /// The test binary run again as a helper. Not `sh` and `sleep`: on macOS `ps` does not show
    /// the environment of Apple's own binaries, only of the likes of this one.
    fn helper(mode: &str, marker: Option<&str>) -> Command {
        let mut cmd = Command::new(std::env::current_exe().expect("the test binary"));
        cmd.args(["--exact", "ownership::tests::helper_process", "--nocapture"])
            .env(HELPER_VAR, mode)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match marker {
            Some(marker) => cmd.env(MARKER_VAR, marker),
            None => cmd.env_remove(MARKER_VAR),
        };
        cmd
    }

    /// Not a test: the body of the processes the tests start. `sleep` waits; `parent` starts an
    /// unmarked `sleep` helper in its own process group and waits for it.
    #[test]
    fn helper_process() {
        match std::env::var(HELPER_VAR).as_deref() {
            Ok("sleep") => std::thread::sleep(Duration::from_secs(30)),
            Ok("parent") => {
                let _ = helper("sleep", None)
                    .spawn()
                    .and_then(|mut child| child.wait());
            }
            _ => {}
        }
    }

    /// Starts helpers, marked with a marker or with none, and kills and reaps whatever it
    /// started when dropped, even when a test fails.
    #[derive(Default)]
    struct Kids {
        children: Vec<Child>,
        pids: Vec<u32>,
    }

    impl Kids {
        /// Starts a helper in `mode`; `own_group` makes it lead a new process group.
        fn spawn(&mut self, mode: &str, marker: Option<&str>, own_group: bool) -> u32 {
            let mut cmd = helper(mode, marker);
            if own_group {
                std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
            }
            self.spawn_command(&mut cmd)
        }

        /// Starts `cmd`, to be killed and reaped with the others.
        fn spawn_command(&mut self, cmd: &mut Command) -> u32 {
            let child = cmd.spawn().expect("the process starts");
            let pid = child.id();
            self.children.push(child);
            self.pids.push(pid);
            pid
        }
    }

    impl Drop for Kids {
        fn drop(&mut self) {
            for pid in &self.pids {
                octet_proc::signal::kill(*pid);
            }
            for child in &mut self.children {
                let _ = child.wait();
            }
        }
    }

    /// Lists processes until `ready` accepts the list: a child is visible only once it exists.
    async fn list_until(ready: impl Fn(&[ProcessInfo]) -> bool) -> Vec<ProcessInfo> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let list = list_processes().await.expect("the process list");
            if ready(&list) {
                return list;
            }
            assert!(
                Instant::now() < deadline,
                "the expected processes never appeared"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn has_marker(list: &[ProcessInfo], pid: u32, marker: &str) -> bool {
        list.iter()
            .any(|p| p.pid == pid && p.marker.as_deref() == Some(marker))
    }

    /// A run directory with a `run.json`-less root: the lock needs only the directory.
    fn run_dir(data: &Path, id: &str) -> RunDir {
        let root = data.join("fleet").join(id);
        std::fs::create_dir_all(&root).expect("run dir");
        RunDir { root }
    }

    /// Locks once the lock is free. A lock is released when every copy of its file is closed,
    /// and a child another test is forking holds a copy until it execs (std's files close on
    /// exec), so a lock just dropped can stay busy for a moment.
    async fn lock_when_free(data: &Path, run: &RunDir, hash: &str) -> Held {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match lock(data, run, hash).await {
                Ok(held) => return held,
                Err(OwnershipError::RunBusy | OwnershipError::WorktreeBusy { .. })
                    if Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => panic!("the lock never came free: {error:?}"),
            }
        }
    }

    fn unresolved(attempt: u32) -> Unresolved {
        Unresolved {
            attempt: AttemptId(attempt),
            identities: vec![Identity {
                pid: 7,
                start: "s".into(),
            }],
        }
    }

    #[tokio::test]
    async fn a_second_driver_is_refused() {
        let data = TempDir::new("ownership-driver");
        let run = run_dir(data.path(), "r1");
        let log = run.root.join("log.jsonl");
        std::fs::write(&log, b"{\"kind\":\"x\"}\n").expect("log");
        let before = std::fs::metadata(&log)
            .expect("meta")
            .modified()
            .expect("mtime");
        let held = lock(data.path(), &run, "aaaa").await.expect("first lock");
        let second = lock(data.path(), &run, "bbbb").await;
        assert!(matches!(second, Err(OwnershipError::RunBusy)), "{second:?}");
        let meta = std::fs::metadata(&log).expect("meta");
        assert_eq!(std::fs::read(&log).expect("log"), b"{\"kind\":\"x\"}\n");
        assert_eq!(meta.modified().expect("mtime"), before);
        drop(held);
        lock_when_free(data.path(), &run, "aaaa").await;
    }

    #[tokio::test]
    async fn the_run_lock_names_its_driver_while_held() {
        let data = TempDir::new("ownership-holder");
        let run = run_dir(data.path(), "r1");
        assert_eq!(run_holder(&run).await, None, "no lock file yet");
        let held = lock(data.path(), &run, "aaaa").await.expect("lock");
        assert_eq!(run_holder(&run).await, Some(std::process::id()));
        drop(held);
        assert_eq!(run_holder(&run).await, None, "released");
        // A holder that is gone, or a PID now someone else's, is no driver.
        std::fs::write(run.root.join("lock"), "1 Thu Jan  1 00:00:00 1970").expect("write");
        assert_eq!(run_holder(&run).await, None);
    }

    #[tokio::test]
    async fn the_locks_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let data = TempDir::new("ownership-modes");
        let run = run_dir(data.path(), "r1");
        let _held = lock(data.path(), &run, "aaaa").await.expect("lock");
        for path in [
            run.root.join("lock"),
            data.path().join("fleet/worktrees/aaaa.lock"),
        ] {
            let mode = std::fs::metadata(&path)
                .expect("lock file")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{}", path.display());
        }
    }

    #[tokio::test]
    async fn a_second_run_on_the_worktree_is_refused() {
        let data = TempDir::new("ownership-worktree");
        let first = run_dir(data.path(), "r1");
        let second = run_dir(data.path(), "r2");
        let held = lock(data.path(), &first, "aaaa").await.expect("first");
        let refused = lock(data.path(), &second, "aaaa").await;
        match refused {
            Err(OwnershipError::WorktreeBusy { holder }) => {
                assert_eq!(holder.as_deref(), Some("r1"));
            }
            other => panic!("expected WorktreeBusy, got {other:?}"),
        }
        // The refused run's own lock was released again, and another work tree is free.
        drop(lock_when_free(data.path(), &second, "bbbb").await);
        drop(held);
        lock_when_free(data.path(), &second, "aaaa").await;
    }

    #[tokio::test]
    async fn a_holder_that_wrote_nothing_is_unnamed() {
        let data = TempDir::new("ownership-unnamed");
        let first = run_dir(data.path(), "r1");
        let second = run_dir(data.path(), "r2");
        let held = lock(data.path(), &first, "aaaa").await.expect("first");
        std::fs::write(data.path().join("fleet/worktrees/aaaa.lock"), b"").expect("empty it");
        let refused = lock(data.path(), &second, "aaaa").await;
        assert!(
            matches!(refused, Err(OwnershipError::WorktreeBusy { holder: None })),
            "{refused:?}"
        );
        drop(held);
    }

    #[tokio::test]
    async fn the_record_round_trips_and_a_missing_one_is_empty() {
        let data = TempDir::new("ownership-record");
        let run = run_dir(data.path(), "r1");
        let held = lock(data.path(), &run, "aaaa").await.expect("lock");
        assert_eq!(
            load(data.path(), "aaaa").await.expect("missing"),
            WorktreeRecord::default()
        );
        let record = WorktreeRecord {
            runs: BTreeMap::from([("r1".to_string(), unresolved(3))]),
        };
        save(&held, data.path(), "aaaa", &record)
            .await
            .expect("save");
        assert_eq!(load(data.path(), "aaaa").await.expect("load"), record);
        assert!(!data.path().join("fleet/worktrees/aaaa.json.tmp").exists());
        save(&held, data.path(), "aaaa", &WorktreeRecord::default())
            .await
            .expect("save again");
        assert_eq!(
            load(data.path(), "aaaa").await.expect("load"),
            WorktreeRecord::default()
        );
    }

    #[tokio::test]
    async fn a_record_is_saved_only_under_its_own_worktree_lock() {
        let data = TempDir::new("ownership-wrong");
        let run = run_dir(data.path(), "r1");
        let held = lock(data.path(), &run, "aaaa").await.expect("lock");
        let saved = save(&held, data.path(), "bbbb", &WorktreeRecord::default()).await;
        match saved {
            Err(OwnershipError::WrongWorktree { held, asked }) => {
                assert_eq!((held.as_str(), asked.as_str()), ("aaaa", "bbbb"));
            }
            other => panic!("expected WrongWorktree, got {other:?}"),
        }
        assert!(!data.path().join("fleet/worktrees/bbbb.json").exists());
        assert!(!data.path().join("fleet/worktrees/bbbb.json.tmp").exists());
    }

    #[tokio::test]
    async fn an_unparsable_record_is_an_error_not_an_empty_one() {
        let data = TempDir::new("ownership-corrupt");
        let dir = data.path().join("fleet/worktrees");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("aaaa.json"), b"{not json").expect("write");
        let loaded = load(data.path(), "aaaa").await;
        assert!(matches!(loaded, Err(OwnershipError::Json(_))), "{loaded:?}");
    }

    #[tokio::test]
    async fn the_worktree_hash_names_the_canonical_path() {
        let data = TempDir::new("ownership-hash");
        let tree = data.path().join("tree");
        std::fs::create_dir_all(tree.join("sub")).expect("dirs");
        let plain = worktree_hash(&tree).await.expect("hash");
        assert_eq!(plain.len(), 40, "{plain}");
        let roundabout = tree.join("sub").join("..");
        assert_eq!(worktree_hash(&roundabout).await.expect("hash"), plain);
        let other = worktree_hash(&tree.join("sub")).await.expect("hash");
        assert_ne!(other, plain);
        let missing = worktree_hash(&tree.join("absent")).await;
        assert!(matches!(missing, Err(OwnershipError::Io(_))), "{missing:?}");
    }

    #[test]
    fn admit_refuses_while_another_run_is_listed() {
        let mut record = WorktreeRecord::default();
        assert!(admit(&record, "r2").is_ok(), "an empty record admits");
        record.runs.insert("r2".into(), unresolved(0));
        assert!(
            admit(&record, "r2").is_ok(),
            "the run itself does not block itself"
        );
        record.runs.insert("r9".into(), unresolved(1));
        record.runs.insert("r5".into(), unresolved(1));
        match admit(&record, "r2") {
            Err(OwnershipError::OtherRunUnresolved { run }) => assert_eq!(run, "r5"),
            other => panic!("expected OtherRunUnresolved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_marked_child_is_verified() {
        let mut kids = Kids::default();
        let marker = "run-marked/0";
        let pid = kids.spawn("sleep", Some(marker), false);
        let list = list_until(|l| has_marker(l, pid, marker)).await;
        assert_eq!(
            check(&list, marker, &[], &[], &[]),
            ProcessCheck::Verified(vec![pid])
        );
        // Another attempt's marker does not claim it, and it is in no registered group.
        assert_eq!(
            check(&list, "run-marked/1", &[], &[], &[]),
            ProcessCheck::Clear
        );
    }

    #[tokio::test]
    async fn a_matching_identity_is_verified() {
        let mut kids = Kids::default();
        let pid = kids.spawn("sleep", None, false);
        let list = list_until(|l| l.iter().any(|p| p.pid == pid)).await;
        let start = start_time(pid).await.expect("start time");
        let identity = Identity { pid, start };
        let marker = "run-identity/0";
        assert_eq!(
            check(&list, marker, std::slice::from_ref(&identity), &[], &[]),
            ProcessCheck::Verified(vec![pid])
        );
        // The same PID with another start time is a reused PID, not the run's.
        let reused = Identity {
            pid,
            start: format!("{}x", identity.start),
        };
        assert_eq!(
            check(&list, marker, &[reused], &[], &[]),
            ProcessCheck::Clear
        );
    }

    #[tokio::test]
    async fn an_unmarked_group_member_is_unverified() {
        let mut kids = Kids::default();
        let marker = "run-group/0";
        // Neither the leader nor the sleeper it starts carries a marker, and the leader is not
        // registered, so nothing is verified, and the members are known only by their group.
        let leader = kids.spawn("parent", None, true);
        let group = leader;
        let list = list_until(|l| {
            l.iter()
                .any(|p| p.pgid == group && p.pid != leader && p.ppid == leader)
        })
        .await;
        let member = list
            .iter()
            .find(|p| p.pgid == group && p.pid != leader && p.ppid == leader)
            .map(|p| p.pid)
            .expect("the member");
        kids.pids.push(member);
        let mut both = vec![leader, member];
        both.sort_unstable();
        assert_eq!(
            check(&list, marker, &[], &[group], &[]),
            ProcessCheck::Unverified(both.clone())
        );
        // Without the registered group they are nobody's business.
        assert_eq!(check(&list, marker, &[], &[], &[]), ProcessCheck::Clear);
        // Released, the leader neither counts nor makes its child the run's.
        assert_eq!(
            check(&list, marker, &[], &[group], &[leader]),
            ProcessCheck::Unverified(vec![member])
        );
        // Registered by identity, the leader is verified and so is its child, by ancestry.
        let start = start_time(leader).await.expect("start time");
        let identity = Identity { pid: leader, start };
        assert_eq!(
            check(
                &list,
                marker,
                std::slice::from_ref(&identity),
                &[group],
                &[]
            ),
            ProcessCheck::Verified(both)
        );
        // Released as well as registered: released wins, and the child is unverified again.
        assert_eq!(
            check(&list, marker, &[identity], &[group], &[leader]),
            ProcessCheck::Unverified(vec![member])
        );
    }

    #[tokio::test]
    async fn a_descendant_with_no_visible_marker_is_verified_by_ancestry() {
        let mut kids = Kids::default();
        let marker = "run-ancestry/0";
        // Real system binaries: on macOS their environment is not shown, so only the shell's
        // registered identity and the sleep's parentage can make them the run's.
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30 & wait"])
            .env_remove(MARKER_VAR)
            .stdin(Stdio::null());
        let shell = kids.spawn_command(&mut cmd);
        let list = list_until(|l| l.iter().any(|p| p.ppid == shell)).await;
        let sleeper = list.iter().find(|p| p.ppid == shell).expect("the sleeper");
        kids.pids.push(sleeper.pid);
        assert_eq!(sleeper.marker, None);
        assert_eq!(check(&list, marker, &[], &[], &[]), ProcessCheck::Clear);
        let start = start_time(shell).await.expect("start time");
        let identity = Identity { pid: shell, start };
        let mut expected = vec![shell, sleeper.pid];
        expected.sort_unstable();
        assert_eq!(
            check(&list, marker, &[identity], &[], &[]),
            ProcessCheck::Verified(expected)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_ps_line_is_parsed_into_its_fields() {
        let line = "  812   799   799 Mon Nov  3 10:11:12 2026     sleep 30 HOME=/h OCTET_FLEET_ATTEMPT=r/1 X=y";
        let info = platform::parse_line(line).expect("a line");
        assert_eq!((info.pid, info.ppid, info.pgid), (812, 799, 799));
        assert_eq!(info.start, "Mon Nov 3 10:11:12 2026");
        assert_eq!(info.marker.as_deref(), Some("r/1"));
        assert!(
            info.command.starts_with("sleep 30 HOME=/h"),
            "{}",
            info.command
        );
        assert!(platform::parse_line("not a line").is_none());
        // An argument that spells a marker comes before the environment's.
        let line = "  813   799   799 Mon Nov  3 10:11:12 2026     sh -c OCTET_FLEET_ATTEMPT=x/9 \
                    HOME=/h OCTET_FLEET_ATTEMPT=r/1";
        let info = platform::parse_line(line).expect("a line");
        assert_eq!(info.marker.as_deref(), Some("r/1"));
        assert_eq!(platform::marker_in("a b"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_proc_stat_is_parsed_after_the_last_paren() {
        let stat =
            "812 (a) b) c) S 799 798 797 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 123456 1000 50";
        // ppid 799, pgrp 798, starttime is field 22 (counting the pid as 1).
        let (ppid, pgid, start) = platform::parse_stat(stat).expect("a stat");
        assert_eq!((ppid, pgid, start.as_str()), (799, 798, "123456"));
        assert!(platform::parse_stat("812 no parens").is_none());
    }

    fn info(pid: u32, ppid: u32, pgid: u32, marker: Option<&str>) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid,
            pgid,
            start: format!("start-{pid}"),
            marker: marker.map(str::to_string),
            command: format!("cmd-{pid}"),
        }
    }

    /// Made-up PIDs above every system's largest (Linux's `PID_MAX_LIMIT`
    /// is 2^22, macOS's 99 999), so none is this test's own PID, which
    /// `check` skips (in a container the test can run as PID 10 or 20).
    const P: u32 = 4_200_000;

    #[test]
    fn ancestry_reaches_every_generation_whatever_the_list_order() {
        // 40 is listed before its parent 30, which is listed before its parent 20.
        let list = [
            info(P + 40, P + 30, 7, None),
            info(P + 30, P + 20, 7, None),
            info(P + 20, P + 10, 7, None),
            info(P + 10, 1, 7, Some("m")),
            info(P + 50, 1, 7, None),
            info(P + 60, P + 99, 8, None),
        ];
        assert_eq!(
            check(&list, "m", &[], &[], &[]),
            ProcessCheck::Verified(vec![P + 10, P + 20, P + 30, P + 40])
        );
        // The unrelated process in the registered group is the only unverified one, and only
        // when nothing is verified.
        assert_eq!(
            check(&list[4..], "m", &[], &[7], &[]),
            ProcessCheck::Unverified(vec![P + 50])
        );
    }

    #[test]
    fn a_released_parent_confers_nothing_on_its_children() {
        let list = [
            info(P + 10, 1, 7, Some("m")),
            info(P + 20, P + 10, 7, None),
            info(P + 30, P + 20, 7, None),
        ];
        // Releasing the middle one cuts its subtree off, not the rest.
        assert_eq!(
            check(&list, "m", &[], &[], &[P + 20]),
            ProcessCheck::Verified(vec![P + 10])
        );
        // Releasing the root leaves no verified process, and the children are not the run's.
        assert_eq!(check(&list, "m", &[], &[], &[P + 10]), ProcessCheck::Clear);
        assert_eq!(
            check(&list, "m", &[], &[7], &[P + 10]),
            ProcessCheck::Unverified(vec![P + 20, P + 30])
        );
    }

    #[test]
    fn octets_own_children_are_not_the_runs() {
        let own = std::process::id();
        // Something Octet itself started (the `ps` of a listing) is the child of a pid that is
        // never verified, even one carrying the marker.
        let list = [info(own, 1, 7, Some("m")), info(P + 900, own, 7, None)];
        assert_eq!(check(&list, "m", &[], &[], &[]), ProcessCheck::Clear);
        assert_eq!(
            check(&list, "m", &[], &[7], &[]),
            ProcessCheck::Unverified(vec![P + 900])
        );
    }

    #[tokio::test]
    async fn released_pids_are_ignored() {
        let mut kids = Kids::default();
        let marker = "run-released/0";
        let pid = kids.spawn("sleep", Some(marker), false);
        let list = list_until(|l| has_marker(l, pid, marker)).await;
        assert_eq!(check(&list, marker, &[], &[], &[pid]), ProcessCheck::Clear);
        assert_eq!(
            check(&list, marker, &[], &[], &[pid + 1]),
            ProcessCheck::Verified(vec![pid])
        );
    }

    #[test]
    fn octet_itself_is_never_listed_as_the_runs() {
        let own = ProcessInfo {
            pid: std::process::id(),
            ppid: 1,
            pgid: 5,
            start: "s".into(),
            marker: Some("m".into()),
            command: "octet".into(),
        };
        assert_eq!(check(&[own], "m", &[], &[5], &[]), ProcessCheck::Clear);
    }

    #[tokio::test]
    async fn kill_verified_kills_only_those() {
        let mut kids = Kids::default();
        let marker = "run-kill/0";
        let marked = kids.spawn("sleep", Some(marker), false);
        let bystander = kids.spawn("sleep", None, false);
        let list =
            list_until(|l| has_marker(l, marked, marker) && l.iter().any(|p| p.pid == bystander))
                .await;
        let ProcessCheck::Verified(pids) = check(&list, marker, &[], &[], &[]) else {
            panic!("the marked child is verified");
        };
        assert_eq!(pids, vec![marked]);
        // A dead child stays a zombie, which still counts as alive, until its parent reaps it.
        let reaper = kids.children.remove(0);
        let reaped = tokio::task::spawn_blocking(move || {
            let mut reaper = reaper;
            reaper.wait().expect("reap")
        });
        let survivors = kill_verified(&pids).await;
        assert!(survivors.is_empty(), "{survivors:?}");
        assert!(!reaped.await.expect("join").success());
        assert!(
            octet_proc::signal::alive(bystander),
            "the bystander was not signalled"
        );
        // The reaped child is out of `kids`, whose drop reaps only the bystander.
        kids.pids.retain(|pid| *pid != marked);
    }

    #[tokio::test]
    async fn a_command_line_comes_without_the_environment() {
        let mut kids = Kids::default();
        let pid = kids.spawn("sleep", Some("run/0"), false);
        list_until(|l| l.iter().any(|p| p.pid == pid)).await;
        let command = command_of(pid).await.expect("its command line");
        assert!(command.contains("helper_process"), "{command}");
        assert!(!command.contains(MARKER_VAR), "{command}");
        assert_eq!(command_of(u32::MAX).await, None);
    }

    #[tokio::test]
    async fn start_time_agrees_with_the_listing() {
        let mut kids = Kids::default();
        let pid = kids.spawn("sleep", None, false);
        let list = list_until(|l| l.iter().any(|p| p.pid == pid)).await;
        let listed = list.iter().find(|p| p.pid == pid).expect("listed");
        assert_eq!(
            start_time(pid).await.as_deref(),
            Some(listed.start.as_str())
        );
        assert!(!listed.start.is_empty());
        assert!(
            listed.command.contains("helper_process"),
            "{}",
            listed.command
        );
        assert_eq!(start_time(u32::MAX).await, None);
    }
}
