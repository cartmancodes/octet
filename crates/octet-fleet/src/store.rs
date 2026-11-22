//! The run directory: `run.json` written once, the durable append-only
//! `log.jsonl`, and artifact files.
//!
//! The log is the authority on accepted work: [`Log::append`] syncs each record
//! before it returns, so the runner never acts on a record that could be lost.
//! A crash can leave a torn last line; [`Log::open`] cuts it back to the last
//! newline before anything is appended. An artifact's files are synced by
//! [`write_artifact`] before any record names them.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use octet_store::{create_private, create_private_dir, sync_directory};
use serde::{Deserialize, Serialize};
use tokio::fs::{DirBuilder, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::record::{ArtifactId, ArtifactMeta, AttemptId, CandidateId, Record};
use crate::snapshot::{SnapshotError, command, exec};
use crate::validate::Resolved;

/// Why the run store failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A file or directory could not be read, written or synced.
    #[error("run store I/O: {0}")]
    Io(#[from] io::Error),
    /// `run.json` or an artifact's metadata could not be serialized or parsed.
    #[error("run store JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// A complete line of `log.jsonl` is not a record. The log is never
    /// repaired past a torn tail, so this needs a person.
    #[error("log.jsonl line {line} is corrupt: {source}")]
    Corrupt {
        /// The line, counting from 1.
        line: usize,
        /// Why it did not parse.
        source: serde_json::Error,
    },
    /// There is no run with this ID.
    #[error("no such run: {0}")]
    NoSuchRun(String),
    /// A run with this ID already exists.
    #[error("run {0} already exists")]
    RunExists(String),
    /// An append failed and the log could not be restored to its last whole
    /// record, so nothing more is written to it.
    #[error("log.jsonl is poisoned: an append failed and could not be rolled back")]
    Poisoned,
    /// Git could not hash the workflow.
    #[error(transparent)]
    Git(#[from] SnapshotError),
}

/// What a run was started with: `run.json`, written once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunFile {
    /// The resolved workflow, inlined.
    pub workflow: Resolved,
    /// The git hash of the workflow's serialized form.
    pub hash: String,
    /// The task the user gave.
    pub task: String,
    /// The work tree's top level.
    pub top: PathBuf,
    /// The directory the run started in, relative to `top`.
    pub cwd_rel: PathBuf,
    /// The base candidate.
    pub base: CandidateId,
    /// The base candidate's tree object.
    pub base_tree: String,
    /// The version of Octet that started the run.
    pub octet_version: String,
}

/// A run's directory, `<data_dir>/fleet/<id>/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunDir {
    /// The directory.
    pub root: PathBuf,
}

/// The run ID for second `secs` and tie-breaker `tag`. The seconds are
/// zero-padded to ten digits (good until 2286) so string order is time order.
fn run_id_at(secs: u64, tag: u16) -> String {
    format!("f{secs:010}-{tag:04x}")
}

/// A new run ID, `f<unix seconds>-<4 hex>`: sortable by time and typeable.
#[must_use]
pub fn new_run_id() -> String {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u32(now.subsec_nanos());
    hasher.write_u32(std::process::id());
    hasher.write_u64(COUNT.fetch_add(1, Ordering::Relaxed));
    // Four typeable hex digits: the low 16 bits of the hash.
    let tag = u16::try_from(hasher.finish() & 0xffff).unwrap_or_default();
    run_id_at(now.as_secs(), tag)
}

/// The directory of run `id`, or `None` when `id` could name something else.
fn run_root(data_dir: &Path, id: &str) -> Option<PathBuf> {
    let plain = !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    plain.then(|| data_dir.join("fleet").join(id))
}

