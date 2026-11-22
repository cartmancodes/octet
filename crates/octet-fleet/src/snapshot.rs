//! Git snapshots of the user's work tree.
//!
//! [`take`] captures the whole work tree as a git tree without touching the
//! user's index, `HEAD` or branches: it works on a copy of the index.
//! [`keep`] puts that tree under a ref so `git gc` leaves it alone and a scratch
//! clone can check it out; [`scratch_clone`] makes that clone for the steps
//! that must not write to the user's checkout, and [`tree_of`] snapshots it
//! again to detect a write. [`diff`] renders the change between two trees.
//!
//! Every git command here turns `core.fsmonitor`, `core.untrackedCache` and
//! `core.ignoreStat` off, so no stat cache can hide a change,
//! and runs without the `GIT_*` variables a hook may have exported, so the
//! repository is the one named by the working directory.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// The config every git command runs with: no cache may hide a change.
const CONFIG: [&str; 6] = [
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.untrackedCache=false",
    "-c",
    "core.ignoreStat=false",
];

/// Variables an outer git (a hook, a script) exports that would point git at
/// another repository or index than the working directory's.
const INHERITED: [&str; 7] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_PREFIX",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

/// The identity of a snapshot commit, so a user's missing identity cannot fail it.
const IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "octet-fleet"),
    ("GIT_COMMITTER_NAME", "octet-fleet"),
    ("GIT_AUTHOR_EMAIL", "octet-fleet@localhost"),
    ("GIT_COMMITTER_EMAIL", "octet-fleet@localhost"),
];

/// Why a snapshot step failed. The runner treats every one as a local failure.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// The directory is not inside a git work tree.
    #[error("{} is not inside a git work tree", .0.display())]
    NotARepository(PathBuf),
    /// The work tree is a sparse checkout, which snapshots cannot represent.
    #[error(
        "{} is a sparse checkout; clearing the skip-worktree flag would record the absent files \
         as deleted, so workflows refuse it",
        .0.display()
    )]
    SparseCheckout(PathBuf),
    /// A git command exited unsuccessfully.
    #[error("git {args} failed ({status}): {stderr}")]
    Git {
        /// The arguments after the fixed config, space separated.
        args: String,
        /// How the process ended.
        status: String,
        /// Its standard error, trimmed.
        stderr: String,
    },
    /// Git could not be run, or a file could not be copied.
    #[error("snapshot I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// A git work tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    /// The work tree's top level.
    pub top: PathBuf,
    /// The directory the caller was in, relative to `top` (empty at the top).
    pub cwd_rel: PathBuf,
    /// The absolute git directory (of the linked work tree, if it is one).
    pub git_dir: PathBuf,
}

/// The one place a git [`Command`] is built: runs in `dir`, with the fixed
/// config and without the inherited `GIT_*` variables.
fn command<I, S>(dir: &Path, args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new("git");
    cmd.args(CONFIG)
        .args(args)
        .current_dir(dir)
        .kill_on_drop(true);
    for name in INHERITED {
        cmd.env_remove(name);
    }
    cmd
}

/// A [`command`] that reads and writes the index at `index`.
fn on_index<I, S>(dir: &Path, index: &Path, args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = command(dir, args);
    cmd.env("GIT_INDEX_FILE", index);
    cmd
}

