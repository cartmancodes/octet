//! Git in a session's workspace, for the phone's Changes screen: what changed, one file's diff,
//! and a commit.
//!
//! The workspace comes from the session's registration, never from the phone, and the hub fixes
//! every argument: the phone names a session, a path from the hub's own last listing, or a
//! message that goes on stdin.
//!
//! - **Scope.** Each call runs `git -c core.fsmonitor=false -C <workspace>`, and what it lists,
//!   stages and commits is limited to the workspace with the pathspec `.`: in a workspace below
//!   its repository's top, files outside it are never listed, staged or committed, and what was
//!   already staged outside stays staged. A diff runs at the top git itself reported for the
//!   workspace (the listed paths are relative to it), naming a listed path only.
//! - **Environment.** Every `GIT_*` variable of the hub's own environment is removed (a
//!   `GIT_DIR`, a `GIT_CONFIG_PARAMETERS`, an identity), then `GIT_TERMINAL_PROMPT=0`,
//!   `GIT_OPTIONAL_LOCKS=0` and `LC_ALL=C` are set. Read calls add `--no-optional-locks`.
//! - **Programs the repository names.** No fsmonitor hook runs (`core.fsmonitor=false`), and
//!   diffs and numstat take `--no-ext-diff --no-textconv`, so no `diff.external` or textconv
//!   driver runs. A `filter.<name>.clean` that `.gitattributes` names still runs when numstat or
//!   `add` reads a changed file, and the commit hooks run: that is the user's own configured
//!   filter and hooks, doing what they do at the desk (accepted).
//! - **Time.** A call runs in a process group of its own, which is killed when the call takes
//!   over 10 seconds, or when the call is dropped before git ended, so no hook outlives it. At
//!   most two calls run at once; a third is refused as busy.
use octet_core::remote::protocol::{
    ChangesInfo, Code, DiffInfo, FileChange, FileStatus, MAX_MESSAGE,
};
use rustix::{
    fs::OFlags,
    process::{Pid, Signal, kill_process_group},
};
use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt as _, OsStringExt as _},
    path::{Component, Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{OwnedSemaphorePermit, Semaphore, watch},
    time::timeout,
};

/// How long one git call may take, hooks included.
pub(crate) const GIT_WITHIN: Duration = Duration::from_secs(10);
/// Git calls that may run at once.
const SLOTS: usize = 2;
/// The most files one `changes` lists.
pub(crate) const FILES_LIMIT: usize = 500;
/// The most bytes of diff text sent.
pub(crate) const DIFF_BYTES: usize = 64 * 1024;
/// The most lines of diff text sent.
pub(crate) const DIFF_LINES: usize = 2000;
/// The most bytes of an untracked file read to count its lines.
const UNTRACKED_READ: u64 = 1024 * 1024;
/// The bytes of an untracked file in which a NUL means binary.
const BINARY_SNIFF: usize = 8 * 1024;
/// The most characters of git's own words a refusal carries.
pub(crate) const GIT_WORDS: usize = 2000;
/// The most bytes of `status` or `numstat` output read.
const LIST_OUTPUT: usize = 32 * 1024 * 1024;
/// The most bytes read of anything else git says.
const WORDS_OUTPUT: usize = 16 * 1024;
/// The longest branch or upstream name kept, in characters.
const NAME_CHARS: usize = 200;
/// The JSON a `changes` or `diff` frame may spend on paths and text: [`MAX_MESSAGE`], less room
/// for everything else in the frame.
const FRAME_ROOM: usize = MAX_MESSAGE - 4096;
/// The JSON one listed file costs besides its path.
const ENTRY_COST: usize = 100;
/// The top-level option every read call takes.
const READ_ONLY: &str = "--no-optional-locks";
/// The pathspec that keeps a call to the workspace.
const HERE: &str = ".";

/// The refusal for a workspace that is not a git work tree (none, bare or corrupt).
pub(crate) const NOT_A_REPOSITORY: &str = "This folder isn't a git repository";
/// The refusal for a third git call while two run.
pub(crate) const GIT_BUSY: &str = "Git is busy; try again";
/// The refusal for a commit with nothing changed.
pub(crate) const NOTHING_TO_COMMIT: &str = "Nothing to commit";
/// The refusal for a commit while a turn runs or an approval waits.
pub(crate) const TURN_FIRST: &str = "Finish or cancel the current turn first";
/// The refusal for a git call past [`GIT_WITHIN`].
pub(crate) const GIT_TIMED_OUT: &str = "Git took too long; try again";
/// The refusal when git cannot be started.
const NO_GIT: &str = "Couldn't run git on the Mac";
/// What a failed git call says when git itself said nothing.
const GIT_FAILED: &str = "Git failed";

/// Why a git call was refused: a code and the words to show.
pub(crate) type Refusal = (Code, String);

fn refusal(code: Code, reason: &str) -> Refusal {
    (code, reason.to_owned())
}

fn not_a_repository() -> Refusal {
    refusal(Code::Unavailable, NOT_A_REPOSITORY)
}

/// Runs git, two calls at a time.
#[derive(Debug, Clone)]
pub(crate) struct Git {
    slots: Arc<Semaphore>,
    within: Duration,
    calls: Arc<AtomicUsize>,
    /// The most bytes of `status` or `numstat` output read.
    list_limit: usize,
    /// The environment git's is made from; the hub's own when none.
    environment: Option<Arc<Vec<(OsString, OsString)>>>,
}

/// What a commit did: its short hash or refusal, and whether `add` had run (so files may be
/// staged even though it was refused).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Commit {
    pub(crate) result: Result<String, Refusal>,
    pub(crate) added: bool,
}

/// Kills git's process group as it drops, unless git was waited for first: a call dropped or
/// timed out takes the hooks git started with it.
#[derive(Debug)]
struct Group(Option<Pid>);

impl Drop for Group {
    fn drop(&mut self) {
        // Git has not been waited for, so its pid still names its group.
        if let Some(group) = self.0.take() {
            let _ = kill_process_group(group, Signal::KILL);
        }
    }
}

/// One of the two places to run git; given back as it drops.
#[derive(Debug)]
pub(crate) struct Slot {
    _permit: OwnedSemaphorePermit,
}

/// What one git call did.
#[derive(Debug)]
struct Ran {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Whether stdout went past its limit (and git was cut off there).
    clipped: bool,
}

/// One file of a listing: git's bytes for its path, and what the diff needs to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listed {
    raw: Vec<u8>,
    /// A rename's old name, diffed with it so the pairing shows.
    orig: Option<Vec<u8>>,
    untracked: bool,
    binary: bool,
}

/// What one `changes` listed, which is all a later `diff` may name.
#[derive(Debug, Clone, Default)]
pub(crate) struct Listing {
    /// The repository's top, as git reported it for the workspace.
    top: PathBuf,
    /// What the files were compared with: `HEAD`, or the empty tree before the first commit.
    base: String,
    /// The listed files, by the path the phone was sent.
    files: HashMap<String, Listed>,
}