/// Creates the run directory and writes `run.json`, durably.
///
/// # Errors
///
/// [`StoreError::RunExists`] when the ID is taken; [`StoreError::Io`] when
/// the ID is not a plain name or a file cannot be created or synced.
pub async fn create(data_dir: &Path, id: &str, run: &RunFile) -> Result<RunDir, StoreError> {
    let root = run_root(data_dir, id).ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("{id:?} is not a plain run ID"),
        )
    })?;
    let fleet = data_dir.join("fleet");
    create_private_dir(&fleet).await?;
    // Not recursive: an existing run must make this fail.
    match DirBuilder::new().mode(0o700).create(&root).await {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            return Err(StoreError::RunExists(id.to_string()));
        }
        Err(e) => return Err(e.into()),
    }
    create_private_dir(&root.join("artifacts")).await?;
    create_private_dir(&root.join("attempts")).await?;
    let mut file = create_private(&root.join("run.json")).await?;
    file.write_all(&serde_json::to_vec_pretty(run)?).await?;
    file.sync_all().await?;
    sync_directory(&root).await?;
    sync_directory(&fleet).await?;
    Ok(RunDir { root })
}

/// Opens run `id` and reads its `run.json`.
///
/// # Errors
///
/// [`StoreError::NoSuchRun`] when there is no such run; otherwise
/// [`StoreError::Io`] or [`StoreError::Json`].
pub async fn open(data_dir: &Path, id: &str) -> Result<(RunDir, RunFile), StoreError> {
    let no_such = || StoreError::NoSuchRun(id.to_string());
    let root = run_root(data_dir, id).ok_or_else(no_such)?;
    let bytes = match tokio::fs::read(root.join("run.json")).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Err(no_such()),
        Err(e) => return Err(e.into()),
    };
    Ok((RunDir { root }, serde_json::from_slice(&bytes)?))
}

/// How a line reaches the log; swapped in tests to fail a write partway.
type Writer = fn(&std::fs::File, &[u8]) -> io::Result<()>;

/// Writes `line` at the end of the (append-mode) log and syncs it.
fn write_synced(file: &std::fs::File, line: &[u8]) -> io::Result<()> {
    let mut file = file;
    file.write_all(line)?;
    file.sync_data()
}

/// Appends `line` after the first `len` bytes. When the write or sync fails
/// the file is cut back to `len`; the flag says whether even that failed.
fn append_line(
    file: &std::fs::File,
    len: u64,
    line: &[u8],
    write: Writer,
) -> Result<(), (io::Error, bool)> {
    write(file, line).map_err(|error| {
        let rolled_back = file.set_len(len).and_then(|()| file.sync_all());
        (error, rolled_back.is_err())
    })
}

/// The open `log.jsonl`, for appending.
#[derive(Debug)]
pub struct Log {
    file: Arc<std::fs::File>,
    /// The length of the log's whole records: where the next one goes.
    len: u64,
    /// Set when a failed append could not be undone.
    poisoned: bool,
    write: Writer,
}

impl Log {
    /// Opens (creating, owner-only, if absent) the run's log and reads it. A
    /// torn last line, one with no newline after it, is cut off and the cut
    /// synced before anything is parsed or appended.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] when a complete line is not a [`Record`];
    /// [`StoreError::Io`] on a read, truncate or sync failure.
    pub async fn open(dir: &RunDir) -> Result<(Self, Vec<Record>), StoreError> {
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .open(dir.root.join("log.jsonl"))
            .await?;
        // A newly created log must survive a crash too.
        sync_directory(&dir.root).await?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).await?;
        let whole = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        if whole != bytes.len() {
            file.set_len(whole as u64).await?;
            file.sync_all().await?;
            bytes.truncate(whole);
        }
        let mut records = Vec::new();
        for (index, line) in bytes.split_inclusive(|&b| b == b'\n').enumerate() {
            let record = serde_json::from_slice(line).map_err(|source| StoreError::Corrupt {
                line: index + 1,
                source,
            })?;
            records.push(record);
        }
        let log = Self {
            file: Arc::new(file.into_std().await),
            len: whole as u64,
            poisoned: false,
            write: write_synced,
        };
        Ok((log, records))
    }

    /// Appends `record` as one line and syncs it before returning. A failed
    /// append is rolled back (the log is cut to its last whole record), so the
    /// record is simply not in the log and the log stays usable.
    ///
    /// # Errors
    ///
    /// [`StoreError::Json`] if it cannot be serialized; [`StoreError::Io`] if
    /// the write or sync fails; [`StoreError::Poisoned`] if an earlier failure
    /// could not be rolled back (this one included: it returns the I/O error,
    /// and every later call returns `Poisoned` without writing).
    pub async fn append(&mut self, record: &Record) -> Result<(), StoreError> {
        if self.poisoned {
            return Err(StoreError::Poisoned);
        }
        // Compact JSON has no raw newline: they are escaped inside strings.
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        let (file, len, write) = (Arc::clone(&self.file), self.len, self.write);
        let size = line.len() as u64;
        let done = tokio::task::spawn_blocking(move || append_line(&file, len, &line, write)).await;
        match done {
            Ok(Ok(())) => {
                self.len += size;
                Ok(())
            }
            Ok(Err((error, unrecoverable))) => {
                self.poisoned = unrecoverable;
                Err(error.into())
            }
            Err(joined) => {
                // The blocking task died; what it left in the file is unknown.
                self.poisoned = true;
                Err(io::Error::other(joined).into())
            }
        }
    }
}

