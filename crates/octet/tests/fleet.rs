//! Release tests for `octet fleet`, numbered as the spec's "Release tests":
//! `octet fleet` runs as a child process against a real git work tree, with
//! the scripted fake vendors (`protocol-child`) in every step: the Claude
//! dialect researching, planning and reviewing, the Codex dialect coding and
//! testing.
//!
//! Each test has its own directory: the work tree `work/`, the data directory
//! `data/`, the fleet script and its turn log. The script is named to the
//! `octet` process (`OCTET_FLEET_SCRIPT`), and every vendor inherits it
//! through the gate. What a test asserts it reads from the run's log, its
//! artifacts and the fake's turn log, never from timing.
//!
//! Every wait is bounded and fails the test with what was seen. Every `octet`
//! a test starts leads its own process group, which is killed when the test
//! ends however it ends, together with any process still carrying one of the
//! test's run markers.
#![expect(
    clippy::unwrap_used,
    reason = "test code: an unwrap that fails is the test failing"
)]

use std::ffi::OsStr;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use octet_fleet::evidence::TEST_PAUSE_ENV;
use octet_fleet::record::{Accepted, AttemptId, CandidateId, Origin, PauseReason, Record};
use octet_testkit::TempDir;
use serde_json::{Value, json};

/// How long one `octet fleet` command may take. Generous: a freshly built
/// binary can stall at its first launch on macOS while it is assessed.
const PATIENCE: Duration = Duration::from_secs(300);

/// How long the processes of a killed driver may take to go away.
const SHORT: Duration = Duration::from_secs(30);

/// The task every run is given.
const TASK: &str = "Add the feature.";

/// The git variables a test's git, and `octet`, must not inherit.
const GIT_ENV: [&str; 3] = ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"];

// ---- workflows and scripts ---------------------------------------------

/// The profiles: Claude for documents and reviews, Codex for the coder and
/// the tester, every one running the fake.
fn profiles() -> String {
    let binary = octet_testkit::protocol_child();
    let binary = binary.display();
    format!(
        "profiles:\n\
\x20 claude-research: {{ engine: claude, binary: '{binary}' }}\n\
\x20 claude-review: {{ engine: claude, binary: '{binary}' }}\n\
\x20 codex-code: {{ engine: codex, binary: '{binary}', mode: full-access }}\n\
\x20 codex-test: {{ engine: codex, binary: '{binary}', mode: full-access }}\n\
roles:\n\
\x20 researcher: {{ kind: document, instructions: Investigate the code. }}\n\
\x20 planner: {{ kind: document, instructions: Write the plan. }}\n\
\x20 reviewer: {{ kind: verdict, instructions: Review the artifact. }}\n\
\x20 coder: {{ kind: change, instructions: Implement what was approved. }}\n\
\x20 tester: {{ kind: tests, instructions: Test the change. }}\n"
    )
}

/// The tests step and the final review, running `commands`.
fn tail_steps(commands: &[&str]) -> String {
    format!(
        "\x20 - id: test\n\
\x20   role: tester\n\
\x20   profile: codex-test\n\
\x20   change_from: implement\n\
\x20   commands: {}\n\
\x20 - {{ id: review_change, role: reviewer, profile: claude-review, reviews: implement, \
evidence_from: test }}\n",
        json!(commands)
    )
}

/// The shipped `research-plan-build` workflow's steps, on the fake profiles,
/// with its tests step running `commands`.
fn research_plan_build(commands: &[&str]) -> String {
    format!(
        "version: 1\n\
name: research-plan-build\n\
limits: {{ max_rounds: 5, transient_retries: 1 }}\n\
{profiles}\
steps:\n\
\x20 - {{ id: research, role: researcher, profile: claude-research }}\n\
\x20 - {{ id: review_research, role: reviewer, profile: claude-review, reviews: research }}\n\
\x20 - id: plan\n\
\x20   role: planner\n\
\x20   profile: claude-research\n\
\x20   inputs: [{{ from: research, approval_from: review_research }}]\n\
\x20 - {{ id: review_plan, role: reviewer, profile: claude-review, reviews: plan }}\n\
\x20 - id: implement\n\
\x20   role: coder\n\
\x20   profile: codex-code\n\
\x20   inputs: [{{ from: plan, approval_from: review_plan }}]\n\
{tail}",
        profiles = profiles(),
        tail = tail_steps(commands)
    )
}

/// A small workflow: the research and its review, the coder, the tests
/// (running `commands`) and the final review, `max_rounds` jumps per edge.
fn small(commands: &[&str], max_rounds: u32) -> String {
    format!(
        "version: 1\n\
name: small\n\
limits: {{ max_rounds: {max_rounds}, transient_retries: 1 }}\n\
{profiles}\
steps:\n\
\x20 - {{ id: research, role: researcher, profile: claude-research }}\n\
\x20 - {{ id: review_research, role: reviewer, profile: claude-review, reviews: research }}\n\
\x20 - id: implement\n\
\x20   role: coder\n\
\x20   profile: codex-code\n\
\x20   inputs: [{{ from: research, approval_from: review_research }}]\n\
{tail}",
        profiles = profiles(),
        tail = tail_steps(commands)
    )
}

