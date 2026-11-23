//! Octet's evidence runner: executes a candidate's test commands held, observes
//! the work tree before and after each, and records both observations durably.
//!
//! **The guarantee: for every command, the tree Octet observed immediately
//! before releasing it and immediately after it exited equals the candidate's
//! tree, and both observations are persisted in the evidence record.** It cannot
//! see a change made and undone within a single command's lifetime.
//!
//! [`run`] makes one pass over the command list. Each command is spawned held
//! (`sh -c 'read _ || exit 125; exec sh -c "$1"'`, leading its own process
//! group), so nothing of it runs until its identity is in the log: the runner
//! reads the identity, observes the tree, appends `evidence_command`, and only
//! then writes the go line. On the first observation that is not the
//! candidate's tree the pass stops and the caller starts the next one (see
//! [`EvidenceOutcome::Changed`]).

use std::io::SeekFrom;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use octet_proc::GroupLeader;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::process::Command;

use crate::contract::CommandSummary;
use crate::ownership::start_time;
use crate::record::{AttemptId, CandidateId, Identity, Record};
use crate::snapshot::{Repo, SnapshotError, take};
use crate::store::{Log, StoreError};

/// How many restarts a tests attempt may make: a third change is unstable.
const MAX_RESTARTS: u32 = 2;

/// How much of a command's output is kept.
const TAIL_BYTES: usize = 16 * 1024;

/// The held command: waits for the go line, then becomes the real command.
const HELD: &str = r#"read _ || exit 125; exec sh -c "$1""#;

/// Why a pass could not be completed.
#[derive(Debug, thiserror::Error)]
pub enum EvidenceError {
    /// A command could not be spawned, released or waited for, or its output
    /// could not be kept.
    #[error("evidence I/O: {0}")]
    Io(#[from] std::io::Error),
    /// The tree could not be observed.
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    /// A record could not be made durable.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The held command's start time could not be read, so it was killed
    /// unreleased: nothing is released that is not registered.
    #[error("the start time of held process {0} is unreadable")]
    Identity(u32),
}

/// One pass of the evidence runner.
#[derive(Debug)]
pub struct EvidenceRun<'a> {
    /// The work tree the commands run in.
    pub repo: &'a Repo,
    /// The run's log, which receives the evidence records.
    pub log: &'a mut Log,
    /// The tests attempt.
    pub attempt: AttemptId,
    /// The candidate under test; the caller's bookkeeping, not recorded here.
    pub candidate: CandidateId,
    /// The candidate's tree: what every observation must equal.
    pub tree: String,
    /// Which pass this is: 0, then 1 and 2 after a change.
    pub restart: u32,
    /// The commands, each one `sh` command line.
    pub commands: &'a [String],
    /// How long one command may run before its process group is killed.
    pub timeout: Duration,
    /// The value of `OCTET_FLEET_ATTEMPT` each command carries.
    pub marker: String,
    /// Tests only: after each held spawn create `held-<restart>-<index>` here
    /// and wait for `continue-<restart>-<index>` before observing.
    pub pause_hook: Option<PathBuf>,
    /// The attempt's directory: output goes to `evidence/<restart>-<index>.out`,
    /// the scratch index to `index.tmp`.
    pub attempt_dir: PathBuf,
}

/// What one command did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    /// The command line.
    pub command: String,
    /// Its exit code; none when a signal ended it or it timed out.
    pub exit: Option<i32>,
    /// Whether it was killed at its timeout.
    pub timed_out: bool,
    /// How long it ran, in milliseconds.
    pub duration_ms: u64,
    /// The tree before it was released.
    pub pre_tree: String,
    /// The tree after it exited.
    pub post_tree: String,
    /// The last 16 KiB of its combined output.
    pub tail: String,
}

impl From<&CommandResult> for CommandSummary {
    fn from(result: &CommandResult) -> Self {
        Self {
            command: result.command.clone(),
            exit: result.exit,
            timed_out: result.timed_out,
            pre_tree: result.pre_tree.clone(),
            post_tree: result.post_tree.clone(),
            tail: result.tail.clone(),
        }
    }
}