/// The path of an artifact's file with extension `ext`.
fn artifact_path(dir: &RunDir, id: ArtifactId, ext: &str) -> PathBuf {
    dir.root.join("artifacts").join(format!("{}.{ext}", id.0))
}

/// Writes `bytes` to `path` through a `.tmp` sibling: created owner-only
/// (an orphan from a crash is truncated), synced, then renamed into place.
async fn put(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&tmp, path).await
}

/// Writes an artifact's body (`artifacts/<id>.<ext>`) and metadata
/// (`artifacts/<id>.json`), both synced, then the directory. Nothing may name
/// the artifact before this returns.
///
/// Each file is written to a `.tmp` sibling and renamed into place, body
/// first, so the metadata's rename is the commit point. A crash leaves at most
/// orphans, and calling this again for the same artifact replaces them.
/// Overwriting is safe because the runner writes an artifact only before any
/// `attempt_finished` record names it.
///
/// # Errors
///
/// [`StoreError::Io`] when `ext` is not a plain extension (`json` is the
/// metadata's) or a write, sync or rename fails.
pub async fn write_artifact(
    dir: &RunDir,
    meta: &ArtifactMeta,
    body: &[u8],
    ext: &str,
) -> Result<(), StoreError> {
    if ext.is_empty() || ext == "json" || !ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("{ext:?} is not an extension"),
        )
        .into());
    }
    let meta_json = serde_json::to_vec(meta)?;
    put(&artifact_path(dir, meta.id, ext), body).await?;
    put(&artifact_path(dir, meta.id, "json"), &meta_json).await?;
    sync_directory(&dir.root.join("artifacts")).await?;
    Ok(())
}