impl Listing {
    /// What `diff` needs for `path`, if this listing had it.
    pub(crate) fn target(&self, path: &str) -> Option<DiffTarget> {
        let file = self.files.get(path)?;
        Some(DiffTarget {
            top: self.top.clone(),
            base: self.base.clone(),
            path: path.to_owned(),
            file: file.clone(),
        })
    }
}

/// One listed file to diff.
#[derive(Debug, Clone)]
pub(crate) struct DiffTarget {
    top: PathBuf,
    base: String,
    path: String,
    file: Listed,
}

/// A `changes` answer: the frame's contents, and the listing a `diff` is checked against.
#[derive(Debug)]
pub(crate) struct Changes {
    pub(crate) info: ChangesInfo,
    pub(crate) listing: Listing,
}

/// What `git status --porcelain=v2 --branch -z` said.
#[derive(Debug, Default)]
struct Status {
    branch: Option<String>,
    upstream: Option<String>,
    ahead: u64,
    behind: u64,
    /// No commit yet.
    unborn: bool,
    entries: Vec<Entry>,
}

/// One changed path of a status.
#[derive(Debug)]
struct Entry {
    raw: Vec<u8>,
    /// A rename's old name.
    orig: Option<Vec<u8>>,
    status: FileStatus,
}

impl Git {
    /// Git with the 10-second limit.
    pub(crate) fn new() -> Self {
        Self::with_timeout(GIT_WITHIN)
    }