/// How a pass ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceOutcome {
    /// Every command ran and every observation was the candidate's tree.
    Complete {
        /// Whether every command exited 0 within its time.
        passed: bool,
        /// One result per command.
        results: Vec<CommandResult>,
    },
    /// An observation differed: the record of this pass is void. The caller
    /// proposes `tree` as a candidate and runs the whole list again with
    /// `restart`.
    Changed {
        /// The tree observed.
        tree: String,
        /// The restart count of the next pass.
        restart: u32,
    },
    /// An observation differed on the last allowed pass.
    Unstable,
}

/// The outcome of a pass that saw `observed` where `tree` belonged.
fn changed(observed: String, restart: u32) -> EvidenceOutcome {
    if restart < MAX_RESTARTS {
        EvidenceOutcome::Changed {
            tree: observed,
            restart: restart + 1,
        }
    } else {
        EvidenceOutcome::Unstable
    }
}

/// The last [`TAIL_BYTES`] of `bytes` as text, not starting mid-character.
fn tail_of(bytes: &[u8]) -> String {
    let mut tail = bytes;
    if tail.len() > TAIL_BYTES {
        tail = &tail[tail.len() - TAIL_BYTES..];
        // A UTF-8 continuation byte (10xxxxxx) belongs to a character that
        // began before the cut.
        let skip = tail.iter().take_while(|byte| **byte & 0xC0 == 0x80).count();
        tail = &tail[skip..];
    }
    String::from_utf8_lossy(tail).into_owned()
}

/// The last 16 KiB of the file at `path`, as text.
async fn read_tail(path: &Path) -> std::io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let len = file.metadata().await?.len();
    let from = len.saturating_sub(TAIL_BYTES as u64);
    file.seek(SeekFrom::Start(from)).await?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES as u64).read_to_end(&mut bytes).await?;
    Ok(tail_of(&bytes))
}

/// How one command ended its part of the pass.
enum Step {
    /// It ran and every observation was the candidate's tree.
    Done(CommandResult),
    /// An observation was this tree, not the candidate's.
    Observed(String),
}

/// Kills `leader`'s group, then reaps the leader so no zombie is left.
async fn kill_and_reap(leader: &mut GroupLeader) {
    leader.kill_group();
    let _ = leader.child().wait().await;
}

/// Runs one pass of `run.commands` against `run.tree`.
///
/// Appends `EvidenceCommand` before each command is released and
/// `EvidenceResult` after each exits (the latter even when the tree it
/// observed is not `run.tree`, so the log shows what was seen). Stops at the
/// first observation that is not `run.tree`.
///
/// Cancelling it is safe: a command spawned but not yet released is killed with
/// its group, and never ran.
///
/// # Errors
///
/// [`EvidenceError`] when a command cannot be started, registered or waited
/// for, the tree cannot be observed, or a record cannot be appended.
pub async fn run(run: EvidenceRun<'_>) -> Result<EvidenceOutcome, EvidenceError> {
    let evidence_dir = run.attempt_dir.join("evidence");
    octet_store::create_private_dir(&evidence_dir).await?;
    let mut results = Vec::with_capacity(run.commands.len());
    let EvidenceRun {
        repo,
        log,
        attempt,
        tree,
        restart,
        commands,
        timeout,
        marker,
        pause_hook,
        attempt_dir,
        candidate: _,
    } = run;
    let pass = Pass {
        repo,
        attempt,
        tree: &tree,
        restart,
        timeout,
        marker: &marker,
        pause_hook: pause_hook.as_deref(),
        evidence_dir: &evidence_dir,
        scratch_index: attempt_dir.join("index.tmp"),
    };
    for (index, command) in commands.iter().enumerate() {
        match pass.command(log, index, command).await? {
            Step::Done(result) => results.push(result),
            Step::Observed(observed) => return Ok(changed(observed, restart)),
        }
    }
    let passed = results
        .iter()
        .all(|result| result.exit == Some(0) && !result.timed_out);
    Ok(EvidenceOutcome::Complete { passed, results })
}

/// What one pass shares between its commands.
struct Pass<'a> {
    repo: &'a Repo,
    attempt: AttemptId,
    tree: &'a str,
    restart: u32,
    timeout: Duration,
    marker: &'a str,
    pause_hook: Option<&'a Path>,
    evidence_dir: &'a Path,
    scratch_index: PathBuf,
}