/// A verdict reply: `kind`, with `new` blocking findings, resolving
/// `resolved`.
fn verdict(kind: &str, new: &[&str], resolved: &[&str]) -> String {
    let block = json!({
        "verdict": kind,
        "new_findings": new
            .iter()
            .map(|summary| json!({"severity": "blocking", "summary": summary}))
            .collect::<Vec<_>>(),
        "previous": resolved
            .iter()
            .map(|id| json!({"id": id, "status": "resolved"}))
            .collect::<Vec<_>>(),
    });
    format!("I read it closely.\n\n```json\n{block}\n```\n")
}

/// A verdict approving, resolving `resolved`.
fn approve(resolved: &[&str]) -> String {
    verdict("approved", &[], resolved)
}

/// The turns of the small workflow where every step does its work and every
/// review approves; the coder writes `feature.txt`.
fn small_turns() -> Value {
    json!({
        "research#1": {"reply": "The repository holds a README and fixture.txt."},
        "review_research#1": {"reply": approve(&[])},
        "implement#1": {"write": {"feature.txt": "the feature\n"}, "reply": "Added feature.txt."},
        "test#1": {"reply": "The commands cover it."},
        "review_change#1": {"reply": approve(&[])},
    })
}

// ---- git ---------------------------------------------------------------

/// `git ARGS` in `dir`; its stdout.
fn git(dir: &Path, args: &[&str]) -> String {
    git_with(dir, args, &[])
}