    /// Git whose calls may each take `within`.
    pub(crate) fn with_timeout(within: Duration) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(SLOTS)),
            within,
            calls: Arc::new(AtomicUsize::new(0)),
            list_limit: LIST_OUTPUT,
            environment: None,
        }
    }

    /// Git that reads at most `limit` bytes of `status` or `numstat`.
    #[cfg(test)]
    fn with_list_limit(limit: usize) -> Self {
        Self {
            list_limit: limit,
            ..Self::new()
        }
    }

    /// Git as if the hub's environment were `environment`.
    #[cfg(test)]
    fn with_environment(environment: Vec<(OsString, OsString)>) -> Self {
        Self {
            environment: Some(Arc::new(environment)),
            ..Self::new()
        }
    }

    /// A place to run git, or busy while two calls run.
    ///
    /// # Errors
    ///
    /// Busy, "Git is busy; try again", while both places are taken.
    pub(crate) fn begin(&self) -> Result<Slot, Refusal> {
        Arc::clone(&self.slots)
            .try_acquire_owned()
            .map(|permit| Slot { _permit: permit })
            .map_err(|_| refusal(Code::Busy, GIT_BUSY))
    }

    /// How many git processes this has started.
    #[cfg(test)]
    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    /// What `ws` changed against HEAD: a `rev-parse` for the repository's top, one `status` and
    /// one `numstat` (with a `hash-object` for the empty tree before the first commit), and the
    /// lines of each listed untracked file counted from at most its first 1 MiB.
    ///
    /// # Errors
    ///
    /// Not a repository, a timeout, or git's own words.
    pub(crate) async fn changes(&self, ws: &Path) -> Result<Changes, Refusal> {
        let top = self.top(ws).await?;
        let status = [
            READ_ONLY,
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
            "--",
            HERE,
        ];
        let ran = self.run(ws, &status, None, self.list_limit).await?;
        // Output past its limit was cut off by the hub: what came is listed, and `more` says
        // there was more.
        if !ran.status.success() && !ran.clipped {
            return Err(not_a_repository());
        }
        let cut_short = ran.clipped;
        let status = parse_status(&ran.stdout);
        let base = if status.unborn {
            self.empty_tree(ws).await?
        } else {
            "HEAD".to_owned()
        };
        let numstat = [
            READ_ONLY,
            "diff",
            "--numstat",
            "--no-ext-diff",
            "--no-textconv",
            "-z",
            base.as_str(),
            "--",
            HERE,
        ];
        let ran = self.run(ws, &numstat, None, self.list_limit).await?;
        if !ran.status.success() && !ran.clipped {
            return Err((Code::Unavailable, words(&ran)));
        }
        let counts = parse_numstat(&ran.stdout);
        let total = status.entries.len();
        let mut files = Vec::new();
        let mut listed = HashMap::new();
        let mut room = FRAME_ROOM;
        let gather = async {
            for entry in status.entries {
                if files.len() == FILES_LIMIT {
                    break;
                }
                let path = String::from_utf8_lossy(&entry.raw).into_owned();
                // Two names that read alike once decoded: only the first can be asked for.
                if listed.contains_key(&path) {
                    continue;
                }
                let cost = json_len(&path) + ENTRY_COST;
                if cost > room {
                    break;
                }
                room -= cost;
                let untracked = entry.status == FileStatus::Untracked;
                let (add, del, binary) = if untracked {
                    count_lines(&top, &entry.raw).await
                } else {
                    counts.get(&entry.raw).copied().unwrap_or_default()
                };
                files.push(FileChange {
                    path: path.clone(),
                    add,
                    del,
                    binary,
                    status: entry.status,
                });
                let file = Listed {
                    raw: entry.raw,
                    orig: entry.orig,
                    untracked,
                    binary,
                };
                listed.insert(path, file);
            }
        };
        timeout(self.within, gather)
            .await
            .map_err(|_| refusal(Code::Timeout, GIT_TIMED_OUT))?;
        let more = (total - files.len()) as u64 + u64::from(cut_short);
        Ok(Changes {
            info: ChangesInfo {
                branch: status.branch,
                upstream: status.upstream,
                ahead: status.ahead,
                behind: status.behind,
                files,
                more,
            },
            listing: Listing {
                top,
                base,
                files: listed,
            },
        })
    }

    /// One listed file's diff against the listing's base, clipped at 64 KiB, 2,000 lines, or
    /// what the frame has room for. An untracked file is diffed against `/dev/null`.
    ///
    /// # Errors
    ///
    /// A timeout, or git's own words (the file went since it was listed, say).
    pub(crate) async fn diff(&self, target: &DiffTarget) -> Result<DiffInfo, Refusal> {
        let mut args: Vec<OsString> = [
            READ_ONLY,
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
        ]
        .map(OsString::from)
        .to_vec();
        if target.file.untracked {
            // `-` alone would be stdin.
            let raw = &target.file.raw;
            let path = if raw == b"-" { &b"./-"[..] } else { raw };
            args.extend(["--no-index", "--", "/dev/null"].map(OsString::from));
            args.push(OsStr::from_bytes(path).to_owned());
        } else {
            // A literal pathspec: a name such as `*` or `:(glob)x` is only itself. A rename is
            // diffed over both its names, so it shows as one.
            let literal = |raw: &[u8]| OsString::from_vec([&b":(literal)"[..], raw].concat());
            args.extend(["-M", target.base.as_str(), "--"].map(OsString::from));
            args.push(literal(&target.file.raw));
            if let Some(orig) = &target.file.orig {
                args.push(literal(orig));
            }
        }
        let ran = self.run(&target.top, &args, None, DIFF_BYTES).await?;
        // `--no-index` exits 1 for differences, and for a file it cannot read (saying so on
        // stderr, with no diff); anything cut short was cut off by the hub.
        let fine = match ran.status.code() {
            Some(0) => true,
            Some(1) => target.file.untracked && !ran.stdout.is_empty(),
            _ => false,
        };
        if !fine && !ran.clipped {
            return Err((Code::Unavailable, words(&ran)));
        }
        let mut text = String::from_utf8_lossy(&ran.stdout).into_owned();
        if ran.clipped {
            // The last character may have been cut in two.
            while text.ends_with('\u{fffd}') {
                text.pop();
            }
        }
        let room = FRAME_ROOM.saturating_sub(json_len(&target.path));
        let cut = clip_text(&text, room);
        let clipped = ran.clipped || cut < text.len();
        text.truncate(cut);
        let binary = target.file.binary
            || text
                .lines()
                .any(|line| line.starts_with("Binary files ") && line.ends_with(" differ"));
        Ok(DiffInfo {
            path: target.path.clone(),
            text,
            clipped,
            binary,
        })
    }

    /// Stages everything in `ws` (and nothing outside it) and commits it with `message` on
    /// stdin, the repository's hooks running: the new commit's short hash (empty if git would
    /// not say it). `clear` is the hub's latest word on whether the session's turn allows a
    /// commit; it is looked at again just before `add`. A turn that starts after that look is
    /// not seen (the window is the time `add` takes to start, and the hub only learns of a
    /// turn when its bridge says so).
    pub(crate) async fn commit(
        &self,
        ws: &Path,
        message: &str,
        clear: &watch::Receiver<bool>,
    ) -> Commit {
        let mut added = false;
        let result = self.committing(ws, message, clear, &mut added).await;
        Commit { result, added }
    }

    async fn committing(
        &self,
        ws: &Path,
        message: &str,
        clear: &watch::Receiver<bool>,
        added: &mut bool,
    ) -> Result<String, Refusal> {
        self.top(ws).await?;
        let status = [
            READ_ONLY,
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--",
            HERE,
        ];
        let ran = self.run(ws, &status, None, WORDS_OUTPUT).await?;
        if !ran.status.success() && !ran.clipped {
            return Err(not_a_repository());
        }
        if ran.stdout.is_empty() {
            return Err(refusal(Code::Invalid, NOTHING_TO_COMMIT));
        }
        if !*clear.borrow() {
            return Err(refusal(Code::TurnOpen, TURN_FIRST));
        }
        *added = true;
        let ran = self
            .run(ws, &["add", "-A", "--", HERE], None, WORDS_OUTPUT)
            .await?;
        if !ran.status.success() {
            return Err((Code::NotAllowed, words(&ran)));
        }
        let commit = ["commit", "-F", "-", "--", HERE];
        let ran = self
            .run(ws, &commit, Some(message.as_bytes()), WORDS_OUTPUT)
            .await?;
        if !ran.status.success() {
            let said = words(&ran);
            if said.contains("nothing to commit") || said.contains("no changes added") {
                return Err(refusal(Code::Invalid, NOTHING_TO_COMMIT));
            }
            return Err((Code::NotAllowed, said));
        }
        let short = [READ_ONLY, "rev-parse", "--short", "HEAD"];
        let ran = self.run(ws, &short, None, WORDS_OUTPUT).await?;
        let hash = String::from_utf8_lossy(&ran.stdout).trim().to_owned();
        Ok(if ran.status.success() {
            hash
        } else {
            String::new()
        })
    }

    /// The top of the work tree `ws` is in, as git reports it; refused for a relative path, a
    /// folder that is not in a work tree, a bare repository or a corrupt one.
    async fn top(&self, ws: &Path) -> Result<PathBuf, Refusal> {
        if !ws.is_absolute() {
            return Err(not_a_repository());
        }
        let ran = self
            .run(
                ws,
                &[READ_ONLY, "rev-parse", "--show-toplevel"],
                None,
                WORDS_OUTPUT,
            )
            .await?;
        let mut out = ran.stdout;
        if out.last() == Some(&b'\n') {
            out.pop();
        }
        if !ran.status.success() || ran.clipped || out.is_empty() {
            return Err(not_a_repository());
        }
        Ok(PathBuf::from(OsStr::from_bytes(&out)))
    }

    /// The empty tree's id in `ws`'s object format.
    async fn empty_tree(&self, ws: &Path) -> Result<String, Refusal> {
        let args = [READ_ONLY, "hash-object", "-t", "tree", "/dev/null"];
        let ran = self.run(ws, &args, None, WORDS_OUTPUT).await?;
        let id = String::from_utf8_lossy(&ran.stdout).trim().to_owned();
        if !ran.status.success() || id.is_empty() {
            return Err((Code::Unavailable, words(&ran)));
        }
        Ok(id)
    }

    /// Runs `git -C dir <args>`, feeding `input` on stdin (null without), keeping at most
    /// `limit` bytes of stdout: past it the pipe is closed and git is cut off. Past the time
    /// limit, git's process group is killed.
    async fn run<S: AsRef<OsStr>>(
        &self,
        dir: &Path,
        args: &[S],
        input: Option<&[u8]>,
        limit: usize,
    ) -> Result<Ran, Refusal> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let inherited = match &self.environment {
            Some(environment) => environment.to_vec(),
            None => std::env::vars_os().collect(),
        };
        let mut command = Command::new("git");
        command.env_clear();
        for (key, value) in inherited {
            if !key.as_bytes().starts_with(b"GIT_") {
                command.env(key, value);
            }
        }
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("LC_ALL", "C")
            .args(["-c", "core.fsmonitor=false", "-C"])
            .arg(dir)
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        let mut child = command
            .spawn()
            .map_err(|_| refusal(Code::Unavailable, NO_GIT))?;
        let mut group = Group(
            child
                .id()
                .and_then(|id| i32::try_from(id).ok())
                .and_then(Pid::from_raw),
        );
        let (stdin, stdout, stderr) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let work = async {
            let feed = async move {
                if let (Some(mut pipe), Some(bytes)) = (stdin, input) {
                    let _ = pipe.write_all(bytes).await;
                }
            };
            let ((), (stdout, clipped), (stderr, _)) = tokio::join!(
                feed,
                read_capped(stdout, limit),
                read_capped(stderr, WORDS_OUTPUT)
            );
            let status = child.wait().await;
            if status.is_ok() {
                // Waited for: its pid may be another's now.
                group.0 = None;
            }
            (status, stdout, stderr, clipped)
        };
        // Past the time limit, or dropped before git ended, `group` kills git and its hooks.
        match timeout(self.within, work).await {
            Ok((Ok(status), stdout, stderr, clipped)) => Ok(Ran {
                status,
                stdout,
                stderr,
                clipped,
            }),
            Ok((Err(_), ..)) => Err(refusal(Code::Unavailable, NO_GIT)),
            Err(_) => Err(refusal(Code::Timeout, GIT_TIMED_OUT)),
        }
    }
}