impl Pass<'_> {
    /// Spawns `command` held, registers it, releases it, and observes the tree
    /// on both sides.
    async fn command(
        &self,
        log: &mut Log,
        index: usize,
        command: &str,
    ) -> Result<Step, EvidenceError> {
        let output = self
            .evidence_dir
            .join(format!("{}-{index}.out", self.restart));
        let mut leader = self.spawn_held(command, &output)?;
        let pid = leader.child().id().unwrap_or_default();
        // A held command that cannot be registered is never released.
        let Some(start) = start_time(pid).await.filter(|_| pid != 0) else {
            kill_and_reap(&mut leader).await;
            return Err(EvidenceError::Identity(pid));
        };
        let mut stdin = leader.child().stdin.take();
        if let Some(dir) = self.pause_hook {
            pause(dir, self.restart, index).await?;
        }
        let pre_tree = take(self.repo, &self.scratch_index).await?;
        if pre_tree != self.tree {
            drop(stdin);
            kill_and_reap(&mut leader).await;
            return Ok(Step::Observed(pre_tree));
        }
        log.append(&Record::EvidenceCommand {
            attempt: self.attempt,
            restart: self.restart,
            index,
            command: command.to_owned(),
            identity: Identity { pid, start },
            pre_tree: pre_tree.clone(),
        })
        .await?;
        // Release: the go line, then end of file.
        let started = Instant::now();
        if let Some(mut pipe) = stdin.take() {
            pipe.write_all(b"go\n").await?;
            pipe.shutdown().await?;
        }
        let in_time = leader.exited_within(self.timeout).await;
        // Kill in both cases: a member left behind by a command that finished
        // must not edit the tree after the post observation. The leader is
        // still unreaped, so its group id still names this group.
        leader.kill_group();
        let status = leader.child().wait().await?;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let timed_out = !in_time;
        let exit = if timed_out { None } else { status.code() };
        let tail = read_tail(&output).await?;
        let post_tree = take(self.repo, &self.scratch_index).await?;
        log.append(&Record::EvidenceResult {
            attempt: self.attempt,
            restart: self.restart,
            index,
            exit,
            timed_out,
            duration_ms,
            post_tree: post_tree.clone(),
        })
        .await?;
        if post_tree != self.tree {
            return Ok(Step::Observed(post_tree));
        }
        Ok(Step::Done(CommandResult {
            command: command.to_owned(),
            exit,
            timed_out,
            duration_ms,
            pre_tree,
            post_tree,
            tail,
        }))
    }

    /// Spawns the held shell for `command`, its output going to `output`.
    fn spawn_held(&self, command: &str, output: &Path) -> Result<GroupLeader, EvidenceError> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(output)?;
        let errors = file.try_clone()?;
        let child = Command::new("sh")
            .args(["-c", HELD, "sh", command])
            .process_group(0)
            .current_dir(self.repo.top.join(&self.repo.cwd_rel))
            .env("OCTET_FLEET_ATTEMPT", self.marker)
            .stdin(Stdio::piped())
            .stdout(file)
            .stderr(errors)
            .spawn()?;
        Ok(GroupLeader::new(child))
    }
}