/// Runs `cmd` to completion, feeding it `input`; its standard output.
async fn exec(mut cmd: Command, input: &[u8]) -> Result<Vec<u8>, SnapshotError> {
    cmd.stdin(if input.is_empty() {
        Stdio::null()
    } else {
        Stdio::piped()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take();
    let feed = async {
        if let Some(stdin) = stdin.as_mut() {
            stdin.write_all(input).await?;
            stdin.shutdown().await?;
        }
        drop(stdin.take());
        Ok::<(), std::io::Error>(())
    };
    let (fed, out) = tokio::join!(feed, child.wait_with_output());
    let out = out?;
    if !out.status.success() {
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .skip(CONFIG.len())
            .map(|arg| arg.to_string_lossy())
            .collect();
        return Err(SnapshotError::Git {
            args: args.join(" "),
            status: out.status.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    // A pipe closed early is only an error if git then succeeded.
    fed?;
    Ok(out.stdout)
}

/// Runs git in `dir` and returns its standard output.
async fn git<I, S>(dir: &Path, args: I) -> Result<Vec<u8>, SnapshotError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    exec(command(dir, args), &[]).await
}

/// Standard output as one line of text, without its newline.
fn line(out: &[u8]) -> String {
    String::from_utf8_lossy(out)
        .trim_end_matches(['\n', '\r'])
        .to_string()
}

/// A path git printed.
fn path_of(out: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(out.strip_suffix(b"\n").unwrap_or(out)))
}

/// The top level of the work tree `dir` is in.
async fn toplevel(dir: &Path) -> Result<PathBuf, SnapshotError> {
    match git(dir, ["rev-parse", "--show-toplevel"]).await {
        Ok(out) => Ok(path_of(&out)),
        Err(SnapshotError::Git { .. }) => Err(SnapshotError::NotARepository(dir.to_path_buf())),
        Err(other) => Err(other),
    }
}

/// Opens the work tree `cwd` is in.
///
/// # Errors
///
/// [`SnapshotError::NotARepository`] when `cwd` is not in a work tree,
/// [`SnapshotError::SparseCheckout`] when it is a sparse checkout, and the
/// other variants when git fails.
pub async fn open_repo(cwd: &Path) -> Result<Repo, SnapshotError> {
    let top = toplevel(cwd).await?;
    let prefix = git(cwd, ["rev-parse", "--show-prefix"]).await?;
    // The prefix is relative to `top` and so does not depend on how the
    // caller spelled the directory (macOS `/var` is `/private/var`).
    let cwd_rel = path_of(&prefix).components().collect();
    let git_dir = path_of(&git(&top, ["rev-parse", "--absolute-git-dir"]).await?);
    let repo = Repo {
        top,
        cwd_rel,
        git_dir,
    };
    if is_sparse(&repo).await? {
        return Err(SnapshotError::SparseCheckout(repo.top));
    }
    Ok(repo)
}

/// Whether the work tree is sparse: the setting is on, or an index entry has
/// `skip-worktree` set (tag `S`).
async fn is_sparse(repo: &Repo) -> Result<bool, SnapshotError> {
    let setting = git(
        &repo.top,
        [
            "config",
            "--type=bool",
            "--default=false",
            "--get",
            "core.sparseCheckout",
        ],
    )
    .await?;
    if line(&setting) == "true" {
        return Ok(true);
    }
    let listed = git(&repo.top, ["ls-files", "-t", "-z"]).await?;
    Ok(entries(&listed).any(|(tag, _)| tag == b'S'))
}

/// The `(tag, path)` of each entry of `git ls-files -t -z` or `-v -z`.
fn entries(listed: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    listed
        .split(|byte| *byte == 0)
        .filter(|entry| entry.len() > 2)
        .map(|entry| (entry[0], &entry[2..]))
}

/// Captures the whole work tree as a git tree and returns its id.
///
/// The user's index, `HEAD` and refs are not touched: the work happens on a
/// copy of the index at `scratch_index`, which is overwritten.
///
/// # Errors
///
/// A [`SnapshotError`] when a git command fails or the index cannot be copied.
pub async fn take(repo: &Repo, scratch_index: &Path) -> Result<String, SnapshotError> {
    tree_in(&repo.top, scratch_index).await
}

/// Captures the work tree of the scratch clone at `dir` (the clone's top level
/// or a directory inside it), as [`take`] does.
///
/// # Errors
///
/// A [`SnapshotError`] when `dir` is not in a work tree, a git command fails,
/// or the index cannot be copied.
pub async fn tree_of(dir: &Path, scratch_index: &Path) -> Result<String, SnapshotError> {
    tree_in(&toplevel(dir).await?, scratch_index).await
}

/// The four steps of a snapshot, in the work tree whose top level is `top`.
async fn tree_in(top: &Path, scratch_index: &Path) -> Result<String, SnapshotError> {
    let scratch = std::path::absolute(scratch_index)?;
    copy_index(top, &scratch).await?;
    // A flagged file's live contents must be read, not its indexed ones.
    let listed = on_index(top, &scratch, ["ls-files", "-v", "-z"]);
    let listed = exec(listed, &[]).await?;
    let mut flagged = Vec::new();
    for (tag, path) in entries(&listed) {
        if tag.is_ascii_lowercase() {
            flagged.extend_from_slice(path);
            flagged.push(0);
        }
    }
    if !flagged.is_empty() {
        let clear = on_index(
            top,
            &scratch,
            ["update-index", "--no-assume-unchanged", "-z", "--stdin"],
        );
        exec(clear, &flagged).await?;
    }
    // From the top level, so the whole work tree and not `--cwd`.
    exec(on_index(top, &scratch, ["add", "-A"]), &[]).await?;
    let tree = exec(on_index(top, &scratch, ["write-tree"]), &[]).await?;
    Ok(line(&tree))
}

/// Copies the real index of the work tree at `top` to `scratch`; a repository
/// with no index yet starts from nothing. The copy keeps the original's
/// modification time, so git's racy-timestamp check treats entries as it
/// would in the original.
async fn copy_index(top: &Path, scratch: &Path) -> Result<(), SnapshotError> {
    let named = git(top, ["rev-parse", "--git-path", "index"]).await?;
    let real = top.join(path_of(&named));
    if let Some(parent) = scratch.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    match tokio::fs::metadata(&real).await {
        Ok(meta) => {
            tokio::fs::copy(&real, scratch).await?;
            let modified = meta.modified()?;
            let scratch = scratch.to_path_buf();
            tokio::task::spawn_blocking(move || {
                std::fs::File::options()
                    .write(true)
                    .open(scratch)?
                    .set_modified(modified)
            })
            .await
            .map_err(std::io::Error::other)??;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            match tokio::fs::remove_file(scratch).await {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err.into()),
                _ => {}
            }
        }
        Err(err) => return Err(err.into()),
    }
    Ok(())
}

/// Puts `tree` under `refs/octet-fleet/<run>/<seq>`, through a parentless
/// commit, so it survives `git gc` and a scratch clone can check it out.
/// Returns the commit id.
///
/// # Errors
///
/// A [`SnapshotError`] when git cannot make the commit or the ref.
pub async fn keep(repo: &Repo, run: &str, seq: u32, tree: &str) -> Result<String, SnapshotError> {
    let message = format!("octet-fleet {run} {seq}");
    let mut commit = command(
        &repo.top,
        ["commit-tree", tree, "--no-gpg-sign", "-m", &message],
    );
    commit.envs(IDENTITY);
    let commit = line(&exec(commit, &[]).await?);
    let name = format!("refs/octet-fleet/{run}/{seq}");
    git(&repo.top, ["update-ref", &name, &commit]).await?;
    Ok(commit)
}

/// The patch from tree or commit `from` to `to`: binary-safe, or a `--stat`
/// summary when `stat` is set.
///
/// # Errors
///
/// A [`SnapshotError`] when git cannot diff the two.
pub async fn diff(repo: &Repo, from: &str, to: &str, stat: bool) -> Result<String, SnapshotError> {
    let format = if stat { "--stat" } else { "--binary" };
    let args = [
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        format,
        from,
        to,
    ];
    let out = git(&repo.top, args).await?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Clones the repository to `into` without a checkout, then checks out
/// `commit` detached. Returns the directory in the clone at the same relative
/// path as the repository's `cwd_rel`.
///
/// The clone shares the repository's objects, so `commit` (kept under a ref
/// there) arrives with it; refs are not cloned, and the user's hooks do not run.
///
/// # Errors
///
/// A [`SnapshotError`] when git cannot clone or check out.
pub async fn scratch_clone(
    repo: &Repo,
    commit: &str,
    into: &Path,
) -> Result<PathBuf, SnapshotError> {
    let target = std::path::absolute(into)?;
    let no_hooks = ["-c", "core.hooksPath=/dev/null"];
    let mut clone = command(
        &repo.top,
        no_hooks
            .into_iter()
            .chain(["clone", "--quiet", "--shared", "--no-checkout"]),
    );
    clone.arg(&repo.top).arg(&target);
    exec(clone, &[]).await?;
    let checkout = ["checkout", "--quiet", "--detach", commit, "--"];
    git(&target, no_hooks.into_iter().chain(checkout)).await?;
    let mut cwd = into.to_path_buf();
    if !repo.cwd_rel.as_os_str().is_empty() {
        cwd.push(&repo.cwd_rel);
    }
    // A directory with nothing tracked in it is not in the checkout.
    tokio::fs::create_dir_all(&cwd).await?;
    Ok(cwd)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use octet_testkit::TempDir;

    use super::*;

    /// Runs git in `dir` for a test, as a user would: no inherited `GIT_*`.
    fn sh(dir: &Path, args: &[&str]) -> String {
        let mut cmd = std::process::Command::new("git");
        cmd.args(args).current_dir(dir);
        for name in INHERITED {
            cmd.env_remove(name);
        }
        let out = cmd.output().expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .trim_end_matches('\n')
            .to_string()
    }

    /// A fresh repository with a local identity and no signing.
    fn init() -> TempDir {
        let dir = TempDir::new("octet-fleet-snapshot");
        fs::create_dir_all(dir.path()).expect("repo dir");
        sh(dir.path(), &["init", "--quiet"]);
        sh(dir.path(), &["config", "user.name", "Tester"]);
        sh(dir.path(), &["config", "user.email", "tester@example.com"]);
        sh(dir.path(), &["config", "commit.gpgsign", "false"]);
        dir
    }

    fn write(dir: &Path, name: &str, text: &str) {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().expect("a parent")).expect("parent dir");
        fs::write(path, text).expect("file");
    }

    fn commit_all(dir: &Path) {
        sh(dir, &["add", "-A"]);
        sh(dir, &["commit", "--quiet", "-m", "base"]);
    }

    /// The paths in `tree`, sorted, newline-safe.
    fn paths(dir: &Path, tree: &str) -> Vec<String> {
        let out = sh(dir, &["ls-tree", "-r", "-z", "--name-only", tree]);
        let mut paths: Vec<_> = out
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect();
        paths.sort();
        paths
    }

    fn show(dir: &Path, tree: &str, path: &str) -> String {
        sh(dir, &["show", &format!("{tree}:{path}")])
    }

    /// Opens `dir` and takes a snapshot, with the scratch index beside it.
    async fn snap(dir: &TempDir, scratch: &TempDir) -> (Repo, String) {
        let repo = open_repo(dir.path()).await.expect("opens");
        let tree = take(&repo, &scratch.path().join("index.tmp"))
            .await
            .expect("takes");
        (repo, tree)
    }

    #[tokio::test]
    async fn tracked_ignored_files_are_kept() {
        let dir = init();
        write(dir.path(), ".gitignore", "*.log\n");
        write(dir.path(), "a.log", "tracked\n");
        write(dir.path(), "src.txt", "x\n");
        commit_all(dir.path());
        sh(dir.path(), &["add", "-f", "a.log"]);
        sh(
            dir.path(),
            &["commit", "--quiet", "-m", "log", "--allow-empty"],
        );
        write(dir.path(), "a.log", "tracked, edited\n");
        write(dir.path(), "b.log", "ignored and untracked\n");
        let scratch = TempDir::new("octet-fleet-scratch");
        let (_, tree) = snap(&dir, &scratch).await;
        assert_eq!(paths(dir.path(), &tree), [".gitignore", "a.log", "src.txt"]);
        assert_eq!(show(dir.path(), &tree, "a.log"), "tracked, edited");
    }

    #[tokio::test]
    async fn staged_additions_are_kept() {
        let dir = init();
        write(dir.path(), "base.txt", "base\n");
        commit_all(dir.path());
        write(dir.path(), "staged.txt", "staged\n");
        sh(dir.path(), &["add", "staged.txt"]);
        let scratch = TempDir::new("octet-fleet-scratch");
        let (_, tree) = snap(&dir, &scratch).await;
        assert_eq!(paths(dir.path(), &tree), ["base.txt", "staged.txt"]);
    }

    #[tokio::test]
    async fn assume_unchanged_edit_is_captured() {
        let dir = init();
        write(dir.path(), "f.txt", "v1\n");
        write(dir.path(), "g.txt", "same\n");
        commit_all(dir.path());
        sh(dir.path(), &["update-index", "--assume-unchanged", "f.txt"]);
        write(dir.path(), "f.txt", "v2\n");
        // Git itself no longer sees the edit, which is what the test is about.
        assert_eq!(sh(dir.path(), &["status", "--porcelain"]), "");
        let scratch = TempDir::new("octet-fleet-scratch");
        let (_, tree) = snap(&dir, &scratch).await;
        assert_eq!(show(dir.path(), &tree, "f.txt"), "v2");
        assert_eq!(show(dir.path(), &tree, "g.txt"), "same");
        // The user's flag is still set: only the copy was cleared.
        assert!(sh(dir.path(), &["ls-files", "-v"]).starts_with("h f.txt"));
    }

    #[tokio::test]
    async fn a_sparse_checkout_is_refused() {
        let dir = init();
        write(dir.path(), "a/x.txt", "x\n");
        write(dir.path(), "b/y.txt", "y\n");
        commit_all(dir.path());
        open_repo(dir.path()).await.expect("a full checkout opens");
        sh(dir.path(), &["sparse-checkout", "set", "--cone", "a"]);
        let err = open_repo(dir.path()).await.expect_err("refused");
        assert!(matches!(err, SnapshotError::SparseCheckout(_)), "{err}");

        // The skip-worktree flag alone is enough.
        sh(dir.path(), &["sparse-checkout", "disable"]);
        sh(dir.path(), &["config", "core.sparseCheckout", "false"]);
        open_repo(dir.path()).await.expect("opens again");
        sh(dir.path(), &["update-index", "--skip-worktree", "b/y.txt"]);
        let err = open_repo(dir.path()).await.expect_err("refused");
        assert!(matches!(err, SnapshotError::SparseCheckout(_)), "{err}");
    }

    #[tokio::test]
    async fn a_directory_outside_a_repository_is_refused() {
        let dir = TempDir::new("octet-fleet-plain");
        fs::create_dir_all(dir.path()).expect("dir");
        let err = open_repo(dir.path()).await.expect_err("refused");
        assert!(matches!(err, SnapshotError::NotARepository(_)), "{err}");
    }

    #[tokio::test]
    async fn untracked_and_deleted() {
        let dir = init();
        write(dir.path(), "a.txt", "a\n");
        write(dir.path(), "b.txt", "b\n");
        commit_all(dir.path());
        fs::remove_file(dir.path().join("a.txt")).expect("remove");
        write(dir.path(), "b.txt", "b2\n");
        write(dir.path(), "new/c.txt", "c\n");
        let scratch = TempDir::new("octet-fleet-scratch");
        let (_, tree) = snap(&dir, &scratch).await;
        assert_eq!(paths(dir.path(), &tree), ["b.txt", "new/c.txt"]);
        assert_eq!(show(dir.path(), &tree, "b.txt"), "b2");
    }

    #[tokio::test]
    async fn a_fresh_repository_has_no_index_to_copy() {
        let dir = init();
        write(dir.path(), "first.txt", "1\n");
        let scratch = TempDir::new("octet-fleet-scratch");
        fs::create_dir_all(scratch.path()).expect("dir");
        // A stale scratch index from an earlier run must not leak in.
        fs::write(scratch.path().join("index.tmp"), "junk").expect("stale");
        assert!(!dir.path().join(".git/index").exists());
        let (repo, tree) = snap(&dir, &scratch).await;
        assert_eq!(paths(dir.path(), &tree), ["first.txt"]);
        assert!(!dir.path().join(".git/index").exists());

        // And a commit-less repository still clones.
        let commit = keep(&repo, "r", 1, &tree).await.expect("kept");
        let into = TempDir::new("octet-fleet-clone");
        let cwd = scratch_clone(&repo, &commit, into.path())
            .await
            .expect("clones");
        assert_eq!(
            fs::read_to_string(cwd.join("first.txt")).expect("file"),
            "1\n"
        );
    }

    #[tokio::test]
    async fn the_users_index_and_head_are_untouched() {
        let dir = init();
        write(dir.path(), "a.txt", "a\n");
        write(dir.path(), "b.txt", "b\n");
        commit_all(dir.path());
        sh(dir.path(), &["update-index", "--assume-unchanged", "b.txt"]);
        write(dir.path(), "a.txt", "a2\n");
        write(dir.path(), "b.txt", "b2\n");
        write(dir.path(), "u.txt", "untracked\n");
        let git = dir.path().join(".git");
        let branch = sh(dir.path(), &["symbolic-ref", "HEAD"]);
        let refs = |dir: &Path| sh(dir, &["for-each-ref", "--format=%(refname) %(objectname)"]);
        let before = (
            fs::read(git.join("index")).expect("index"),
            fs::read(git.join("HEAD")).expect("HEAD"),
            refs(dir.path()),
            sh(dir.path(), &["rev-parse", &branch]),
            sh(dir.path(), &["status", "--porcelain=2"]),
        );
        let scratch = TempDir::new("octet-fleet-scratch");
        let (repo, tree) = snap(&dir, &scratch).await;
        keep(&repo, "run", 1, &tree).await.expect("kept");
        let after_refs = refs(dir.path());
        let user_refs: Vec<_> = after_refs
            .lines()
            .filter(|l| !l.contains("octet-fleet"))
            .collect();
        assert_eq!(user_refs.join("\n"), before.2);
        assert_eq!(
            fs::read(git.join("index")).expect("index"),
            before.0,
            "index"
        );
        assert_eq!(fs::read(git.join("HEAD")).expect("HEAD"), before.1, "HEAD");
        assert_eq!(sh(dir.path(), &["rev-parse", &branch]), before.3, "branch");
        assert_eq!(
            sh(dir.path(), &["status", "--porcelain=2"]),
            before.4,
            "status"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("b.txt")).expect("file"),
            "b2\n"
        );
    }

    #[tokio::test]
    async fn cwd_below_the_top_level() {
        let dir = init();
        write(dir.path(), "top.txt", "t\n");
        write(dir.path(), "sub/deep/in.txt", "i\n");
        commit_all(dir.path());
        write(dir.path(), "outside.txt", "o\n");
        let deep = dir.path().join("sub/deep");
        let repo = open_repo(&deep).await.expect("opens");
        let top = fs::canonicalize(dir.path()).expect("canonical");
        assert_eq!(repo.top, top);
        assert_eq!(repo.cwd_rel, Path::new("sub/deep"));
        assert_eq!(repo.git_dir, top.join(".git"));
        let scratch = TempDir::new("octet-fleet-scratch");
        let tree = take(&repo, &scratch.path().join("index.tmp"))
            .await
            .expect("takes");
        // The whole work tree, not the directory the caller was in.
        assert_eq!(
            paths(dir.path(), &tree),
            ["outside.txt", "sub/deep/in.txt", "top.txt"]
        );

        let commit = keep(&repo, "run", 1, &tree).await.expect("kept");
        let into = TempDir::new("octet-fleet-clone");
        let cwd = scratch_clone(&repo, &commit, into.path())
            .await
            .expect("clones");
        assert_eq!(cwd, into.path().join("sub/deep"));
        assert_eq!(fs::read_to_string(cwd.join("in.txt")).expect("file"), "i\n");
    }

    #[tokio::test]
    async fn refs_keep_trees() {
        let dir = init();
        write(dir.path(), "a.txt", "a\n");
        commit_all(dir.path());
        write(dir.path(), "kept.txt", "kept\n");
        let scratch = TempDir::new("octet-fleet-scratch");
        let (repo, tree) = snap(&dir, &scratch).await;
        let commit = keep(&repo, "run-7", 3, &tree).await.expect("kept");
        let name = "refs/octet-fleet/run-7/3";
        assert_eq!(sh(dir.path(), &["cat-file", "-t", &commit]), "commit");
        assert_eq!(sh(dir.path(), &["rev-parse", name]), commit);
        assert_eq!(
            sh(dir.path(), &["rev-parse", &format!("{name}^{{tree}}")]),
            tree
        );
        assert_eq!(
            sh(dir.path(), &["log", "-1", "--format=%an <%ae> %s", name]),
            "octet-fleet <octet-fleet@localhost> octet-fleet run-7 3"
        );
        assert_eq!(
            sh(dir.path(), &["rev-list", "--parents", "-1", name]),
            commit,
            "no parent"
        );
        // Unreachable but for the ref: gc would prune it without one.
        sh(dir.path(), &["gc", "--quiet", "--prune=now"]);
        assert_eq!(sh(dir.path(), &["cat-file", "-t", &tree]), "tree");
        assert_eq!(show(dir.path(), &tree, "kept.txt"), "kept");
    }

    #[tokio::test]
    async fn a_signing_user_does_not_fail_a_snapshot() {
        let dir = init();
        write(dir.path(), "a.txt", "a\n");
        sh(dir.path(), &["config", "commit.gpgsign", "true"]);
        sh(dir.path(), &["config", "gpg.program", "/nonexistent/gpg"]);
        sh(dir.path(), &["config", "--unset", "user.name"]);
        sh(dir.path(), &["config", "--unset", "user.email"]);
        let scratch = TempDir::new("octet-fleet-scratch");
        let (repo, tree) = snap(&dir, &scratch).await;
        keep(&repo, "run", 1, &tree)
            .await
            .expect("kept without signing or identity");
    }

    #[tokio::test]
    async fn a_scratch_clone_checks_out_the_candidate() {
        let dir = init();
        write(dir.path(), "a.txt", "a\n");
        commit_all(dir.path());
        let head = sh(dir.path(), &["rev-parse", "HEAD"]);
        write(dir.path(), "a.txt", "candidate\n");
        write(dir.path(), "extra.txt", "untracked\n");
        let scratch = TempDir::new("octet-fleet-scratch");
        let (repo, tree) = snap(&dir, &scratch).await;
        let commit = keep(&repo, "run", 1, &tree).await.expect("kept");
        let into = TempDir::new("octet-fleet-clone");
        let cwd = scratch_clone(&repo, &commit, &into.path().join("tree"))
            .await
            .expect("clones");
        assert_eq!(cwd, into.path().join("tree"));
        assert_eq!(
            fs::read_to_string(cwd.join("a.txt")).expect("file"),
            "candidate\n"
        );
        assert_eq!(
            fs::read_to_string(cwd.join("extra.txt")).expect("file"),
            "untracked\n"
        );
        assert_eq!(sh(&cwd, &["rev-parse", "HEAD"]), commit);
        assert!(
            !sh(&cwd, &["status", "--porcelain"]).contains("a.txt"),
            "clean checkout"
        );
        let detached = std::process::Command::new("git")
            .args(["symbolic-ref", "-q", "HEAD"])
            .current_dir(&cwd)
            .status()
            .expect("git runs");
        assert!(!detached.success(), "HEAD is detached");

        // The clone's tree is the candidate's, until something writes to it.
        let index = scratch.path().join("clone-index");
        assert_eq!(tree_of(&cwd, &index).await.expect("tree"), tree);
        write(&cwd, "a.txt", "tampered\n");
        assert_ne!(tree_of(&cwd, &index).await.expect("tree"), tree);
        // Nothing reached the user's checkout or refs.
        assert_eq!(sh(dir.path(), &["rev-parse", "HEAD"]), head);
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).expect("file"),
            "candidate\n"
        );
        assert!(sh(dir.path(), &["for-each-ref", "refs/heads"]).contains(&head));
    }

    #[tokio::test]
    async fn diff_shows_the_change_between_trees() {
        let dir = init();
        write(dir.path(), "a.txt", "one\n");
        fs::write(dir.path().join("bin.dat"), [0u8, 1, 2, 255]).expect("binary");
        let scratch = TempDir::new("octet-fleet-scratch");
        let (repo, from) = snap(&dir, &scratch).await;
        write(dir.path(), "a.txt", "two\n");
        fs::write(dir.path().join("bin.dat"), [0u8, 9, 9, 255]).expect("binary");
        let to = take(&repo, &scratch.path().join("index.tmp"))
            .await
            .expect("takes");
        let patch = diff(&repo, &from, &to, false).await.expect("diff");
        assert!(patch.contains("-one\n+two\n"), "{patch}");
        assert!(patch.contains("GIT binary patch"), "{patch}");
        let stat = diff(&repo, &from, &to, true).await.expect("stat");
        assert!(
            stat.contains("a.txt") && stat.contains("2 files changed"),
            "{stat}"
        );
        let err = diff(&repo, &from, "nonsense", false)
            .await
            .expect_err("fails");
        assert!(
            matches!(&err, SnapshotError::Git { stderr, .. } if !stderr.is_empty()),
            "{err}"
        );
    }

    #[tokio::test]
    async fn paths_with_spaces_and_unicode() {
        let dir = init();
        let names = [
            "a b.txt",
            "héllo wörld/ñ 日本.txt",
            "line\nbreak.txt",
            "-dash.txt",
            "tab\there.txt",
        ];
        for name in names {
            write(dir.path(), name, "v1\n");
        }
        commit_all(dir.path());
        sh(
            dir.path(),
            &[
                "update-index",
                "--assume-unchanged",
                "--",
                "line\nbreak.txt",
                "héllo wörld/ñ 日本.txt",
            ],
        );
        for name in names {
            write(dir.path(), name, "v2\n");
        }
        write(dir.path(), "new ünï.txt", "n\n");
        let scratch = TempDir::new("octet-fleet-scratch");
        let (_, tree) = snap(&dir, &scratch).await;
        let mut expected: Vec<String> = names.iter().map(ToString::to_string).collect();
        expected.push("new ünï.txt".into());
        expected.sort();
        assert_eq!(paths(dir.path(), &tree), expected);
        for name in names {
            assert_eq!(show(dir.path(), &tree, name), "v2", "{name:?}");
        }
    }
}