/// Up to `limit` bytes from `pipe`, and whether there was more; the pipe is closed after.
async fn read_capped(pipe: Option<impl AsyncRead + Unpin>, limit: usize) -> (Vec<u8>, bool) {
    let mut bytes = Vec::new();
    if let Some(pipe) = pipe {
        let wanted = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
        let _ = pipe.take(wanted).read_to_end(&mut bytes).await;
    }
    let clipped = bytes.len() > limit;
    bytes.truncate(limit);
    (bytes, clipped)
}

/// What git said on stderr, then stdout, clipped to [`GIT_WORDS`] characters.
fn words(ran: &Ran) -> String {
    let mut said = String::from_utf8_lossy(&ran.stderr).trim().to_owned();
    let out = String::from_utf8_lossy(&ran.stdout);
    let out = out.trim();
    if !out.is_empty() {
        if !said.is_empty() {
            said.push('\n');
        }
        said.push_str(out);
    }
    if said.is_empty() {
        return GIT_FAILED.to_owned();
    }
    said.chars().take(GIT_WORDS).collect()
}

/// The bytes `text` takes as a JSON string, quotes included.
fn json_len(text: &str) -> usize {
    2 + text.chars().map(escaped_len).sum::<usize>()
}

/// The bytes `c` takes inside a JSON string.
fn escaped_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if c < ' ' => 6,
        c => c.len_utf8(),
    }
}

/// Where to cut `text` so it keeps at most [`DIFF_BYTES`] bytes, [`DIFF_LINES`] lines and `room`
/// bytes of JSON: its length when it fits.
fn clip_text(text: &str, room: usize) -> usize {
    let (mut lines, mut json) = (0, 2);
    for (at, c) in text.char_indices() {
        json += escaped_len(c);
        if at + c.len_utf8() > DIFF_BYTES || json > room {
            return at;
        }
        if c == '\n' {
            lines += 1;
            if lines == DIFF_LINES {
                return at + 1;
            }
        }
    }
    text.len()
}

/// Counts the lines of the untracked file `raw` under `top` from at most its first 1 MiB: a NUL
/// in its first 8 KiB means binary. Only a regular file is read, never through a symlink, and
/// nothing that could block (a FIFO put there since git looked) holds it up.
async fn count_lines(top: &Path, raw: &[u8]) -> (u64, u64, bool) {
    let relative = Path::new(OsStr::from_bytes(raw));
    if !relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
    {
        return (0, 0, false);
    }
    let flags = (OFlags::NOFOLLOW | OFlags::NONBLOCK).bits().cast_signed();
    let Ok(file) = tokio::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(top.join(relative))
        .await
    else {
        return (0, 0, false);
    };
    if !file.metadata().await.is_ok_and(|meta| meta.is_file()) {
        return (0, 0, false);
    }
    let mut bytes = Vec::new();
    let _ = file.take(UNTRACKED_READ).read_to_end(&mut bytes).await;
    if bytes[..bytes.len().min(BINARY_SNIFF)].contains(&0) {
        return (0, 0, true);
    }
    let mut lines = bytes.iter().fold(0, |n: u64, &b| n + u64::from(b == b'\n'));
    if bytes.last().is_some_and(|&b| b != b'\n') {
        lines += 1;
    }
    (lines, 0, false)
}

/// The NUL-ended records of `out`; a last record with no NUL after it was cut short, and is
/// dropped.
fn records(out: &[u8]) -> Vec<&[u8]> {
    let mut pieces: Vec<&[u8]> = out.split(|&b| b == 0).collect();
    pieces.pop();
    pieces
}

/// At most [`NAME_CHARS`] characters of a branch name, read as UTF-8.
fn name(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .take(NAME_CHARS)
        .collect()
}

/// The rest of `record` after its first `fields` space-separated fields: the path.
fn after_fields(record: &[u8], fields: usize) -> Option<&[u8]> {
    record.splitn(fields + 1, |&b| b == b' ').nth(fields)
}

/// Parses `git status --porcelain=v2 --branch -z`. A path is git's bytes, NULs apart, so no
/// name can be misread; a rename is listed by its new name.
fn parse_status(out: &[u8]) -> Status {
    let mut status = Status::default();
    let pieces = records(out);
    let mut at = 0;
    while at < pieces.len() {
        let record = pieces[at];
        at += 1;
        let mut orig_name = None;
        let (kind, rest) = (record.first(), record.get(2..).unwrap_or_default());
        let entry = match kind {
            Some(b'#') => {
                header(&mut status, rest);
                continue;
            }
            Some(b'1') => {
                let xy = rest.get(..2).unwrap_or_default();
                let file = match xy {
                    [b'D', _] | [_, b'D'] => FileStatus::Deleted,
                    [b'A', _] => FileStatus::Added,
                    [b'R' | b'C', _] => FileStatus::Renamed,
                    _ => FileStatus::Modified,
                };
                after_fields(record, 8).map(|path| (path, file))
            }
            Some(b'2') => {
                // The original name follows as a record of its own.
                let Some(orig) = pieces.get(at) else {
                    break;
                };
                at += 1;
                orig_name = Some(orig.to_vec());
                after_fields(record, 9).map(|path| (path, FileStatus::Renamed))
            }
            Some(b'u') => after_fields(record, 10).map(|path| (path, FileStatus::Unmerged)),
            Some(b'?') => Some((rest, FileStatus::Untracked)),
            _ => None,
        };
        if let Some((path, file)) = entry.filter(|(path, _)| !path.is_empty()) {
            status.entries.push(Entry {
                raw: path.to_vec(),
                orig: orig_name,
                status: file,
            });
        }
    }
    status
}

/// One `# branch.*` header of a status.
fn header(status: &mut Status, line: &[u8]) {
    let mut parts = line.splitn(2, |&b| b == b' ');
    let (Some(key), Some(value)) = (parts.next(), parts.next()) else {
        return;
    };
    match key {
        b"branch.oid" => status.unborn = value == b"(initial)",
        b"branch.head" if value != b"(detached)" => status.branch = Some(name(value)),
        b"branch.upstream" => status.upstream = Some(name(value)),
        b"branch.ab" => {
            let mut counts = value.split(|&b| b == b' ').map(|count| {
                std::str::from_utf8(count.get(1..).unwrap_or_default())
                    .ok()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or_default()
            });
            status.ahead = counts.next().unwrap_or_default();
            status.behind = counts.next().unwrap_or_default();
        }
        _ => {}
    }
}