/// Reads an artifact's metadata back.
///
/// # Errors
///
/// [`StoreError::Io`] when it is absent; [`StoreError::Json`] when it does
/// not parse.
pub async fn read_artifact_meta(dir: &RunDir, id: ArtifactId) -> Result<ArtifactMeta, StoreError> {
    let bytes = tokio::fs::read(artifact_path(dir, id, "json")).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// The directory an attempt keeps its journal, scratch clone and patch in. No
/// I/O: the directory is made when the attempt starts.
#[must_use]
pub fn attempt_dir(dir: &RunDir, attempt: AttemptId) -> PathBuf {
    dir.root.join("attempts").join(attempt.0.to_string())
}

/// The git hash of `resolved_json`, as `git hash-object --stdin` prints it.
///
/// # Errors
///
/// [`StoreError::Git`] when git cannot be run in `repo_top` or fails.
pub async fn workflow_hash(repo_top: &Path, resolved_json: &[u8]) -> Result<String, StoreError> {
    let cmd = command(repo_top, ["hash-object", "--stdin"]);
    let out = exec(cmd, resolved_json).await?;
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use octet_testkit::TempDir;

    use super::*;
    use crate::record::{ArtifactId, ArtifactMeta, AttemptId, CandidateId, Origin, Record};

    /// The git variables a test's own git must not inherit.
    const INHERITED: [&str; 7] = [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_PREFIX",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ];

    fn run_file() -> RunFile {
        let workflow = crate::sim::shipped();
        RunFile {
            workflow,
            hash: "0123456789abcdef".into(),
            task: "add a flag".into(),
            top: PathBuf::from("/work/repo"),
            cwd_rel: PathBuf::from("sub"),
            base: CandidateId(0),
            base_tree: "4b825dc642cb6eb9a060e54bf8d69288fbee4904".into(),
            octet_version: "0.0.0-test".into(),
        }
    }

    fn started(n: u32) -> Record {
        Record::AttemptStarted {
            attempt: AttemptId(n),
            step: "plan".into(),
            round: 0,
            input_candidate: CandidateId(0),
            requires: vec![],
            history: vec![],
        }
    }

    fn candidate(n: u32) -> Record {
        Record::Candidate {
            id: CandidateId(n),
            tree: "4b825dc642cb6eb9a060e54bf8d69288fbee4904".into(),
            parent: Some(CandidateId(0)),
            origin: Origin::Change,
        }
    }

    fn line(record: &Record) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(record).expect("serializes");
        bytes.push(b'\n');
        bytes
    }

    async fn made(data: &Path, id: &str) -> RunDir {
        create(data, id, &run_file()).await.expect("create")
    }

    #[tokio::test]
    async fn a_torn_tail_is_cut_back() {
        let data = TempDir::new("octet-fleet-store");
        let dir = made(data.path(), "f1-0000").await;
        let path = dir.root.join("log.jsonl");
        let mut bytes = line(&started(0));
        bytes.extend(line(&candidate(1)));
        let whole = bytes.len();
        bytes.extend(&line(&started(1))[..20]);
        std::fs::write(&path, &bytes).expect("write log");

        let (mut log, records) = Log::open(&dir).await.expect("open");
        assert_eq!(records, vec![started(0), candidate(1)]);
        let on_disk = std::fs::read(&path).expect("read log");
        assert_eq!(on_disk.len(), whole);
        assert_eq!(on_disk.last(), Some(&b'\n'));

        // Appending after the repair does not glue onto the torn bytes.
        log.append(&started(1)).await.expect("append");
        let (_, records) = Log::open(&dir).await.expect("reopen");
        assert_eq!(records, vec![started(0), candidate(1), started(1)]);
    }

    #[tokio::test]
    async fn a_log_with_no_newline_is_emptied() {
        let data = TempDir::new("octet-fleet-store");
        let dir = made(data.path(), "f1-0000").await;
        let path = dir.root.join("log.jsonl");
        std::fs::write(&path, b"{\"type\":\"attempt_sta").expect("write log");
        let (_, records) = Log::open(&dir).await.expect("open");
        assert!(records.is_empty());
        assert_eq!(std::fs::metadata(&path).expect("meta").len(), 0);
    }

    #[tokio::test]
    async fn a_corrupt_complete_line_is_an_error() {
        let data = TempDir::new("octet-fleet-store");
        let dir = made(data.path(), "f1-0000").await;
        let mut bytes = line(&started(0));
        bytes.extend(b"not json\n");
        bytes.extend(line(&started(1)));
        std::fs::write(dir.root.join("log.jsonl"), &bytes).expect("write log");
        let err = Log::open(&dir).await.expect_err("corrupt");
        assert!(matches!(err, StoreError::Corrupt { line: 2, .. }), "{err}");
    }

    #[tokio::test]
    async fn append_then_reopen() {
        let data = TempDir::new("octet-fleet-store");
        let dir = made(data.path(), "f1-0000").await;
        let (mut log, records) = Log::open(&dir).await.expect("open");
        assert!(records.is_empty());
        log.append(&started(0)).await.expect("append");
        log.append(&candidate(1)).await.expect("append");
        drop(log);
        let (mut log, records) = Log::open(&dir).await.expect("reopen");
        assert_eq!(records, vec![started(0), candidate(1)]);
        log.append(&started(2)).await.expect("append");
        let text = std::fs::read_to_string(dir.root.join("log.jsonl")).expect("read");
        assert_eq!(text.lines().count(), 3);
        assert!(text.ends_with('\n'));
        let mode = std::fs::metadata(dir.root.join("log.jsonl"))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn run_json_is_private() {
        let data = TempDir::new("octet-fleet-store");
        let dir = made(data.path(), "f1-0000").await;
        let mode =
            |path: &Path| std::fs::metadata(path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&dir.root.join("run.json")), 0o600);
        for private in [
            &dir.root,
            &dir.root.join("artifacts"),
            &dir.root.join("attempts"),
        ] {
            assert_eq!(mode(private), 0o700, "{}", private.display());
        }
        assert_eq!(mode(&data.path().join("fleet")), 0o700);
    }

    #[tokio::test]
    async fn create_then_open_round_trips() {
        let data = TempDir::new("octet-fleet-store");
        let made = made(data.path(), "f1-0000").await;
        let (dir, run) = open(data.path(), "f1-0000").await.expect("open");
        assert_eq!(dir.root, made.root);
        assert_eq!(dir.root, data.path().join("fleet").join("f1-0000"));
        assert_eq!(run, run_file());
    }

    #[tokio::test]
    async fn an_existing_run_is_not_overwritten() {
        let data = TempDir::new("octet-fleet-store");
        made(data.path(), "f1-0000").await;
        let err = create(data.path(), "f1-0000", &run_file())
            .await
            .expect_err("exists");
        assert!(matches!(err, StoreError::RunExists(_)), "{err}");
    }

    #[tokio::test]
    async fn a_missing_run_is_named() {
        let data = TempDir::new("octet-fleet-store");
        let err = open(data.path(), "f9-ffff").await.expect_err("missing");
        assert!(
            matches!(&err, StoreError::NoSuchRun(id) if id == "f9-ffff"),
            "{err}"
        );
        made(data.path(), "f1-0000").await;
        let err = open(data.path(), "f9-ffff").await.expect_err("missing");
        assert!(matches!(err, StoreError::NoSuchRun(_)), "{err}");
    }

    #[tokio::test]
    async fn artifacts_are_durable_before_named() {
        let data = TempDir::new("octet-fleet-store");
        let dir = made(data.path(), "f1-0000").await;
        let meta = ArtifactMeta {
            id: ArtifactId(3),
            step: 1,
            requires: vec![ArtifactId(1), ArtifactId(2)],
            candidate: Some(CandidateId(1)),
        };
        write_artifact(&dir, &meta, b"# plan\n", "md")
            .await
            .expect("write");
        assert_eq!(
            std::fs::read(dir.root.join("artifacts/3.md")).expect("body"),
            b"# plan\n"
        );
        assert_eq!(
            read_artifact_meta(&dir, ArtifactId(3)).await.expect("meta"),
            meta
        );
        let err = read_artifact_meta(&dir, ArtifactId(4))
            .await
            .expect_err("absent");
        assert!(matches!(err, StoreError::Io(_)), "{err}");
        let mode = std::fs::metadata(dir.root.join("artifacts/3.md"))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let err = write_artifact(&dir, &meta, b"x", "json")
            .await
            .expect_err("the body cannot be the metadata's file");
        assert!(matches!(err, StoreError::Io(_)), "{err}");
    }

    #[tokio::test]
    async fn an_artifact_write_can_be_retried_after_a_crash() {
        let data = TempDir::new("octet-fleet-store");
        let dir = made(data.path(), "f1-0000").await;
        let artifacts = dir.root.join("artifacts");
        let meta = ArtifactMeta {
            id: ArtifactId(3),
            step: 1,
            requires: vec![],
            candidate: None,
        };
        // A crash after the body's rename, and one mid-write of each temporary.
        std::fs::write(artifacts.join("3.md"), b"old body").expect("orphan body");
        std::fs::write(artifacts.join("3.md.tmp"), b"half a bo").expect("orphan tmp");
        std::fs::write(artifacts.join("3.json.tmp"), b"{\"id\":").expect("orphan tmp");
        assert!(read_artifact_meta(&dir, ArtifactId(3)).await.is_err());

        write_artifact(&dir, &meta, b"new body", "md")
            .await
            .expect("retry");
        assert_eq!(
            std::fs::read(artifacts.join("3.md")).expect("body"),
            b"new body"
        );
        assert_eq!(
            read_artifact_meta(&dir, ArtifactId(3)).await.expect("meta"),
            meta
        );
        let mut names: Vec<_> = std::fs::read_dir(&artifacts)
            .expect("dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["3.json", "3.md"], "no temporary is left behind");
    }

    /// A write that puts half the line down, then fails: a full disk.
    fn half_then_fail(file: &std::fs::File, line: &[u8]) -> io::Result<()> {
        let mut file = file;
        file.write_all(&line[..line.len() / 2])?;
        Err(io::Error::other("disk full"))
    }

    #[tokio::test]
    async fn a_failed_append_is_rolled_back() {
        let data = TempDir::new("octet-fleet-store");
        let dir = made(data.path(), "f1-0000").await;
        let (mut log, _) = Log::open(&dir).await.expect("open");
        log.append(&started(0)).await.expect("append");

        log.write = half_then_fail;
        let err = log.append(&candidate(1)).await.expect_err("fails");
        assert!(matches!(err, StoreError::Io(_)), "{err}");
        assert_eq!(
            std::fs::read(dir.root.join("log.jsonl")).expect("read"),
            line(&started(0)),
            "the torn half line is gone"
        );

        // The log is still usable, and nothing glued onto the torn bytes.
        log.write = write_synced;
        log.append(&started(1))
            .await
            .expect("append after rollback");
        let (_, records) = Log::open(&dir).await.expect("reopen");
        assert_eq!(records, vec![started(0), started(1)]);
    }

    #[tokio::test]
    async fn an_append_that_cannot_roll_back_poisons_the_log() {
        let data = TempDir::new("octet-fleet-store");
        let dir = made(data.path(), "f1-0000").await;
        let path = dir.root.join("log.jsonl");
        std::fs::write(&path, b"").expect("log");
        // A read-only handle: the write fails, and so does the rollback.
        let mut log = Log {
            file: Arc::new(std::fs::File::open(&path).expect("read-only")),
            len: 0,
            poisoned: false,
            write: write_synced,
        };
        let err = log.append(&started(0)).await.expect_err("cannot write");
        assert!(matches!(err, StoreError::Io(_)), "{err}");
        let err = log.append(&started(1)).await.expect_err("poisoned");
        assert!(matches!(err, StoreError::Poisoned), "{err}");
        assert_eq!(std::fs::metadata(&path).expect("meta").len(), 0);
    }

    #[test]
    fn attempt_dirs_are_derived_from_the_id() {
        let dir = RunDir {
            root: PathBuf::from("/data/fleet/f1-0000"),
        };
        assert_eq!(
            attempt_dir(&dir, AttemptId(7)),
            PathBuf::from("/data/fleet/f1-0000/attempts/7")
        );
    }

    #[test]
    fn run_ids_sort_by_time() {
        let id = new_run_id();
        let (secs, hex) = id[1..].split_once('-').expect("f<secs>-<hex>");
        assert!(id.starts_with('f'));
        assert_eq!(secs.len(), 10, "{id}");
        assert!(secs.bytes().all(|b| b.is_ascii_digit()));
        assert_eq!(hex.len(), 4, "{id}");
        assert!(hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        assert_ne!(new_run_id(), new_run_id());
        // Fixed width keeps string order equal to time order.
        let at = |secs: u64, tag: u16| run_id_at(secs, tag);
        assert!(at(999_999_999, 0xffff) < at(1_000_000_000, 0));
        assert!(at(1_763_000_000, 0xffff) < at(1_763_000_001, 0));
        assert_eq!(at(1, 0xab), "f0000000001-00ab");
    }

    #[tokio::test]
    async fn the_workflow_hash_is_gits() {
        let repo = TempDir::new("octet-fleet-store-git");
        std::fs::create_dir_all(repo.path()).expect("repo dir");
        let mut init = std::process::Command::new("git");
        init.args(["init", "--quiet"]).current_dir(repo.path());
        for name in INHERITED {
            init.env_remove(name);
        }
        assert!(init.status().expect("git").success());

        let json = serde_json::to_vec(&run_file().workflow).expect("json");
        let ours = workflow_hash(repo.path(), &json).await.expect("hash");

        let mut hash = std::process::Command::new("git");
        hash.args(["hash-object", "--stdin"])
            .current_dir(repo.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped());
        for name in INHERITED {
            hash.env_remove(name);
        }
        let mut child = hash.spawn().expect("git");
        std::io::Write::write_all(&mut child.stdin.take().expect("stdin"), &json).expect("feed");
        let out = child.wait_with_output().expect("git output");
        assert_eq!(ours, String::from_utf8_lossy(&out.stdout).trim());
        assert_eq!(ours.len(), 40);
    }
}