/// `git ARGS` in `dir` with `env` set; its stdout.
fn git_with(dir: &Path, args: &[&str], env: &[(&str, &OsStr)]) -> String {
    let mut command = Command::new("git");
    command.args(args).current_dir(dir);
    for name in GIT_ENV {
        command.env_remove(name);
    }
    command.envs(env.iter().copied());
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

// ---- the bench ---------------------------------------------------------

/// One test's directory: a git work tree with a tracked `fixture.txt`
/// (`v1`), a data directory, and the fake's script and turn log.
struct Bench {
    dir: TempDir,
    work: PathBuf,
    data: PathBuf,
    script: PathBuf,
    turns: PathBuf,
}

impl Bench {
    fn new(name: &str) -> Self {
        let dir = TempDir::new(name);
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        for args in [
            &["init", "--quiet"][..],
            &["config", "user.name", "Tester"],
            &["config", "user.email", "tester@example.com"],
            &["config", "commit.gpgsign", "false"],
        ] {
            git(&work, args);
        }
        std::fs::write(work.join("README"), "a repository\n").unwrap();
        std::fs::write(work.join("fixture.txt"), "v1\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "--quiet", "-m", "base"]);
        let data = dir.path().join("data");
        let script = dir.path().join("script.json");
        let turns = dir.path().join("turns.log");
        Self {
            dir,
            work,
            data,
            script,
            turns,
        }
    }

    /// A path in the test's directory, outside the work tree.
    fn side(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Writes the fake's script: `turns`, logged to the bench's turn log.
    fn script(&self, turns: &Value) {
        let body = json!({"log": self.turns, "turns": turns});
        std::fs::write(&self.script, body.to_string()).unwrap();
    }

    /// Writes `yaml` as the workflow; its path.
    fn workflow(&self, yaml: &str) -> PathBuf {
        let path = self.side("workflow.yaml");
        std::fs::write(&path, yaml).unwrap();
        path
    }

    /// Starts `octet fleet ARGS --journal-dir DATA` with the script named,
    /// `env` added and stdin closed.
    fn octet(&self, args: &[&OsStr], env: &[(&str, &OsStr)]) -> Octet {
        let mut command = Command::new(env!("CARGO_BIN_EXE_octet"));
        command
            .arg("fleet")
            .args(args)
            .arg("--journal-dir")
            .arg(&self.data)
            .env("OCTET_FLEET_SCRIPT", &self.script)
            .env_remove(TEST_PAUSE_ENV);
        for name in GIT_ENV {
            command.env_remove(name);
        }
        command.envs(env.iter().copied());
        Octet::spawn(command, &self.data)
    }

    /// `octet fleet run WORKFLOW --task … --cwd WORK --trust --output json`.
    fn run(&self, workflow: &Path, env: &[(&str, &OsStr)]) -> Octet {
        let args: [&OsStr; 9] = [
            "run".as_ref(),
            workflow.as_os_str(),
            "--task".as_ref(),
            TASK.as_ref(),
            "--cwd".as_ref(),
            self.work.as_os_str(),
            "--trust".as_ref(),
            "--output".as_ref(),
            "json".as_ref(),
        ];
        self.octet(&args, env)
    }

    /// `octet fleet resume RUN [--decision D] --output json`.
    fn resume(&self, decision: Option<&str>, env: &[(&str, &OsStr)]) -> Octet {
        let run = self.run_id();
        let mut args: Vec<&OsStr> = vec!["resume".as_ref(), run.as_ref()];
        if let Some(decision) = decision {
            args.extend::<[&OsStr; 2]>(["--decision".as_ref(), decision.as_ref()]);
        }
        args.extend::<[&OsStr; 2]>(["--output".as_ref(), "json".as_ref()]);
        self.octet(&args, env)
    }

    /// The only run's ID.
    fn run_id(&self) -> String {
        let runs = runs_in(&self.data);
        assert_eq!(runs.len(), 1, "{runs:?}");
        runs.into_iter().next().unwrap()
    }

    fn run_dir(&self) -> PathBuf {
        self.data.join("fleet").join(self.run_id())
    }

    /// Every whole record of the run's log; none before the log exists. A
    /// torn last line (being written) is left out.
    fn records(&self) -> Vec<Record> {
        if runs_in(&self.data).is_empty() {
            return Vec::new();
        }
        let text = std::fs::read_to_string(self.run_dir().join("log.jsonl")).unwrap_or_default();
        let whole = text.rfind('\n').map_or(0, |end| end + 1);
        text[..whole]
            .lines()
            .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
            .collect()
    }

    /// Polls the log until a record `found` accepts is in it, while `octet`
    /// runs; the records then. Fails the test, with what was seen, when
    /// `octet` ends first or [`PATIENCE`] passes.
    fn await_record(
        &self,
        octet: &mut Octet,
        what: &str,
        found: impl Fn(&Record) -> bool,
    ) -> Vec<Record> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let records = self.records();
            if records.iter().any(&found) {
                return records;
            }
            assert!(
                octet.running(),
                "octet ended before {what}: {records:#?}\n{}",
                octet.output_so_far()
            );
            assert!(
                Instant::now() < deadline,
                "no {what} within {PATIENCE:?}: {records:#?}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// The fake's turn log: one object per turn played.
    fn played(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.turns)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// The turns played for `key`, in order.
    fn plays_of(&self, key: &str) -> Vec<Value> {
        self.played()
            .into_iter()
            .filter(|turn| turn["key"] == key)
            .collect()
    }

    /// The body of evidence artifact `id`.
    fn evidence(&self, id: octet_fleet::record::ArtifactId) -> Value {
        let path = self
            .run_dir()
            .join("artifacts")
            .join(format!("{}.evidence", id.0));
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    /// The tree of the live work tree, taken as a snapshot takes it: every
    /// file, tracked or not, through a scratch index.
    fn live_tree(&self) -> String {
        let index = self.side("scratch-index");
        let _ = std::fs::remove_file(&index);
        let env = [("GIT_INDEX_FILE", index.as_os_str())];
        git_with(&self.work, &["add", "-A"], &env);
        let tree = git_with(&self.work, &["write-tree"], &env);
        std::fs::remove_file(&index).unwrap();
        tree.trim().to_owned()
    }

    /// Asserts the evidence guarantee for every passing record of the run:
    /// each command's `pre_tree` and `post_tree`, in the artifact and in the
    /// log of the pass that produced it, equal the record's tree, and every
    /// command of the step has its result.
    fn assert_passing_evidence_is_whole(&self, records: &[Record], commands: usize) {
        for record in records {
            let Record::AttemptFinished {
                attempt,
                outcome: Accepted::TestsPassed { evidence },
                ..
            } = record
            else {
                continue;
            };
            let body = self.evidence(*evidence);
            let tree = body["tree"].as_str().unwrap();
            let results = body["results"].as_array().unwrap();
            assert_eq!(results.len(), commands, "{body:#}");
            for result in results {
                assert_eq!(result["pre_tree"], tree, "{body:#}");
                assert_eq!(result["post_tree"], tree, "{body:#}");
                assert_eq!(result["exit"], 0, "{body:#}");
            }
            // The pass that produced it is the attempt's last.
            let (candidate, restart) = last_phase(records, *attempt);
            assert_eq!(body["candidate"], candidate.0, "{body:#}");
            for index in 0..commands {
                let (pre, post) = observations(records, *attempt, restart, index);
                assert_eq!(pre.as_deref(), Some(tree), "command {index}: {records:#?}");
                assert_eq!(post.as_deref(), Some(tree), "command {index}: {records:#?}");
            }
        }
    }
}

/// The IDs of the runs under `data`.
fn runs_in(data: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(data.join("fleet")) else {
        return Vec::new();
    };
    let mut runs: Vec<String> = entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.join("run.json").exists())
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    runs.sort();
    runs
}

// ---- reading the log ---------------------------------------------------

/// The steps of the attempts started, in order, with their attempt IDs.
fn started(records: &[Record]) -> Vec<(AttemptId, &str)> {
    records
        .iter()
        .filter_map(|record| match record {
            Record::AttemptStarted { attempt, step, .. } => Some((*attempt, step.as_str())),
            _ => None,
        })
        .collect()
}

/// The position in the log of the first record `found` accepts.
fn position(records: &[Record], found: impl Fn(&Record) -> bool) -> Option<usize> {
    records.iter().position(found)
}

/// Whether `record` finished a tests attempt that passed.
fn passed(record: &Record) -> bool {
    matches!(
        record,
        Record::AttemptFinished {
            outcome: Accepted::TestsPassed { .. },
            ..
        }
    )
}

/// Whether `record` finished a tests attempt that failed.
fn failed(record: &Record) -> bool {
    matches!(
        record,
        Record::AttemptFinished {
            outcome: Accepted::TestsFailed { .. },
            ..
        }
    )
}

/// The candidate and restart of `attempt`'s last evidence phase.
fn last_phase(records: &[Record], attempt: AttemptId) -> (CandidateId, u32) {
    records
        .iter()
        .rev()
        .find_map(|record| match record {
            Record::EvidenceStarted {
                attempt: a,
                candidate,
                restart,
            } if *a == attempt => Some((*candidate, *restart)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("attempt {} has no evidence: {records:#?}", attempt.0))
}

/// The logged `pre_tree` and `post_tree` of command `index` in `attempt`'s
/// pass `restart`, where logged.
fn observations(
    records: &[Record],
    attempt: AttemptId,
    restart: u32,
    index: usize,
) -> (Option<String>, Option<String>) {
    let mut seen = (None, None);
    for record in records {
        match record {
            Record::EvidenceCommand {
                attempt: a,
                restart: r,
                index: i,
                pre_tree,
                ..
            } if (*a, *r, *i) == (attempt, restart, index) => seen.0 = Some(pre_tree.clone()),
            Record::EvidenceResult {
                attempt: a,
                restart: r,
                index: i,
                post_tree,
                ..
            } if (*a, *r, *i) == (attempt, restart, index) => seen.1 = Some(post_tree.clone()),
            _ => {}
        }
    }
    seen
}

/// The exit of command `index` in `attempt`'s pass `restart`.
fn exit_of(records: &[Record], attempt: AttemptId, restart: u32, index: usize) -> Option<i32> {
    records
        .iter()
        .find_map(|record| match record {
            Record::EvidenceResult {
                attempt: a,
                restart: r,
                index: i,
                exit,
                ..
            } if (*a, *r, *i) == (attempt, restart, index) => Some(*exit),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for command {index} of pass {restart}"))
}

/// The tree of candidate `id`, from its `candidate` record.
fn tree_of(records: &[Record], id: CandidateId) -> String {
    records
        .iter()
        .find_map(|record| match record {
            Record::Candidate { id: c, tree, .. } if *c == id => Some(tree.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no candidate {}: {records:#?}", id.0))
}

/// The origin of candidate `id`.
fn origin_of(records: &[Record], id: CandidateId) -> Origin {
    records
        .iter()
        .find_map(|record| match record {
            Record::Candidate { id: c, origin, .. } if *c == id => Some(*origin),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no candidate {}: {records:#?}", id.0))
}

/// The only tests attempt of the run.
fn only_test_attempt(records: &[Record]) -> AttemptId {
    let tests: Vec<AttemptId> = started(records)
        .into_iter()
        .filter(|(_, step)| *step == "test")
        .map(|(attempt, _)| attempt)
        .collect();
    assert_eq!(tests.len(), 1, "{records:#?}");
    tests[0]
}

/// The pause the log ends with.
fn last_pause(records: &[Record]) -> PauseReason {
    match records.last() {
        Some(Record::Paused { reason, .. }) => *reason,
        other => panic!("the log ends with {other:?}, not a pause"),
    }
}

// ---- the octet process -------------------------------------------------

/// What an `octet` process left.
struct Ended {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Ended {
    /// Asserts the exit code, showing the output when it is another.
    fn expect_code(&self, code: i32) {
        assert_eq!(
            self.code,
            Some(code),
            "stdout:\n{}\nstderr:\n{}",
            self.stdout,
            self.stderr
        );
    }
}

/// A running `octet`, leading its own process group. Dropped, it kills that
/// group and every process carrying a marker of a run in its data
/// directory, so nothing it started outlives the test.
struct Octet {
    child: Child,
    reaped: bool,
    data: PathBuf,
    stdout: mpsc::Receiver<String>,
    stderr: mpsc::Receiver<String>,
}

/// Reads `pipe` to its end on a thread; the text comes over the channel.
fn collect(mut pipe: impl std::io::Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        let _ = tx.send(String::from_utf8_lossy(&bytes).into_owned());
    });
    rx
}

impl Octet {
    fn spawn(mut command: Command, data: &Path) -> Self {
        command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let stdout = collect(child.stdout.take().unwrap());
        let stderr = collect(child.stderr.take().unwrap());
        Self {
            child,
            reaped: false,
            data: data.to_path_buf(),
            stdout,
            stderr,
        }
    }

    fn pid(&self) -> i32 {
        i32::try_from(self.child.id()).unwrap()
    }

    /// Whether the process is still running.
    fn running(&mut self) -> bool {
        if self.reaped {
            return false;
        }
        let ended = self.child.try_wait().unwrap().is_some();
        self.reaped = ended;
        !ended
    }

    /// What is known of the output, for a failure message, once it ended.
    fn output_so_far(&self) -> String {
        let take = |rx: &mpsc::Receiver<String>| {
            rx.recv_timeout(Duration::from_secs(1))
                .unwrap_or_else(|_| "(not closed)".into())
        };
        format!(
            "stdout:\n{}\nstderr:\n{}",
            take(&self.stdout),
            take(&self.stderr)
        )
    }

    /// SIGKILLs `octet` alone (not its group) and reaps it.
    fn sigkill(&mut self) {
        // SAFETY: kill(2) of a child this test spawned and has not reaped.
        assert_eq!(unsafe { libc::kill(self.pid(), libc::SIGKILL) }, 0);
        self.child.wait().unwrap();
        self.reaped = true;
    }

    /// Waits for `octet` to exit, for at most [`PATIENCE`].
    fn finish(mut self) -> Ended {
        let deadline = Instant::now() + PATIENCE;
        while self.running() {
            assert!(
                Instant::now() < deadline,
                "octet fleet did not finish within {PATIENCE:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let code = self.child.wait().unwrap().code();
        // A vendor that inherited a pipe may hold it a little longer.
        let left = || {
            deadline
                .saturating_duration_since(Instant::now())
                .max(SHORT)
        };
        let stdout = self.stdout.recv_timeout(left()).unwrap_or_default();
        let stderr = self.stderr.recv_timeout(left()).unwrap_or_default();
        Ended {
            code,
            stdout,
            stderr,
        }
    }
}

impl Drop for Octet {
    fn drop(&mut self) {
        if !self.reaped && self.child.try_wait().ok().flatten().is_none() {
            // SAFETY: killpg(2) of the group this unreaped child leads.
            unsafe { libc::killpg(self.pid(), libc::SIGKILL) };
            let _ = self.child.wait();
        }
        kill_marked(&self.data);
    }
}

/// SIGKILLs every process whose `OCTET_FLEET_ATTEMPT` marker names a run in
/// `data`: a vendor or a test command of a run this test drove.
fn kill_marked(data: &Path) {
    let runs = runs_in(data);
    if runs.is_empty() {
        return;
    }
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    let Ok(processes) = runtime.block_on(octet_fleet::ownership::list_processes()) else {
        return;
    };
    for process in processes {
        let ours = process.marker.as_deref().is_some_and(|marker| {
            runs.iter().any(|run| {
                marker
                    .strip_prefix(run.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
            })
        });
        if ours && let Ok(pid) = i32::try_from(process.pid) {
            // SAFETY: kill(2) of a process carrying this test's run marker.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// Whether process `pid` exists.
fn alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Waits until every process the log or the gate registered for `attempt`
/// is gone, for at most [`SHORT`].
fn await_gone(bench: &Bench, records: &[Record], attempt: AttemptId) {
    let mut pids: Vec<u32> = records
        .iter()
        .filter_map(|record| match record {
            Record::AttemptSession {
                attempt: a,
                identities,
                ..
            } if *a == attempt => Some(identities.iter().map(|identity| identity.pid)),
            _ => None,
        })
        .flatten()
        .collect();
    let registry = bench
        .run_dir()
        .join("attempts")
        .join(attempt.0.to_string())
        .join("processes");
    pids.extend(
        std::fs::read_to_string(registry)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok()),
    );
    let gone = octet_testkit::wait_until(SHORT, || !pids.iter().any(|&pid| alive(pid)));
    assert!(gone, "the killed driver's processes {pids:?} outlived it");
}

// ---- 1. the full run ---------------------------------------------------

/// The spec's release test 1, "The full run".
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario: its assertions follow the spec's sentence in order"
)]
fn the_full_run() {
    let bench = Bench::new("octet-fleet-full");
    let commands = ["sh feature_test.sh"];
    let workflow = bench.workflow(&research_plan_build(&commands));
    let mut turns = json!({
        "research#1": {"reply": "RESEARCH-ONE: the repository holds a README."},
        "review_research#1": {
            "reply": verdict("changes_requested", &["The research misses fixture.txt."], &[])
        },
        "research#2": {"reply": "RESEARCH-TWO: the repository holds a README and fixture.txt."},
        "review_research#2": {"reply": approve(&["review_research/F1"])},
        "plan#1": {"reply": "PLAN-ONE: 1. Add feature.txt. 2. Test that it works."},
        "review_plan#1": {"reply": approve(&[])},
        "implement#1": {"write": {"feature.txt": "broken\n"}, "reply": "Added feature.txt."},
        // The first tester holds its turn, having done nothing, until Octet dies.
        "test#1": {"hold": "before_write"},
        "implement#2": {"write": {"feature.txt": "working\n"}, "reply": "Fixed feature.txt."},
        "test#2": {"reply": "The test stands."},
        "review_change#1": {
            "reply": verdict("changes_requested", &["feature.txt needs a comment."], &[])
        },
        "implement#3": {
            "write": {"feature.txt": "working # the feature\n"},
            "reply": "Commented feature.txt."
        },
        "test#3": {"reply": "The test stands."},
        "review_change#2": {"reply": approve(&["review_change/F1"])},
    });
    bench.script(&turns);

    // Octet is SIGKILLed once the tests attempt's start is durable.
    let mut octet = bench.run(&workflow, &[]);
    let records = bench.await_record(
        &mut octet,
        "the tests attempt",
        |record| matches!(record, Record::AttemptStarted { step, .. } if step == "test"),
    );
    octet.sigkill();
    let killed = started(&records)
        .into_iter()
        .find(|(_, step)| *step == "test")
        .unwrap()
        .0;
    // The held tester reads its input from Octet: it ends at end of input,
    // and the gate gives up once Octet is gone. Resume starts when they have.
    await_gone(&bench, &bench.records(), killed);

    // From now on the tester writes its test, which fails on the first
    // feature.txt.
    turns["test#1"] = json!({
        "write": {"feature_test.sh": "grep -q working feature.txt\n"},
        "reply": "Added feature_test.sh."
    });
    bench.script(&turns);
    bench.resume(None, &[]).finish().expect_code(0);

    let records = bench.records();
    let played = bench.played();
    let keys: Vec<&str> = played
        .iter()
        .map(|turn| turn["key"].as_str().unwrap())
        .filter(|key| *key != "test#1")
        .collect();
    assert_eq!(
        keys,
        [
            "research#1",
            "review_research#1",
            "research#2",
            "review_research#2",
            "plan#1",
            "review_plan#1",
            "implement#1",
            "implement#2",
            "test#2",
            "review_change#1",
            "implement#3",
            "test#3",
            "review_change#2",
        ],
        "{played:#?}"
    );
    assert!(
        !played.iter().any(|turn| turn.get("missing").is_some()),
        "{played:#?}"
    );

    // The coder's prompt carries the approved plan and the approved second
    // research, never the rejected first.
    let implement = bench.plays_of("implement#1");
    assert_eq!(implement.len(), 1, "the coder ran once: {implement:#?}");
    let prompt = implement[0]["prompt"].as_str().unwrap();
    assert!(
        prompt.contains("## Approved document (plan)\nPLAN-ONE"),
        "{prompt}"
    );
    assert!(
        prompt.contains("## Approved document (research)\nRESEARCH-TWO"),
        "{prompt}"
    );
    assert!(!prompt.contains("RESEARCH-ONE"), "{prompt}");

    // Resume re-ran the tester, in a new attempt; the coder ran once before
    // the tests failed.
    let first_failure = position(&records, failed).expect("the tests failed once");
    let before: Vec<(AttemptId, &str)> = started(&records[..first_failure]);
    let tests: Vec<AttemptId> = before
        .iter()
        .filter(|(_, step)| *step == "test")
        .map(|(attempt, _)| *attempt)
        .collect();
    assert_eq!(tests.len(), 2, "{before:?}");
    assert_eq!(tests[0], killed);
    assert!(
        !records.iter().any(|record| matches!(
            record,
            Record::AttemptFinished { attempt, .. } if *attempt == killed
        )),
        "the killed tester's attempt was accepted: {records:#?}"
    );
    let coders = before
        .iter()
        .filter(|(_, step)| *step == "implement")
        .count();
    assert_eq!(coders, 1, "{before:?}");
    // The re-run tester's turn is what the failing evidence ran on: its file.
    let Some(Record::AttemptFinished { attempt: rerun, .. }) = records.get(first_failure) else {
        unreachable!()
    };
    assert_eq!(*rerun, tests[1]);
    let (tested, _) = last_phase(&records, tests[1]);
    assert_eq!(origin_of(&records, tested), Origin::Tests);

    // A command failed, and the run could not go on to the final review:
    // the coder came next, and no review ran before the tests passed.
    let Some(Record::AttemptFinished {
        outcome: Accepted::TestsFailed { evidence, failing },
        ..
    }) = records.get(first_failure)
    else {
        unreachable!()
    };
    assert_eq!(failing, &[0]);
    assert_ne!(bench.evidence(*evidence)["results"][0]["exit"], 0);
    let after: Vec<&str> = started(&records[first_failure..])
        .into_iter()
        .map(|(_, step)| step)
        .collect();
    assert_eq!(
        after,
        [
            "implement",
            "test",
            "review_change",
            "implement",
            "test",
            "review_change"
        ]
    );
    let first_pass = position(&records, passed).expect("the tests passed");
    assert!(
        !started(&records[..first_pass])
            .iter()
            .any(|(_, step)| *step == "review_change"),
        "{records:#?}"
    );
    let completed_at = position(&records, |r| matches!(r, Record::Completed { .. })).unwrap();
    assert_eq!(completed_at, records.len() - 1, "{records:#?}");

    // The coder's second round continued its session.
    let second = bench.plays_of("implement#2");
    assert_eq!(second.len(), 1);
    assert_eq!(second[0]["resumed"], true, "{second:#?}");

    // The final review is fresh: a new attempt in a new session. Its prompt
    // names the passing evidence's candidate, and its patch has the tester's
    // file.
    let reviews: Vec<AttemptId> = started(&records)
        .into_iter()
        .filter(|(_, step)| *step == "review_change")
        .map(|(attempt, _)| attempt)
        .collect();
    assert_eq!(reviews.len(), 2);
    assert_ne!(reviews[0], reviews[1]);
    let last = bench.plays_of("review_change#2");
    assert_eq!(last.len(), 1);
    assert_eq!(last[0]["resumed"], false, "{last:#?}");
    let Some(Record::Completed {
        candidate,
        tree,
        evidence,
        observed_tree,
        ..
    }) = records.last()
    else {
        unreachable!()
    };
    assert_eq!(observed_tree, tree);
    let body = bench.evidence(*evidence);
    assert_eq!(body["candidate"], candidate.0);
    assert_eq!(body["tree"], tree.as_str());
    let prompt = last[0]["prompt"].as_str().unwrap();
    let k = candidate.0;
    assert!(
        prompt.contains(&format!(
            "## Test evidence for {k}\nCandidate {k} (tree {tree})."
        )),
        "{prompt}"
    );
    let patch = prompt
        .split_once(&format!("## Patch (base..{k})\n"))
        .unwrap_or_else(|| panic!("no patch of {k}: {prompt}"))
        .1;
    let patch = patch.split_once("\n## ").map_or(patch, |(patch, _)| patch);
    assert!(patch.contains("feature_test.sh"), "{patch}");
    assert!(patch.contains("+grep -q working feature.txt"), "{patch}");
    assert_eq!(bench.live_tree(), *tree);
    bench.assert_passing_evidence_is_whole(&records, commands.len());
}

// ---- 2. wrong-revision approval ----------------------------------------

/// The spec's release test 2, "Wrong-revision approval".
#[test]
fn a_wrong_revision_is_never_approved() {
    let bench = Bench::new("octet-fleet-revision");
    // A passes on the base fixture; B rewrites it and passes; A fails on it.
    let commands = ["grep -qx v1 fixture.txt", "printf 'v2\\n' > fixture.txt"];
    let workflow = bench.workflow(&small(&commands, 5));
    // Sent back, the coder changes nothing: the run stalls there.
    let mut turns = small_turns();
    turns["implement#2"] = json!({"reply": "Nothing to change."});
    bench.script(&turns);
    bench.run(&workflow, &[]).finish().expect_code(3);

    let records = bench.records();
    let attempt = started(&records)
        .into_iter()
        .find(|(_, step)| *step == "test")
        .unwrap()
        .0;
    // Pass 0: A passed, B passed and changed the tree, which voided the pass.
    let base = records
        .iter()
        .find_map(|record| match record {
            Record::EvidenceStarted {
                attempt: a,
                candidate,
                restart: 0,
            } if *a == attempt => Some(*candidate),
            _ => None,
        })
        .unwrap();
    let t = tree_of(&records, base);
    assert_eq!(exit_of(&records, attempt, 0, 0), Some(0));
    assert_eq!(exit_of(&records, attempt, 0, 1), Some(0));
    let (_, after_b) = observations(&records, attempt, 0, 1);
    let after_b = after_b.unwrap();
    assert_ne!(after_b, t);
    // Pass 1 ran on B's tree, where A fails: the tests failed.
    let (derived, restart) = last_phase(&records, attempt);
    assert_eq!(restart, 1);
    assert_eq!(origin_of(&records, derived), Origin::Evidence);
    assert_eq!(tree_of(&records, derived), after_b);
    assert_eq!(exit_of(&records, attempt, 1, 0), Some(1));
    assert!(records.iter().any(|record| matches!(
        record,
        Record::AttemptFinished {
            attempt: a,
            outcome: Accepted::TestsFailed { failing, .. },
            ..
        } if *a == attempt && failing == &[0]
    )));
    // The run cannot complete: no passing record, no final review, no
    // completion. The failure sent the run back to the coder, whose
    // unchanged tree is a stall.
    assert!(!records.iter().any(passed), "{records:#?}");
    assert!(
        !started(&records)
            .iter()
            .any(|(_, step)| *step == "review_change"),
        "{records:#?}"
    );
    assert!(bench.plays_of("review_change#1").is_empty());
    assert!(
        !records
            .iter()
            .any(|r| matches!(r, Record::Completed { .. }))
    );
    let after_tests: Vec<&str> = started(&records)
        .into_iter()
        .map(|(_, step)| step)
        .skip_while(|step| *step != "test")
        .collect();
    assert_eq!(after_tests, ["test", "implement"], "{records:#?}");
    assert_eq!(last_pause(&records), PauseReason::Stalled);
}

// ---- 3. change, use, restore -------------------------------------------

/// The spec's release test 3, "Change, use, restore".
#[test]
fn a_change_used_and_restored_is_caught() {
    let bench = Bench::new("octet-fleet-restore");
    let commands = [
        "printf 'changed\\n' > fixture.txt",
        "grep -qx changed fixture.txt",
        "printf 'v1\\n' > fixture.txt",
    ];
    let workflow = bench.workflow(&small(&commands, 1));
    bench.script(&small_turns());
    bench.run(&workflow, &[]).finish().expect_code(3);

    let records = bench.records();
    let attempt = only_test_attempt(&records);
    // The boundary after A voided pass 0: neither B nor C was released in it.
    let (pre, post) = observations(&records, attempt, 0, 0);
    assert_ne!(pre, post, "{records:#?}");
    for index in [1, 2] {
        assert_eq!(
            observations(&records, attempt, 0, index),
            (None, None),
            "command {index} ran in the voided pass: {records:#?}"
        );
    }
    // Wherever B ran, its pass was voided too: C's restore changed the tree.
    for record in &records {
        if let Record::EvidenceResult {
            restart,
            index: 1,
            post_tree,
            ..
        } = record
        {
            let (_, after_c) = observations(&records, attempt, *restart, 2);
            assert!(
                after_c.is_some_and(|after_c| after_c != *post_tree),
                "pass {restart} was not voided after B: {records:#?}"
            );
        }
    }
    // No passing record names a pass in which B ran on another tree: there
    // is none, and the tree would not hold still.
    assert!(!records.iter().any(passed), "{records:#?}");
    assert!(
        !records
            .iter()
            .any(|r| matches!(r, Record::AttemptFinished { attempt: a, .. } if *a == attempt)),
        "{records:#?}"
    );
    assert!(
        !records
            .iter()
            .any(|r| matches!(r, Record::Completed { .. }))
    );
    assert_eq!(last_pause(&records), PauseReason::UnstableTests);
}

// ---- 8. the completion boundary ----------------------------------------

/// The spec's release test 8, "The completion boundary".
///
/// The edit lands between the final approval and completion through `git`:
/// Octet observes the tree with `git` from its `PATH`, and the test puts a
/// wrapper first there that, on the first call after the approval is in
/// the log, edits the work tree before running the real `git`. So the edit
/// comes after the approval and before any observation that follows it.
#[test]
fn an_edit_before_completion_is_drift() {
    let bench = Bench::new("octet-fleet-boundary");
    let commands = ["test -f feature.txt"];
    let workflow = bench.workflow(&small(&commands, 1));
    let mut turns = small_turns();
    turns["test#2"] = json!({"reply": "The commands cover it."});
    turns["review_change#2"] = json!({"reply": approve(&[])});
    bench.script(&turns);

    let real_git = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real_git = String::from_utf8(real_git.stdout)
        .unwrap()
        .trim()
        .to_owned();
    assert!(Path::new(&real_git).is_absolute(), "{real_git:?}");
    let edited = bench.side("edited");
    let late = bench.work.join("late.txt");
    let bin = bench.side("bin");
    octet_testkit::write_script(
        &bin,
        "git",
        &format!(
            "if [ ! -e '{edited}' ] && cat '{data}'/fleet/*/log.jsonl 2>/dev/null \
             | grep -q '\"type\":\"change_approved\"'; then\n\
             : > '{edited}'\n\
             printf 'edited after the approval\\n' > '{late}'\n\
             fi\n\
             exec '{real_git}' \"$@\"",
            edited = edited.display(),
            data = bench.data.display(),
            late = late.display(),
        ),
    );
    let path = std::env::join_paths(
        std::iter::once(bin.clone())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let env = [("PATH", path.as_os_str())];
    bench.run(&workflow, &env).finish().expect_code(3);

    // The edit came after the final approval: drift, not completion.
    let records = bench.records();
    assert!(edited.exists() && late.exists());
    let approved = position(&records, |record| {
        matches!(
            record,
            Record::AttemptFinished {
                outcome: Accepted::ChangeApproved { .. },
                ..
            }
        )
    })
    .expect("the final review approved");
    assert!(
        !records
            .iter()
            .any(|r| matches!(r, Record::Completed { .. }))
    );
    assert_eq!(last_pause(&records), PauseReason::Drift);
    assert!(position(&records, |r| matches!(r, Record::Paused { .. })).unwrap() > approved);

    // Adopted, the tests and the review run again on the edited tree, and the
    // completed record carries the tree observed last: the live one.
    bench.resume(Some("adopt"), &env).finish().expect_code(0);
    let records = bench.records();
    let Some(Record::Completed {
        candidate,
        tree,
        observed_tree,
        ..
    }) = records.last()
    else {
        panic!("{records:#?}");
    };
    assert_eq!(observed_tree, tree);
    assert_eq!(*tree, bench.live_tree());
    assert_eq!(origin_of(&records, *candidate), Origin::Drift);
    let listed = git(&bench.work, &["ls-tree", "--name-only", tree]);
    assert!(listed.lines().any(|name| name == "late.txt"), "{listed}");
    bench.assert_passing_evidence_is_whole(&records, commands.len());
}

// ---- 10. an edit between commands --------------------------------------

/// Answers the evidence runner's test pauses in `dir` until `stop`: each
/// `held-<restart>-<index>` gets its `continue-…`, and before the one of
/// command 1 in pass 0 `edit` runs. Ends by itself after [`PATIENCE`].
fn answer_pauses(dir: PathBuf, stop: Arc<AtomicBool>, edit: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        let deadline = Instant::now() + PATIENCE;
        while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let name = entry.unwrap().file_name().to_string_lossy().into_owned();
                let Some(point) = name.strip_prefix("held-") else {
                    continue;
                };
                let go = dir.join(format!("continue-{point}"));
                if go.exists() {
                    continue;
                }
                if point == "0-1" {
                    edit();
                }
                std::fs::write(go, "").unwrap();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
}

/// The spec's release test 10, "An edit between commands".
#[test]
fn an_edit_between_commands_voids_the_pass_before_the_next_runs() {
    let bench = Bench::new("octet-fleet-between");
    let pauses = bench.side("pauses");
    std::fs::create_dir_all(&pauses).unwrap();
    // B's first action: a line naming its pass, the newest it is held in.
    let marks = bench.side("b-ran");
    let b = bench.side("b.sh");
    std::fs::write(
        &b,
        format!(
            "n=$(ls \"${TEST_PAUSE_ENV}\" | sed -n 's/^held-\\([0-9]*\\)-1$/\\1/p' | sort -n | tail -n 1)\n\
             echo \"B ran (restart $n)\" >> '{}'\n\
             test -f feature.txt\n",
            marks.display()
        ),
    )
    .unwrap();
    let b_command = format!("sh '{}'", b.display());
    let commands = ["test -f feature.txt", b_command.as_str()];
    let workflow = bench.workflow(&small(&commands, 1));
    bench.script(&small_turns());

    let stop = Arc::new(AtomicBool::new(false));
    let fixture = bench.work.join("fixture.txt");
    answer_pauses(pauses.clone(), Arc::clone(&stop), move || {
        std::fs::write(&fixture, "edited between commands\n").unwrap();
    });
    let ended = bench
        .run(&workflow, &[(TEST_PAUSE_ENV, pauses.as_os_str())])
        .finish();
    stop.store(true, Ordering::Relaxed);
    ended.expect_code(0);

    let records = bench.records();
    let attempt = only_test_attempt(&records);
    // A finished on T; B, edited under while held, was never released in
    // that pass: it ran once, in the pass on the edited tree.
    let ran = std::fs::read_to_string(&marks).unwrap();
    assert_eq!(ran, "B ran (restart 1)\n");
    let (pre, post) = observations(&records, attempt, 0, 0);
    assert!(pre.is_some() && pre == post, "{records:#?}");
    assert_eq!(observations(&records, attempt, 0, 1), (None, None));
    let (derived, restart) = last_phase(&records, attempt);
    assert_eq!(restart, 1);
    assert_eq!(origin_of(&records, derived), Origin::Evidence);
    assert_ne!(Some(tree_of(&records, derived)), pre);
    // The suite passed on the edited tree, every observation equal to it.
    let Some(Record::Completed {
        candidate,
        evidence,
        tree,
        ..
    }) = records.last()
    else {
        panic!("{records:#?}");
    };
    assert_eq!(*candidate, derived);
    let body = bench.evidence(*evidence);
    assert_eq!(body["candidate"], derived.0);
    assert_eq!(body["tree"], tree.as_str());
    assert_eq!(
        git(&bench.work, &["show", &format!("{tree}:fixture.txt")]),
        "edited between commands\n"
    );
    bench.assert_passing_evidence_is_whole(&records, commands.len());
}