/// Parses `git diff --numstat -z`: `(added, deleted, binary)` by path, a binary file's counts
/// `-` read as 0, and a rename (an empty path, then the old and new names) kept by its new name.
fn parse_numstat(out: &[u8]) -> HashMap<Vec<u8>, (u64, u64, bool)> {
    let count = |field: &[u8]| {
        std::str::from_utf8(field)
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or_default()
    };
    let mut counts = HashMap::new();
    let pieces = records(out);
    let mut at = 0;
    while at < pieces.len() {
        let mut fields = pieces[at].splitn(3, |&b| b == b'\t');
        at += 1;
        let (Some(add), Some(del), Some(path)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let path = if path.is_empty() {
            let Some(new) = pieces.get(at + 1) else {
                break;
            };
            at += 2;
            *new
        } else {
            path
        };
        let binary = add == b"-" && del == b"-";
        counts.insert(path.to_vec(), (count(add), count(del), binary));
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use octet_core::remote::protocol::{MAX_MESSAGE, changes_frame};
    use octet_testkit::TempDir;
    use std::fmt::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::Instant;

    /// Runs git in `dir` as a person at the desk would, with an identity of its own.
    fn git_in(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A fresh repository whose own config names who commits, so the hub's git needs nothing
    /// from the machine.
    fn repo(name: &str) -> TempDir {
        let dir = TempDir::new(name);
        std::fs::create_dir_all(dir.path()).expect("repo directory");
        git_in(dir.path(), &["init", "-q"]);
        let hooks = dir.path().join(".git/hooks");
        let hooks = hooks.to_str().expect("a UTF-8 temp path");
        for (key, value) in [
            ("user.name", "Test"),
            ("user.email", "test@example.com"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", hooks),
        ] {
            git_in(dir.path(), &["config", key, value]);
        }
        dir
    }

    /// A repository with one commit of `tracked.txt`.
    fn committed(name: &str) -> TempDir {
        let dir = repo(name);
        std::fs::write(dir.path().join("tracked.txt"), "one\ntwo\n").expect("write");
        git_in(dir.path(), &["add", "-A"]);
        git_in(dir.path(), &["commit", "-q", "-m", "First"]);
        dir
    }

    fn hook(dir: &Path, name: &str, body: &str) {
        let path = dir.join(".git/hooks").join(name);
        std::fs::create_dir_all(path.parent().expect("hooks")).expect("hooks directory");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("hook");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    /// Commits with the session's turn clear throughout.
    async fn commit(git: &Git, ws: &Path, message: &str) -> Result<String, Refusal> {
        let (_clear, turn) = watch::channel(true);
        git.commit(ws, message, &turn).await.result
    }

    /// A program the repository's config could name: it leaves a mark in `marks`, then does
    /// what a harmless one would (`cat` its file, or for fsmonitor, say nothing is known).
    fn marker(bin: &Path, marks: &Path, name: &str) -> String {
        std::fs::create_dir_all(marks).expect("marks");
        std::fs::create_dir_all(bin).expect("bin");
        let script = bin.join(format!("{name}.sh"));
        let body = format!(
            "#!/bin/sh\ntouch '{}/{name}-'$$\n[ -f \"$1\" ] && cat \"$1\"\nexit 0\n",
            marks.display()
        );
        std::fs::write(&script, body).expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        script.to_str().expect("a UTF-8 temp path").to_owned()
    }

    fn marks_left(marks: &Path) -> Vec<String> {
        std::fs::read_dir(marks)
            .expect("marks")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect()
    }

    fn paths(changes: &Changes) -> Vec<&str> {
        changes.info.files.iter().map(|f| f.path.as_str()).collect()
    }

    #[test]
    fn porcelain_v2_parses_with_nuls_in_mind() {
        let text = b"# branch.oid 0123abcd\0# branch.head feat/x\0\
            # branch.upstream origin/feat/x\0# branch.ab +2 -1\0\
            1 .M N... 100644 100644 100644 aaaa aaaa src/a b.rs\0\
            1 A. N... 000000 100644 100644 0000 bbbb new\nline.txt\0\
            2 R. N... 100644 100644 100644 cccc cccc R100 to me.txt\0from me.txt\0\
            1 .D N... 100644 100644 000000 dddd dddd gone.txt\0\
            u UU N... 100644 100644 100644 100644 e e e conflict.rs\0\
            ? -rf --no-preserve-root\0! ignored.log\0";
        let status = parse_status(text);
        assert_eq!(status.branch.as_deref(), Some("feat/x"));
        assert_eq!(status.upstream.as_deref(), Some("origin/feat/x"));
        assert_eq!((status.ahead, status.behind), (2, 1));
        assert!(!status.unborn);
        let entries: Vec<(&[u8], FileStatus)> = status
            .entries
            .iter()
            .map(|e| (e.raw.as_slice(), e.status))
            .collect();
        assert_eq!(
            entries,
            [
                (&b"src/a b.rs"[..], FileStatus::Modified),
                (&b"new\nline.txt"[..], FileStatus::Added),
                (&b"to me.txt"[..], FileStatus::Renamed),
                (&b"gone.txt"[..], FileStatus::Deleted),
                (&b"conflict.rs"[..], FileStatus::Unmerged),
                (&b"-rf --no-preserve-root"[..], FileStatus::Untracked),
            ]
        );

        assert_eq!(status.entries[2].orig.as_deref(), Some(&b"from me.txt"[..]));
        assert_eq!(status.entries[0].orig, None);
        let fresh = parse_status(b"# branch.oid (initial)\0# branch.head (detached)\0");
        assert!(fresh.unborn);
        assert_eq!(fresh.branch, None);
        assert_eq!(fresh.upstream, None);
        assert!(fresh.entries.is_empty());
        // A record cut short (output past its limit) is dropped, not misread.
        let cut = parse_status(b"1 .M N... 100644 100644 100644 aaaa aaaa ok.rs\0\x31 .M N");
        assert_eq!(cut.entries.len(), 1);
    }

    #[test]
    fn numstat_parses_renames_and_binaries() {
        let counts =
            parse_numstat(b"3\t1\tsrc/a b.rs\0-\t-\timg.png\x005\t0\t\0from.txt\0to\tme.txt\0");
        assert_eq!(counts.get(&b"src/a b.rs"[..]), Some(&(3, 1, false)));
        assert_eq!(counts.get(&b"img.png"[..]), Some(&(0, 0, true)));
        assert_eq!(counts.get(&b"to\tme.txt"[..]), Some(&(5, 0, false)));
        assert_eq!(counts.len(), 3);
        assert!(parse_numstat(b"").is_empty());
        assert!(parse_numstat(b"garbage").is_empty());
    }

    #[tokio::test]
    async fn not_a_repository() {
        let git = Git::new();
        let plain = TempDir::new("octet-git-plain");
        std::fs::create_dir_all(plain.path()).expect("plain directory");
        let bare = TempDir::new("octet-git-bare");
        std::fs::create_dir_all(bare.path()).expect("bare directory");
        git_in(bare.path(), &["init", "-q", "--bare"]);
        let corrupt = TempDir::new("octet-git-corrupt");
        std::fs::create_dir_all(corrupt.path().join(".git")).expect("corrupt directory");
        std::fs::write(corrupt.path().join(".git/HEAD"), "garbage").expect("HEAD");
        let missing = TempDir::new("octet-git-missing");
        let file = TempDir::new("octet-git-file");
        std::fs::write(file.path(), "a file").expect("file");
        for ws in [
            plain.path(),
            bare.path(),
            corrupt.path(),
            missing.path(),
            file.path(),
            Path::new("relative/path"),
        ] {
            let refusal = git.changes(ws).await.expect_err("refused");
            assert_eq!(
                refusal,
                (Code::Unavailable, NOT_A_REPOSITORY.to_owned()),
                "{}",
                ws.display()
            );
            let refusal = commit(&git, ws, "Message").await.expect_err("refused");
            assert_eq!(refusal, (Code::Unavailable, NOT_A_REPOSITORY.to_owned()));
        }
        // Git is still there for the next caller.
        let fine = committed("octet-git-fine");
        assert!(git.changes(fine.path()).await.is_ok());
    }

    #[tokio::test]
    async fn hostile_names_round_trip() {
        let git = Git::new();
        let dir = committed("octet-git-hostile");
        let ws = dir.path();
        let tracked = "-tracked \"q\" 'x'\n.txt";
        std::fs::write(ws.join(tracked), "base\n").expect("write");
        git_in(ws, &["add", "-A"]);
        git_in(ws, &["commit", "-q", "-m", "Hostile"]);
        std::fs::write(ws.join(tracked), "base\nhello tracked\n").expect("write");
        let untracked = [
            "new\nline.txt",
            "quote\"d 'name'.txt",
            "-rf",
            "--output=pwned",
            "ünïcødé ✓.txt",
            "..dots",
            "tab\there",
            "back\\slash",
            " leading space",
            "$(touch pwned)",
            ":(glob)*",
            "HEAD",
            "-",
        ];
        std::fs::create_dir_all(ws.join("dir")).expect("dir");
        let nested = "dir/..hidden";
        for (n, name) in untracked.iter().chain([&nested]).enumerate() {
            std::fs::write(ws.join(name), format!("hello {n}\n")).expect("write");
        }
        let changes = git.changes(ws).await.expect("changes");
        let listed = paths(&changes);
        for name in untracked.iter().chain([&nested]) {
            let file = changes
                .info
                .files
                .iter()
                .find(|f| f.path == *name)
                .unwrap_or_else(|| panic!("{name:?} not in {listed:?}"));
            assert_eq!(file.status, FileStatus::Untracked, "{name:?}");
            assert_eq!((file.add, file.del, file.binary), (1, 0, false), "{name:?}");
        }
        let file = changes.info.files.iter().find(|f| f.path == tracked);
        let file = file.unwrap_or_else(|| panic!("{tracked:?} not in {listed:?}"));
        assert_eq!(file.status, FileStatus::Modified);
        assert_eq!((file.add, file.del), (1, 0));
        // Each one's diff is its own file's, whatever its name says.
        for (n, name) in untracked.iter().chain([&nested]).enumerate() {
            let target = changes.listing.target(name).expect("listed");
            let diff = git.diff(&target).await.expect("diff");
            assert_eq!(diff.path, *name);
            assert!(
                diff.text.contains(&format!("+hello {n}\n")),
                "{name:?}: {}",
                diff.text
            );
            assert!(!diff.text.contains("+hello tracked"), "{name:?}");
        }
        let target = changes.listing.target(tracked).expect("listed");
        let diff = git.diff(&target).await.expect("diff");
        assert!(diff.text.contains("+hello tracked\n"), "{}", diff.text);
        assert!(!diff.text.contains("hello 0"), "{}", diff.text);
        // Nothing reached a shell or became an option.
        assert!(!ws.join("pwned").exists());
        assert_eq!(changes.info.files.len(), untracked.len() + 2);
    }

    #[tokio::test]
    async fn diffs_are_clipped_and_say_binary() {
        let git = Git::new();
        let dir = committed("octet-git-diffs");
        let ws = dir.path();
        let long = (0..5000).fold(String::new(), |mut text, n| {
            let _ = writeln!(text, "line {n}");
            text
        });
        std::fs::write(ws.join("tracked.txt"), &long).expect("write");
        std::fs::write(ws.join("wide.txt"), "é".repeat(200_000)).expect("write");
        std::fs::write(ws.join("blob.bin"), b"\x00\x01\x02binary").expect("write");
        let changes = git.changes(ws).await.expect("changes");
        let tracked = changes.info.files.iter().find(|f| f.path == "tracked.txt");
        assert_eq!(tracked.map(|f| (f.add, f.del)), Some((5000, 2)));
        let blob = changes.info.files.iter().find(|f| f.path == "blob.bin");
        assert!(blob.is_some_and(|f| f.binary && f.add == 0));

        let diff = git
            .diff(&changes.listing.target("tracked.txt").expect("listed"))
            .await
            .expect("diff");
        assert!(diff.clipped);
        assert!(diff.text.lines().count() <= DIFF_LINES);
        assert!(diff.text.contains("+line 0\n"));
        let diff = git
            .diff(&changes.listing.target("wide.txt").expect("listed"))
            .await
            .expect("diff");
        assert!(diff.clipped);
        assert!(diff.text.len() <= DIFF_BYTES);
        assert!(!diff.text.ends_with('\u{fffd}'));
        let diff = git
            .diff(&changes.listing.target("blob.bin").expect("listed"))
            .await
            .expect("diff");
        assert!(diff.binary);
        assert!(!diff.clipped);

        // A diff whose file went since the listing is refused in git's words, and git goes on.
        std::fs::remove_file(ws.join("wide.txt")).expect("remove");
        let gone = git
            .diff(&changes.listing.target("wide.txt").expect("listed"))
            .await;
        assert!(gone.is_err());
        assert!(git.changes(ws).await.is_ok());
    }

    #[tokio::test]
    async fn a_repository_with_no_commit_lists_and_diffs_against_the_empty_tree() {
        let git = Git::new();
        let dir = repo("octet-git-unborn");
        let ws = dir.path();
        std::fs::write(ws.join("staged.txt"), "a\nb\n").expect("write");
        git_in(ws, &["add", "staged.txt"]);
        std::fs::write(ws.join("loose.txt"), "c\n").expect("write");
        let changes = git.changes(ws).await.expect("changes");
        let staged = changes.info.files.iter().find(|f| f.path == "staged.txt");
        assert_eq!(
            staged.map(|f| (f.status, f.add, f.del)),
            Some((FileStatus::Added, 2, 0))
        );
        assert!(changes.info.branch.is_some());
        let diff = git
            .diff(&changes.listing.target("staged.txt").expect("listed"))
            .await
            .expect("diff");
        assert!(diff.text.contains("+a\n+b\n"), "{}", diff.text);
        // A first commit works too.
        let hash = commit(&git, ws, "First").await.expect("committed");
        assert_eq!(hash, git_in(ws, &["rev-parse", "--short", "HEAD"]).trim());
    }

    #[tokio::test]
    async fn a_workspace_below_the_top_lists_and_commits_only_itself() {
        let git = Git::new();
        let dir = repo("octet-git-below");
        let top = dir.path();
        let ws = top.join("ws");
        std::fs::create_dir_all(top.join("other")).expect("other");
        std::fs::create_dir_all(&ws).expect("ws");
        for (path, text) in [
            (".bashrc", "dot\n"),
            ("other/o.txt", "o\n"),
            ("ws/keep.txt", "k\n"),
            ("ws/old.txt", "r\n"),
        ] {
            std::fs::write(top.join(path), text).expect("write");
        }
        git_in(top, &["add", "-A"]);
        git_in(top, &["commit", "-q", "-m", "First"]);
        std::fs::write(top.join(".bashrc"), "dot\nchanged\n").expect("write");
        std::fs::write(top.join("other/new.txt"), "x\n").expect("write");
        git_in(top, &["add", "other/new.txt"]);
        std::fs::write(ws.join("keep.txt"), "k\nw\n").expect("write");
        git_in(top, &["mv", "ws/old.txt", "ws/renamed.txt"]);
        std::fs::write(ws.join("new.txt"), "n\n").expect("write");

        let changes = git.changes(&ws).await.expect("changes");
        let mut listed = paths(&changes);
        listed.sort_unstable();
        assert_eq!(listed, ["ws/keep.txt", "ws/new.txt", "ws/renamed.txt"]);
        assert_eq!(changes.info.more, 0);
        for path in listed {
            let target = changes.listing.target(path).expect("listed");
            assert!(git.diff(&target).await.is_ok_and(|d| !d.text.is_empty()));
        }
        // A rename shows as one, from its old name.
        let target = changes.listing.target("ws/renamed.txt").expect("listed");
        let diff = git.diff(&target).await.expect("diff");
        assert!(
            diff.text.contains("rename from ws/old.txt"),
            "{}",
            diff.text
        );
        assert!(
            diff.text.contains("rename to ws/renamed.txt"),
            "{}",
            diff.text
        );

        commit(&git, &ws, "Only the workspace")
            .await
            .expect("committed");
        let committed = git_in(
            top,
            &["show", "--no-renames", "--name-only", "--format=", "HEAD"],
        );
        let mut committed: Vec<&str> = committed.lines().collect();
        committed.sort_unstable();
        assert_eq!(
            committed,
            ["ws/keep.txt", "ws/new.txt", "ws/old.txt", "ws/renamed.txt"]
        );
        // What was staged outside stays staged, and the rest of the top is untouched.
        assert_eq!(
            git_in(top, &["diff", "--cached", "--name-only"]).trim(),
            "other/new.txt"
        );
        assert_eq!(git_in(top, &["diff", "--name-only"]).trim(), ".bashrc");
        assert_eq!(
            commit(&git, &ws, "Again").await,
            Err((Code::Invalid, NOTHING_TO_COMMIT.to_owned()))
        );
    }

    #[tokio::test]
    async fn the_repository_runs_no_programs_of_its_own() {
        let git = Git::new();
        let dir = committed("octet-git-programs");
        let ws = dir.path();
        let (bin, marks) = (
            TempDir::new("octet-git-bin"),
            TempDir::new("octet-git-marks"),
        );
        let textconv = marker(bin.path(), marks.path(), "textconv");
        let fsmonitor = marker(bin.path(), marks.path(), "fsmonitor");
        let external = marker(bin.path(), marks.path(), "external");
        std::fs::write(ws.join(".gitattributes"), "*.txt diff=evil\n").expect("attributes");
        git_in(ws, &["add", "-A"]);
        git_in(ws, &["commit", "-q", "-m", "Attributes"]);
        for (key, value) in [
            ("diff.evil.textconv", textconv.as_str()),
            ("diff.external", external.as_str()),
            ("core.fsmonitor", fsmonitor.as_str()),
        ] {
            git_in(ws, &["config", key, value]);
        }
        std::fs::write(ws.join("tracked.txt"), "one\ntwo\nthree\n").expect("write");
        std::fs::write(ws.join("loose.txt"), "loose\n").expect("write");
        let changes = git.changes(ws).await.expect("changes");
        for path in ["tracked.txt", "loose.txt"] {
            let target = changes.listing.target(path).expect("listed");
            let diff = git.diff(&target).await.expect("diff");
            assert!(diff.text.contains("@@"), "{path}: {}", diff.text);
        }
        commit(&git, ws, "No programs").await.expect("committed");
        assert_eq!(marks_left(marks.path()), Vec::<String>::new());
    }

    #[tokio::test]
    async fn the_hubs_own_git_environment_never_reaches_git() {
        let dir = committed("octet-git-environment");
        let ws = dir.path();
        let elsewhere = committed("octet-git-elsewhere");
        let inherited = [
            ("GIT_CONFIG_PARAMETERS", "'user.name'='Evil'".to_owned()),
            ("GIT_AUTHOR_NAME", "Evil".to_owned()),
            ("GIT_COMMITTER_NAME", "Evil".to_owned()),
            (
                "GIT_DIR",
                elsewhere.path().join(".git").display().to_string(),
            ),
            ("GIT_INDEX_FILE", "/nonexistent/index".to_owned()),
        ];
        let git = Git::with_environment(
            std::env::vars_os()
                .chain(inherited.map(|(k, v)| (k.into(), v.into())))
                .collect(),
        );
        std::fs::write(ws.join("new.txt"), "new\n").expect("write");
        let changes = git.changes(ws).await.expect("changes");
        assert_eq!(paths(&changes), ["new.txt"]);
        commit(&git, ws, "Mine").await.expect("committed");
        assert_eq!(
            git_in(ws, &["log", "-1", "--format=%an %cn"]).trim(),
            "Test Test"
        );
        assert_eq!(
            git_in(elsewhere.path(), &["rev-list", "--count", "HEAD"]).trim(),
            "1"
        );
    }

    #[tokio::test]
    async fn a_clipped_status_lists_what_it_has() {
        let git = Git::with_list_limit(2000);
        let dir = committed("octet-git-clipped");
        // About 13 bytes a file, over 128 KiB in all: more than a pipe holds, so git is still
        // writing when the hub stops reading, and is cut off.
        for n in 0..10_000 {
            std::fs::write(dir.path().join(format!("f{n:05}.txt")), "x\n").expect("write");
        }
        let changes = git.changes(dir.path()).await.expect("changes");
        let shown = changes.info.files.len();
        assert!(shown > 0 && shown < 10_000, "{shown}");
        assert!(changes.info.more >= 1);
        assert_eq!(changes.info.files[0].path, "f00000.txt");
    }

    #[tokio::test]
    async fn a_commit_stops_before_add_when_a_turn_starts() {
        let git = Git::new();
        let dir = committed("octet-git-turn");
        let ws = dir.path();
        std::fs::write(ws.join("new.txt"), "new\n").expect("write");
        let (clear, turn) = watch::channel(true);
        clear.send_replace(false);
        let commit = git.commit(ws, "Too late", &turn).await;
        assert_eq!(commit.result, Err((Code::TurnOpen, TURN_FIRST.to_owned())));
        assert!(!commit.added);
        assert_eq!(git_in(ws, &["diff", "--cached", "--name-only"]), "");
        // Once git has staged, a refusal says so.
        hook(ws, "pre-commit", "exit 1");
        clear.send_replace(true);
        let commit = git.commit(ws, "Refused", &turn).await;
        assert!(commit.result.is_err() && commit.added);
        let fresh = committed("octet-git-turn-clean");
        let commit = git.commit(fresh.path(), "Nothing", &turn).await;
        assert_eq!(
            (commit.result, commit.added),
            (Err((Code::Invalid, NOTHING_TO_COMMIT.to_owned())), false)
        );
    }

    #[tokio::test]
    async fn a_dropped_call_takes_its_hooks_with_it() {
        let git = Git::new();
        let dir = committed("octet-git-dropped");
        let ws = dir.path().to_owned();
        std::fs::write(ws.join("new.txt"), "new\n").expect("write");
        let pid_file = dir.path().join(".git/hook.pid");
        hook(
            &ws,
            "pre-commit",
            &format!("echo $$ > '{}'\nsleep 30", pid_file.display()),
        );
        let call = tokio::spawn(async move { commit(&git, &ws, "Hangs").await });
        let began = Instant::now();
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|t| t.trim().parse::<i32>().ok())
            {
                break pid;
            }
            assert!(
                began.elapsed() < Duration::from_secs(5),
                "the hook never ran"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        call.abort();
        let _ = call.await;
        let alive = |pid: i32| {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        let began = Instant::now();
        while alive(pid) {
            assert!(
                began.elapsed() < Duration::from_secs(3),
                "the hook outlived its call"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn commit_refusals_and_hook_failure() {
        let git = Git::new();
        let dir = committed("octet-git-commit");
        let ws = dir.path();
        assert_eq!(
            commit(&git, ws, "Nothing here").await,
            Err((Code::Invalid, NOTHING_TO_COMMIT.to_owned()))
        );

        std::fs::write(ws.join("new.txt"), "new\n").expect("write");
        hook(
            ws,
            "pre-commit",
            "echo \"lint failed: $(printf 'x%.0s' $(seq 1 3000))\" >&2\nexit 1",
        );
        let (code, words) = commit(&git, ws, "Add new")
            .await
            .expect_err("the hook refuses");
        assert_eq!(code, Code::NotAllowed);
        assert!(words.starts_with("lint failed: xxx"), "{words}");
        assert!(words.chars().count() <= GIT_WORDS, "{}", words.len());
        // The index stays as git left it: `add -A` ran.
        assert!(git_in(ws, &["diff", "--cached", "--name-only"]).contains("new.txt"));

        // A hook that hangs ends at the timeout.
        hook(ws, "pre-commit", "sleep 30");
        let quick = Git::with_timeout(Duration::from_secs(1));
        let began = Instant::now();
        assert_eq!(
            commit(&quick, ws, "Add new").await,
            Err((Code::Timeout, GIT_TIMED_OUT.to_owned()))
        );
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "{:?}",
            began.elapsed()
        );
        // Git was killed holding the index lock, which stays as git left it; the desk clears
        // it as after any git that was killed.
        let _ = std::fs::remove_file(ws.join(".git/index.lock"));

        std::fs::remove_file(ws.join(".git/hooks/pre-commit")).expect("unhook");
        let message = "-m evil --amend\n\nThe body; $(touch pwned)\n";
        let hash = commit(&git, ws, message).await.expect("committed");
        assert_eq!(hash, git_in(ws, &["rev-parse", "--short", "HEAD"]).trim());
        assert_eq!(
            git_in(ws, &["log", "-1", "--format=%B"]).trim_end(),
            message.trim_end()
        );
        assert_eq!(git_in(ws, &["rev-list", "--count", "HEAD"]).trim(), "2");
        assert!(!ws.join("pwned").exists());
        assert_eq!(
            commit(&git, ws, "Again").await,
            Err((Code::Invalid, NOTHING_TO_COMMIT.to_owned()))
        );
    }

    #[tokio::test]
    async fn huge_change_is_capped() {
        let git = Git::new();
        let dir = committed("octet-git-huge");
        let ws = dir.path();
        let vendor = ws.join("vendor");
        std::fs::create_dir_all(&vendor).expect("vendor");
        for n in 0..20_000 {
            std::fs::write(vendor.join(format!("f{n:05}.js")), "x\n").expect("write");
        }
        std::fs::write(ws.join("tracked.txt"), "changed\n").expect("write");
        let before = git.calls();
        let changes = git.changes(ws).await.expect("changes");
        // One rev-parse for the top, one status and one numstat: none per file.
        assert_eq!(git.calls() - before, 3);
        assert_eq!(changes.info.files.len(), FILES_LIMIT);
        assert_eq!(changes.info.more, 20_001 - FILES_LIMIT as u64);
        assert_eq!(changes.listing.files.len(), FILES_LIMIT);
        let frame = changes_frame(1, &changes.info).to_string();
        assert!(frame.len() < MAX_MESSAGE, "{}", frame.len());

        // Long names fill the frame before the count does.
        let dir = committed("octet-git-long-names");
        let ws = dir.path();
        let deep = ws.join("d".repeat(200));
        std::fs::create_dir_all(&deep).expect("deep");
        for n in 0..600 {
            std::fs::write(deep.join(format!("{n:03}{}", "\"".repeat(200))), "x\n").expect("write");
        }
        let changes = git.changes(ws).await.expect("changes");
        let shown = changes.info.files.len() as u64;
        assert!(shown < 600, "{shown}");
        assert_eq!(shown + changes.info.more, 600);
        let frame = changes_frame(1, &changes.info).to_string();
        assert!(frame.len() < MAX_MESSAGE, "{}", frame.len());
    }

    #[tokio::test]
    async fn a_third_call_is_busy() {
        let git = Git::new();
        let one = git.begin().expect("a slot");
        let two = git.begin().expect("a second slot");
        assert_eq!(
            git.begin().map(|_| ()),
            Err((Code::Busy, GIT_BUSY.to_owned()))
        );
        drop(one);
        let three = git.begin().expect("a slot again");
        drop((two, three));
    }
}