/// The test pause: announces that command `index` is held and waits to be told
/// to go on.
async fn pause(dir: &Path, restart: u32, index: usize) -> std::io::Result<()> {
    tokio::fs::write(dir.join(format!("held-{restart}-{index}")), b"").await?;
    let go = dir.join(format!("continue-{restart}-{index}"));
    while !tokio::fs::try_exists(&go).await? {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use octet_testkit::{TempDir, wait_until};

    use super::*;
    use crate::ownership::list_processes;
    use crate::record::Record;
    use crate::snapshot::open_repo;
    use crate::store::{self, RunDir, RunFile};

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

    /// How long a test waits for something that should happen at once.
    const PATIENCE: Duration = Duration::from_secs(60);

    fn git(dir: &Path, args: &[&str]) {
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
    }

    /// A committed repository, a run to log into, and a place outside the
    /// repository for commands to leave marks.
    struct Fixture {
        _dirs: [TempDir; 3],
        top: PathBuf,
        side: PathBuf,
        run_dir: RunDir,
        attempt_dir: PathBuf,
        repo: Repo,
        log: Log,
        tree: String,
    }

    impl Fixture {
        async fn new() -> Self {
            let repo_dir = TempDir::new("octet-fleet-evidence-repo");
            let side_dir = TempDir::new("octet-fleet-evidence-side");
            let data_dir = TempDir::new("octet-fleet-evidence-data");
            let top = repo_dir.path().to_path_buf();
            let side = side_dir.path().to_path_buf();
            fs::create_dir_all(&top).expect("repo dir");
            fs::create_dir_all(&side).expect("side dir");
            git(&top, &["init", "--quiet"]);
            git(&top, &["config", "user.name", "Tester"]);
            git(&top, &["config", "user.email", "tester@example.com"]);
            git(&top, &["config", "commit.gpgsign", "false"]);
            fs::write(top.join("tracked.txt"), "one\n").expect("file");
            git(&top, &["add", "-A"]);
            git(&top, &["commit", "--quiet", "-m", "base"]);
            let repo = open_repo(&top).await.expect("repo");
            let file = RunFile {
                workflow: crate::sim::shipped(),
                hash: "0123456789abcdef".into(),
                task: "task".into(),
                top: repo.top.clone(),
                cwd_rel: PathBuf::new(),
                base: CandidateId(0),
                base_tree: "4b825dc642cb6eb9a060e54bf8d69288fbee4904".into(),
                octet_version: "0.0.0-test".into(),
            };
            let run_dir = store::create(data_dir.path(), "f0000000001-0001", &file)
                .await
                .expect("run");
            let attempt_dir = store::attempt_dir(&run_dir, AttemptId(0));
            fs::create_dir_all(&attempt_dir).expect("attempt dir");
            let (log, _) = Log::open(&run_dir).await.expect("log");
            let tree = take(&repo, &attempt_dir.join("index.tmp"))
                .await
                .expect("tree");
            Self {
                _dirs: [repo_dir, side_dir, data_dir],
                top,
                side,
                run_dir,
                attempt_dir,
                repo,
                log,
                tree,
            }
        }

        /// A pass over `commands` against `tree` at `restart`.
        async fn pass(
            &mut self,
            commands: &[String],
            tree: &str,
            restart: u32,
            timeout: Duration,
            pause_hook: Option<PathBuf>,
        ) -> EvidenceOutcome {
            run(EvidenceRun {
                repo: &self.repo,
                log: &mut self.log,
                attempt: AttemptId(0),
                candidate: CandidateId(0),
                tree: tree.to_owned(),
                restart,
                commands,
                timeout,
                marker: "f0000000001-0001/0".into(),
                pause_hook,
                attempt_dir: self.attempt_dir.clone(),
            })
            .await
            .expect("the pass runs")
        }

        /// A pass with the base tree, no pause, a generous timeout.
        async fn plain(&mut self, commands: &[String]) -> EvidenceOutcome {
            let tree = self.tree.clone();
            self.pass(commands, &tree, 0, Duration::from_secs(60), None)
                .await
        }

        async fn records(&self) -> Vec<Record> {
            Log::open(&self.run_dir).await.expect("log").1
        }

        fn side(&self, name: &str) -> PathBuf {
            self.side.join(name)
        }
    }

    fn cmds(commands: &[&str]) -> Vec<String> {
        commands.iter().map(|c| (*c).to_owned()).collect()
    }

    fn complete(outcome: EvidenceOutcome) -> (bool, Vec<CommandResult>) {
        match outcome {
            EvidenceOutcome::Complete { passed, results } => (passed, results),
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn all_pass_on_an_unchanged_tree() {
        let mut f = Fixture::new().await;
        let outcome = f.plain(&cmds(&["echo one", "echo two >&2", "true"])).await;
        let (passed, results) = complete(outcome);
        assert!(passed);
        assert_eq!(results.len(), 3);
        for result in &results {
            assert_eq!(result.exit, Some(0));
            assert!(!result.timed_out);
            assert_eq!(result.pre_tree, f.tree);
            assert_eq!(result.post_tree, f.tree);
        }
        assert_eq!(results[0].tail, "one\n");
        assert_eq!(results[1].tail, "two\n", "stderr is captured too");
        // The log holds a command and a result per command, in order, with
        // the identity and both observations.
        let records = f.records().await;
        assert_eq!(records.len(), 6);
        for (index, pair) in records.chunks(2).enumerate() {
            let Record::EvidenceCommand {
                restart,
                index: at,
                command,
                identity,
                pre_tree,
                ..
            } = &pair[0]
            else {
                panic!("expected a command record, got {:?}", pair[0]);
            };
            assert_eq!((*restart, *at), (0, index));
            assert_eq!(command, &results[index].command);
            assert!(identity.pid > 1 && !identity.start.is_empty());
            assert_eq!(pre_tree, &f.tree);
            let Record::EvidenceResult {
                index: at,
                exit,
                timed_out,
                post_tree,
                ..
            } = &pair[1]
            else {
                panic!("expected a result record, got {:?}", pair[1]);
            };
            assert_eq!((*at, *exit, *timed_out), (index, Some(0), false));
            assert_eq!(post_tree, &f.tree);
        }
        // Output files are owner-only.
        let out = f.attempt_dir.join("evidence").join("0-0.out");
        assert_eq!(fs::read_to_string(&out).expect("output"), "one\n");
        assert_eq!(
            fs::metadata(&out).expect("meta").permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn commands_run_in_the_directory_the_run_started_in() {
        let mut f = Fixture::new().await;
        fs::create_dir_all(f.top.join("sub")).expect("sub");
        fs::write(f.top.join("sub").join("keep"), "x").expect("file");
        git(&f.top, &["add", "-A"]);
        git(&f.top, &["commit", "--quiet", "-m", "sub"]);
        f.repo = open_repo(&f.top.join("sub")).await.expect("repo");
        f.tree = take(&f.repo, &f.attempt_dir.join("index.tmp"))
            .await
            .expect("tree");
        let (passed, results) = complete(f.plain(&cmds(&["ls"])).await);
        assert!(passed);
        assert_eq!(results[0].tail, "keep\n");
    }

    #[tokio::test]
    async fn the_marker_is_in_the_commands_environment() {
        let mut f = Fixture::new().await;
        let (_, results) = complete(f.plain(&cmds(&["echo $OCTET_FLEET_ATTEMPT"])).await);
        assert_eq!(results[0].tail, "f0000000001-0001/0\n");
    }

    #[tokio::test]
    async fn a_failure_is_tests_failed() {
        let mut f = Fixture::new().await;
        let (passed, results) =
            complete(f.plain(&cmds(&["true", "echo bad; exit 3", "true"])).await);
        assert!(!passed);
        assert_eq!(results.len(), 3, "a failure does not stop the list");
        assert_eq!(results[1].exit, Some(3));
        assert_eq!(results[1].tail, "bad\n");
        assert!(!results[1].timed_out);
    }

    #[tokio::test]
    async fn a_signal_death_has_no_exit_code() {
        let mut f = Fixture::new().await;
        let (passed, results) = complete(f.plain(&cmds(&["kill -9 $$"])).await);
        assert!(!passed);
        assert_eq!(results[0].exit, None);
        assert!(!results[0].timed_out);
    }

    #[tokio::test]
    async fn a_change_after_a_restarts() {
        let mut f = Fixture::new().await;
        let outcome = f
            .plain(&cmds(&[
                "echo two >> tracked.txt",
                "echo never >> ../never",
            ]))
            .await;
        let EvidenceOutcome::Changed { tree, restart } = outcome else {
            panic!("expected Changed, got {outcome:?}");
        };
        assert_eq!(restart, 1);
        assert_ne!(tree, f.tree);
        // The observed tree is the one the caller proposes.
        let now = take(&f.repo, &f.attempt_dir.join("check.tmp"))
            .await
            .expect("tree");
        assert_eq!(tree, now);
        let records = f.records().await;
        assert_eq!(records.len(), 2, "only A was run and recorded");
        let Record::EvidenceResult { post_tree, .. } = &records[1] else {
            panic!("expected a result, got {:?}", records[1]);
        };
        assert_eq!(post_tree, &tree, "the log records what was observed");
    }

    #[tokio::test]
    async fn change_use_restore_is_caught() {
        let mut f = Fixture::new().await;
        let used = f.side("used");
        let commands = cmds(&[
            "echo changed > tracked.txt",
            &format!("cat tracked.txt > '{}'", used.display()),
            "echo one > tracked.txt",
        ]);
        let outcome = f.plain(&commands).await;
        assert!(
            matches!(outcome, EvidenceOutcome::Changed { restart: 1, .. }),
            "{outcome:?}"
        );
        assert!(!used.exists(), "B never ran against the changed tree");
        assert_eq!(f.records().await.len(), 2);
    }

    #[tokio::test]
    async fn an_edit_before_release_is_caught_before_b_runs() {
        let mut f = Fixture::new().await;
        let pause = f.side("pause");
        fs::create_dir_all(&pause).expect("pause dir");
        let (a, b) = (f.side("a"), f.side("b"));
        let commands = cmds(&[
            &format!("echo restart-0 >> '{}'", a.display()),
            &format!("echo restart-0 >> '{}'", b.display()),
        ]);
        let tracked = f.top.join("tracked.txt");
        let driver = {
            let pause = pause.clone();
            async move {
                wait_for(&pause.join("held-0-0")).await;
                fs::write(pause.join("continue-0-0"), "").expect("continue");
                wait_for(&pause.join("held-0-1")).await;
                // Someone edits the tree while B is held.
                fs::write(&tracked, "edited\n").expect("edit");
                fs::write(pause.join("continue-0-1"), "").expect("continue");
            }
        };
        let tree = f.tree.clone();
        let (outcome, ()) = tokio::join!(
            f.pass(
                &commands,
                &tree,
                0,
                Duration::from_secs(60),
                Some(pause.clone())
            ),
            driver
        );
        assert!(
            matches!(outcome, EvidenceOutcome::Changed { restart: 1, .. }),
            "{outcome:?}"
        );
        assert_eq!(fs::read_to_string(&a).expect("A ran"), "restart-0\n");
        assert!(!b.exists(), "B was killed unreleased");
        let records = f.records().await;
        assert_eq!(
            records.len(),
            2,
            "B has no record: its pre-observation differed"
        );
    }

    #[tokio::test]
    async fn three_changes_are_unstable() {
        let mut f = Fixture::new().await;
        let commands = cmds(&["echo more >> tracked.txt"]);
        let mut tree = f.tree.clone();
        for restart in 0..2 {
            let outcome = f
                .pass(&commands, &tree, restart, Duration::from_secs(60), None)
                .await;
            let EvidenceOutcome::Changed {
                tree: next,
                restart: next_restart,
            } = outcome
            else {
                panic!("expected Changed at restart {restart}, got {outcome:?}");
            };
            assert_eq!(next_restart, restart + 1);
            assert_ne!(next, tree);
            tree = next;
        }
        let outcome = f
            .pass(&commands, &tree, 2, Duration::from_secs(60), None)
            .await;
        assert_eq!(outcome, EvidenceOutcome::Unstable);
        let restarts: Vec<u32> = f
            .records()
            .await
            .iter()
            .filter_map(|r| match r {
                Record::EvidenceResult { restart, .. } => Some(*restart),
                _ => None,
            })
            .collect();
        assert_eq!(restarts, vec![0, 1, 2]);
    }

    /// The live processes whose command line mentions `token`.
    async fn with_token(token: &str) -> Vec<u32> {
        list_processes()
            .await
            .expect("processes")
            .into_iter()
            .filter(|p| p.command.contains(token))
            .map(|p| p.pid)
            .collect()
    }

    async fn gone(token: &str) -> bool {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if with_token(token).await.is_empty() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_for(path: &Path) {
        let path = path.to_path_buf();
        let seen = tokio::task::spawn_blocking(move || wait_until(PATIENCE, || path.exists()))
            .await
            .expect("join");
        assert!(seen, "timed out waiting for a file");
    }

    #[tokio::test]
    async fn a_hung_command_times_out_with_its_group() {
        let mut f = Fixture::new().await;
        let token = "sleep 31337";
        let commands = cmds(&[&format!("{token} & {token}")]);
        let tree = f.tree.clone();
        let watcher = async {
            // Both sleeps (and the shells around them) are alive while it hangs.
            let mut seen = 0;
            let deadline = Instant::now() + PATIENCE;
            while seen < 2 && Instant::now() < deadline {
                seen = with_token(token).await.len();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            seen
        };
        let started = Instant::now();
        let (outcome, seen) = tokio::join!(
            f.pass(&commands, &tree, 0, Duration::from_secs(1), None),
            watcher
        );
        assert!(seen >= 2, "the sleeps were running");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the timeout killed it"
        );
        let (passed, results) = complete(outcome);
        assert!(!passed);
        assert!(results[0].timed_out);
        assert_eq!(results[0].exit, None);
        assert!(gone(token).await, "the whole group is gone");
        let records = f.records().await;
        assert!(matches!(
            records.last(),
            Some(Record::EvidenceResult {
                timed_out: true,
                exit: None,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn background_members_do_not_outlive_a_finished_command() {
        let mut f = Fixture::new().await;
        let token = "sleep 31338";
        let (passed, _) = complete(f.plain(&cmds(&[&format!("{token} & true")])).await);
        assert!(passed);
        assert!(gone(token).await, "the leftover member was killed");
    }

    #[tokio::test]
    async fn nothing_starts_before_its_record() {
        let f = Fixture::new().await;
        let pause = f.side("pause");
        fs::create_dir_all(&pause).expect("pause dir");
        let ran = f.side("ran");
        let token = "marker-31339";
        let commands = cmds(&[&format!("echo {token}; echo ran > '{}'", ran.display())]);
        let (repo, mut log, tree, attempt_dir) = (
            f.repo.clone(),
            Log::open(&f.run_dir).await.expect("log").0,
            f.tree.clone(),
            f.attempt_dir.clone(),
        );
        let hook = pause.clone();
        let task = tokio::spawn(async move {
            run(EvidenceRun {
                repo: &repo,
                log: &mut log,
                attempt: AttemptId(0),
                candidate: CandidateId(0),
                tree,
                restart: 0,
                commands: &commands,
                timeout: Duration::from_secs(60),
                marker: "m".into(),
                pause_hook: Some(hook),
                attempt_dir,
            })
            .await
        });
        wait_for(&pause.join("held-0-0")).await;
        assert!(
            !with_token(token).await.is_empty(),
            "the held command exists"
        );
        task.abort();
        assert!(task.await.expect_err("aborted").is_cancelled());
        assert!(
            gone(token).await,
            "the held command was killed with its group"
        );
        assert!(!ran.exists(), "it never ran");
        assert!(f.records().await.is_empty(), "and it was never recorded");
    }

    #[tokio::test]
    async fn the_tail_is_the_last_16_kib() {
        let mut f = Fixture::new().await;
        let (_, results) = complete(
            f.plain(&cmds(&[
                "i=0; while [ $i -lt 4000 ]; do echo line-$i; i=$((i+1)); done",
            ]))
            .await,
        );
        let tail = &results[0].tail;
        assert!(tail.len() <= TAIL_BYTES);
        assert!(tail.len() > TAIL_BYTES - 100);
        // The tail is the end of the whole output, which is in the file.
        let whole =
            fs::read_to_string(f.attempt_dir.join("evidence").join("0-0.out")).expect("output");
        assert!(whole.len() > TAIL_BYTES && whole.ends_with(tail.as_str()));
    }

    #[test]
    fn a_tail_does_not_start_mid_character() {
        // The odd byte comes after the text, so the raw last 16 KiB starts on the
        // second byte of a two-byte character.
        let bytes = format!("{}a", "é".repeat(TAIL_BYTES)).into_bytes();
        let raw = &bytes[bytes.len() - TAIL_BYTES..];
        assert!(
            String::from_utf8_lossy(raw).starts_with('\u{fffd}'),
            "the raw cut starts mid-character, so the skip matters"
        );
        let tail = tail_of(&bytes);
        assert!(!tail.contains('\u{fffd}'), "no replacement character");
        let (accents, last) = tail.split_at(tail.len() - 1);
        assert!(accents.chars().all(|c| c == 'é') && last == "a");
        assert_eq!(
            tail.len(),
            TAIL_BYTES - 1,
            "one byte of the cut character is dropped"
        );
        assert_eq!(tail_of(b"short"), "short");
        assert_eq!(tail_of(b""), "");
    }

    #[tokio::test]
    async fn summaries_carry_the_result() {
        let result = CommandResult {
            command: "c".into(),
            exit: Some(2),
            timed_out: false,
            duration_ms: 7,
            pre_tree: "p".into(),
            post_tree: "q".into(),
            tail: "t".into(),
        };
        let summary = CommandSummary::from(&result);
        assert_eq!(
            (summary.command.as_str(), summary.exit, summary.timed_out),
            ("c", Some(2), false)
        );
        assert_eq!(
            (
                summary.pre_tree.as_str(),
                summary.post_tree.as_str(),
                summary.tail.as_str()
            ),
            ("p", "q", "t")
        );
        let _ = Identity {
            pid: 1,
            start: String::new(),
        };
    }
}
