//! What the release tests share: the workflows and scripts they run, the
//! bench (a git work tree, a data directory, the fake's script and turn
//! log), the `octet` process and its drop guard, the readers of a run's log,
//! and the bounded waits.

use std::ffi::OsStr;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use octet_fleet::evidence::TEST_PAUSE_ENV;
use octet_fleet::ownership::ProcessInfo;
use octet_fleet::record::{Accepted, AttemptId, CandidateId, Origin, PauseReason, Record};
use octet_testkit::TempDir;
use serde_json::{Value, json};

/// How long one `octet fleet` command may take. Generous: a freshly built
/// binary can stall at its first launch on macOS while it is assessed.
pub(crate) const PATIENCE: Duration = Duration::from_secs(300);

/// How long the processes of a killed driver may take to go away.
pub(crate) const SHORT: Duration = Duration::from_secs(30);

/// The task every run is given.
pub(crate) const TASK: &str = "Add the feature.";

/// The git variables a test's git, and `octet`, must not inherit.
pub(crate) const GIT_ENV: [&str; 3] = ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"];

// ---- workflows and scripts ---------------------------------------------

/// The profiles: Claude for documents and reviews, Codex for the coder and
/// the tester, every one running the fake.
pub(crate) fn profiles() -> String {
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
pub(crate) fn tail_steps(commands: &[&str]) -> String {
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
pub(crate) fn research_plan_build(commands: &[&str]) -> String {
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
pub(crate) fn small(commands: &[&str], max_rounds: u32) -> String {
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
pub(crate) fn verdict(kind: &str, new: &[&str], resolved: &[&str]) -> String {
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

/// A verdict reply: `kind`, with `new` blocking findings, giving each earlier
/// finding of `previous` its status (`open` or `resolved`).
pub(crate) fn verdict_previous(kind: &str, new: &[&str], previous: &[(&str, &str)]) -> String {
    let block = json!({
        "verdict": kind,
        "new_findings": new
            .iter()
            .map(|summary| json!({"severity": "blocking", "summary": summary}))
            .collect::<Vec<_>>(),
        "previous": previous
            .iter()
            .map(|(id, status)| json!({"id": id, "status": status}))
            .collect::<Vec<_>>(),
    });
    format!("I read it again.\n\n```json\n{block}\n```\n")
}

/// A verdict approving, resolving `resolved`.
pub(crate) fn approve(resolved: &[&str]) -> String {
    verdict("approved", &[], resolved)
}

/// The turns of the small workflow where every step does its work and every
/// review approves; the coder writes `feature.txt`.
pub(crate) fn small_turns() -> Value {
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
pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
    git_with(dir, args, &[])
}

/// `git ARGS` in `dir` with `env` set; its stdout.
pub(crate) fn git_with(dir: &Path, args: &[&str], env: &[(&str, &OsStr)]) -> String {
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
pub(crate) struct Bench {
    dir: TempDir,
    pub(crate) work: PathBuf,
    pub(crate) data: PathBuf,
    pub(crate) script: PathBuf,
    pub(crate) turns: PathBuf,
}

impl Bench {
    pub(crate) fn new(name: &str) -> Self {
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
    pub(crate) fn side(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Writes the fake's script: `turns`, logged to the bench's turn log;
    /// every vendor process appends its PID to [`Bench::started_pids`]'s file
    /// as it starts.
    pub(crate) fn script(&self, turns: &Value) {
        let body = json!({
            "log": self.turns,
            "startup_marker": self.side("started.pids"),
            "turns": turns,
        });
        std::fs::write(&self.script, body.to_string()).unwrap();
    }

    /// Writes `yaml` as the workflow; its path.
    pub(crate) fn workflow(&self, yaml: &str) -> PathBuf {
        self.workflow_named("workflow.yaml", yaml)
    }

    /// Writes `yaml` as the workflow file `name`; its path.
    pub(crate) fn workflow_named(&self, name: &str, yaml: &str) -> PathBuf {
        let path = self.side(name);
        std::fs::write(&path, yaml).unwrap();
        path
    }

    /// The PIDs of the vendor processes that started, from the script's
    /// startup marker: a gate whose PID is not here never ran its vendor.
    pub(crate) fn started_pids(&self) -> Vec<u32> {
        std::fs::read_to_string(self.side("started.pids"))
            .unwrap_or_default()
            .lines()
            .map(|line| line.trim().parse().unwrap())
            .collect()
    }

    /// Starts `octet fleet ARGS --journal-dir DATA` with the script named,
    /// `env` added and stdin closed.
    pub(crate) fn octet(&self, args: &[&OsStr], env: &[(&str, &OsStr)]) -> Octet {
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
        Octet::spawn(command)
    }

    /// `octet fleet run WORKFLOW --task … --cwd WORK --trust --output json`.
    pub(crate) fn run(&self, workflow: &Path, env: &[(&str, &OsStr)]) -> Octet {
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

    /// `octet fleet resume RUN [--decision D] --output json`, for the only
    /// run.
    pub(crate) fn resume(&self, decision: Option<&str>, env: &[(&str, &OsStr)]) -> Octet {
        self.resume_run(&self.run_id(), decision, env)
    }

    /// `octet fleet resume RUN ARGS --output json`, for the only run.
    pub(crate) fn resume_args(&self, args: &[&str]) -> Octet {
        let run = self.run_id();
        let mut all: Vec<&OsStr> = vec!["resume".as_ref(), run.as_ref()];
        all.extend(args.iter().map(OsStr::new));
        all.extend::<[&OsStr; 2]>(["--output".as_ref(), "json".as_ref()]);
        self.octet(&all, &[])
    }

    /// `octet fleet stop RUN`.
    pub(crate) fn stop_run(&self, run: &str) -> Octet {
        self.octet(&["stop".as_ref(), run.as_ref()], &[])
    }

    /// `octet fleet resume RUN [--decision D] --output json`.
    pub(crate) fn resume_run(
        &self,
        run: &str,
        decision: Option<&str>,
        env: &[(&str, &OsStr)],
    ) -> Octet {
        let mut args: Vec<&OsStr> = vec!["resume".as_ref(), run.as_ref()];
        if let Some(decision) = decision {
            args.extend::<[&OsStr; 2]>(["--decision".as_ref(), decision.as_ref()]);
        }
        args.extend::<[&OsStr; 2]>(["--output".as_ref(), "json".as_ref()]);
        self.octet(&args, env)
    }

    /// The only run's ID.
    pub(crate) fn run_id(&self) -> String {
        let runs = runs_in(&self.data);
        assert_eq!(runs.len(), 1, "{runs:?}");
        runs.into_iter().next().unwrap()
    }

    pub(crate) fn run_dir(&self) -> PathBuf {
        self.run_dir_of(&self.run_id())
    }

    /// The directory of run `run`.
    pub(crate) fn run_dir_of(&self, run: &str) -> PathBuf {
        self.data.join("fleet").join(run)
    }

    /// The directory of attempt `attempt` of the only run.
    pub(crate) fn attempt_dir(&self, attempt: AttemptId) -> PathBuf {
        self.run_dir().join("attempts").join(attempt.0.to_string())
    }

    /// The IDs of the runs in the data directory, oldest first.
    pub(crate) fn run_ids(&self) -> Vec<String> {
        runs_in(&self.data)
    }

    /// The runs the work tree's ownership record lists: those with
    /// unresolved ownership of it.
    pub(crate) fn listed_runs(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.data.join("fleet").join("worktrees")) else {
            return Vec::new();
        };
        let mut listed = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                let record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                listed.extend(record["runs"].as_object().unwrap().keys().cloned());
            }
        }
        listed.sort();
        listed
    }

    /// Every whole record of the run's log; none before the log exists. A
    /// torn last line (being written) is left out.
    pub(crate) fn records(&self) -> Vec<Record> {
        if runs_in(&self.data).is_empty() {
            return Vec::new();
        }
        self.records_of(&self.run_id())
    }

    /// Every whole record of run `run`'s log, as [`Bench::records`].
    pub(crate) fn records_of(&self, run: &str) -> Vec<Record> {
        let text =
            std::fs::read_to_string(self.run_dir_of(run).join("log.jsonl")).unwrap_or_default();
        let whole = text.rfind('\n').map_or(0, |end| end + 1);
        text[..whole]
            .lines()
            .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
            .collect()
    }

    /// Polls the log until a record `found` accepts is in it, while `octet`
    /// runs; the records then. Fails the test, with what was seen, when
    /// `octet` ends first or [`PATIENCE`] passes.
    pub(crate) fn await_record(
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
    pub(crate) fn played(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.turns)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// The turns played for `key`, in order.
    pub(crate) fn plays_of(&self, key: &str) -> Vec<Value> {
        self.played()
            .into_iter()
            .filter(|turn| turn["key"] == key)
            .collect()
    }

    /// The body of evidence artifact `id`.
    pub(crate) fn evidence(&self, id: octet_fleet::record::ArtifactId) -> Value {
        let path = self
            .run_dir()
            .join("artifacts")
            .join(format!("{}.evidence", id.0));
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    /// The tree of the live work tree, taken as a snapshot takes it: every
    /// file, tracked or not, through a scratch index.
    pub(crate) fn live_tree(&self) -> String {
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
    pub(crate) fn assert_passing_evidence_is_whole(&self, records: &[Record], commands: usize) {
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
pub(crate) fn runs_in(data: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(data.join("fleet")) else {
        return Vec::new();
    };
    let mut runs: Vec<String> = entries
        // An entry that cannot be read is no run of this test's.
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.join("run.json").exists())
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    runs.sort();
    runs
}

// ---- reading the log ---------------------------------------------------

/// The steps of the attempts started, in order, with their attempt IDs.
pub(crate) fn started(records: &[Record]) -> Vec<(AttemptId, &str)> {
    records
        .iter()
        .filter_map(|record| match record {
            Record::AttemptStarted { attempt, step, .. } => Some((*attempt, step.as_str())),
            _ => None,
        })
        .collect()
}

/// The position in the log of the first record `found` accepts.
pub(crate) fn position(records: &[Record], found: impl Fn(&Record) -> bool) -> Option<usize> {
    records.iter().position(found)
}

/// Whether `record` finished a tests attempt that passed.
pub(crate) fn passed(record: &Record) -> bool {
    matches!(
        record,
        Record::AttemptFinished {
            outcome: Accepted::TestsPassed { .. },
            ..
        }
    )
}

/// Whether `record` finished a tests attempt that failed.
pub(crate) fn failed(record: &Record) -> bool {
    matches!(
        record,
        Record::AttemptFinished {
            outcome: Accepted::TestsFailed { .. },
            ..
        }
    )
}

/// The candidate and restart of `attempt`'s last evidence phase.
pub(crate) fn last_phase(records: &[Record], attempt: AttemptId) -> (CandidateId, u32) {
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
pub(crate) fn observations(
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
pub(crate) fn exit_of(
    records: &[Record],
    attempt: AttemptId,
    restart: u32,
    index: usize,
) -> Option<i32> {
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
pub(crate) fn tree_of(records: &[Record], id: CandidateId) -> String {
    records
        .iter()
        .find_map(|record| match record {
            Record::Candidate { id: c, tree, .. } if *c == id => Some(tree.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no candidate {}: {records:#?}", id.0))
}

/// The origin of candidate `id`.
pub(crate) fn origin_of(records: &[Record], id: CandidateId) -> Origin {
    records
        .iter()
        .find_map(|record| match record {
            Record::Candidate { id: c, origin, .. } if *c == id => Some(*origin),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no candidate {}: {records:#?}", id.0))
}

/// The only tests attempt of the run.
pub(crate) fn only_test_attempt(records: &[Record]) -> AttemptId {
    let tests: Vec<AttemptId> = started(records)
        .into_iter()
        .filter(|(_, step)| *step == "test")
        .map(|(attempt, _)| attempt)
        .collect();
    assert_eq!(tests.len(), 1, "{records:#?}");
    tests[0]
}

/// The pause the log ends with.
pub(crate) fn last_pause(records: &[Record]) -> PauseReason {
    match records.last() {
        Some(Record::Paused { reason, .. }) => *reason,
        other => panic!("the log ends with {other:?}, not a pause"),
    }
}

// ---- the octet process -------------------------------------------------

/// What an `octet` process left.
pub(crate) struct Ended {
    pub(crate) code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl Ended {
    /// Asserts the exit code, showing the output when it is another.
    pub(crate) fn expect_code(&self, code: i32) {
        assert_eq!(
            self.code,
            Some(code),
            "stdout:\n{}\nstderr:\n{}",
            self.stdout,
            self.stderr
        );
    }
}

/// A running `octet`, leading its own process group. Dropped unreaped, it
/// kills that group. What its vendors and commands leave behind the
/// [`Bench`] kills when the test ends: a test may keep such a process alive
/// from one `octet` to the next on purpose.
pub(crate) struct Octet {
    child: Child,
    reaped: bool,
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
    pub(crate) fn spawn(mut command: Command) -> Self {
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
            stdout,
            stderr,
        }
    }

    pub(crate) fn pid(&self) -> i32 {
        i32::try_from(self.child.id()).unwrap()
    }

    /// Whether the process is still running.
    pub(crate) fn running(&mut self) -> bool {
        if self.reaped {
            return false;
        }
        let ended = self.child.try_wait().unwrap().is_some();
        self.reaped = ended;
        !ended
    }

    /// What is known of the output, for a failure message, once it ended.
    pub(crate) fn output_so_far(&self) -> String {
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

    /// Sends `signal` to `octet` alone, which must still be running.
    pub(crate) fn signal(&mut self, signal: i32) {
        assert!(self.running(), "octet ended: {}", self.output_so_far());
        // SAFETY: kill(2) of a child this test spawned and has not reaped.
        assert_eq!(unsafe { libc::kill(self.pid(), signal) }, 0);
    }

    /// SIGKILLs `octet` alone (not its group) and reaps it.
    pub(crate) fn sigkill(&mut self) {
        // SAFETY: kill(2) of a child this test spawned and has not reaped.
        assert_eq!(unsafe { libc::kill(self.pid(), libc::SIGKILL) }, 0);
        self.child.wait().unwrap();
        self.reaped = true;
    }

    /// Waits for `octet` to exit, for at most [`PATIENCE`].
    pub(crate) fn finish(mut self) -> Ended {
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
    }
}

/// The PIDs of the live processes whose `OCTET_FLEET_ATTEMPT` marker names a
/// run in `data`: a vendor or a test command of a run this test drove. Empty
/// when the processes cannot be listed.
pub(crate) fn marked(data: &Path) -> Vec<u32> {
    let runs = runs_in(data);
    if runs.is_empty() {
        return Vec::new();
    }
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return Vec::new();
    };
    let Ok(processes) = runtime.block_on(octet_fleet::ownership::list_processes()) else {
        return Vec::new();
    };
    processes
        .into_iter()
        .filter(|process| {
            process.marker.as_deref().is_some_and(|marker| {
                runs.iter().any(|run| {
                    marker
                        .strip_prefix(run.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
                })
            })
        })
        .map(|process| process.pid)
        .collect()
}

/// SIGKILLs every process [`marked`] finds for `data`.
pub(crate) fn kill_marked(data: &Path) {
    for pid in marked(data) {
        if let Ok(pid) = i32::try_from(pid) {
            // SAFETY: kill(2) of a process carrying this test's run marker.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// Whether process `pid` exists.
pub(crate) fn alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// The live processes of `attempt` of run `run`: those its
/// `attempt_session` records or its gate registry (`<attempt>/processes`,
/// read afresh) name, and any carrying a marker of the bench's runs.
pub(crate) fn attempt_processes_of(bench: &Bench, run: &str, attempt: AttemptId) -> Vec<u32> {
    let records = bench.records_of(run);
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
    pids.extend(registered(bench, run, attempt));
    pids.retain(|&pid| alive(pid));
    pids.extend(marked(&bench.data));
    pids
}

/// Waits until the killed driver's processes of `attempt` are gone, for at
/// most [`SHORT`]. A gate spawned just before the kill may register itself
/// late, so "gone" must hold on two looks 200 ms apart.
pub(crate) fn await_gone(bench: &Bench, attempt: AttemptId) {
    await_gone_of(bench, &bench.run_id(), attempt);
}

/// [`await_gone`] for `attempt` of run `run`.
pub(crate) fn await_gone_of(bench: &Bench, run: &str, attempt: AttemptId) {
    let mut left = Vec::new();
    let gone = octet_testkit::wait_until(SHORT, || {
        left = attempt_processes_of(bench, run, attempt);
        if !left.is_empty() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
        left = attempt_processes_of(bench, run, attempt);
        left.is_empty()
    });
    assert!(gone, "the killed driver's processes {left:?} outlived it");
}

// ---- 10. an edit between commands --------------------------------------

/// Answers the evidence runner's test pauses in `dir` until `stop`: each
/// `held-<restart>-<index>` gets its `continue-…`, and before the one of
/// command 1 in pass 0 `edit` runs. Ends by itself after [`PATIENCE`].
pub(crate) fn answer_pauses(dir: PathBuf, stop: Arc<AtomicBool>, edit: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        let deadline = Instant::now() + PATIENCE;
        while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            // The directory goes with the test: then there is nothing to answer.
            let Ok(entries) = std::fs::read_dir(&dir) else {
                break;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
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
                if std::fs::write(go, "").is_err() {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
}

// ---- processes ---------------------------------------------------------

/// Every live process, as Octet lists them; none when they cannot be listed.
pub(crate) fn processes() -> Vec<ProcessInfo> {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return Vec::new();
    };
    runtime
        .block_on(octet_fleet::ownership::list_processes())
        .unwrap_or_default()
}

/// Whether process `pid` has exited: it is gone, or it is a zombie that its
/// (stopped or dead) parent has not reaped yet.
pub(crate) fn exited(pid: u32) -> bool {
    !alive(pid) || zombie(pid)
}

#[cfg(target_os = "linux")]
fn zombie(pid: u32) -> bool {
    // The state is the field after the command name, which ends at the last `)`.
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            let end = stat.rfind(')')?;
            Some(stat[end + 1..].trim_start().starts_with('Z'))
        })
        .unwrap_or(true)
}

#[cfg(target_os = "macos")]
fn zombie(pid: u32) -> bool {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let state = String::from_utf8_lossy(&output.stdout);
    let state = state.trim();
    state.is_empty() || state.starts_with('Z')
}

/// Waits, at most [`SHORT`], until `pid` has [`exited`].
pub(crate) fn await_exited(pid: u32, what: &str) {
    assert!(
        octet_testkit::wait_until(SHORT, || exited(pid)),
        "{what} ({pid}) did not exit within {SHORT:?}"
    );
}

/// The survivors this bench's fake vendors started, alive now: the fake run
/// as `fleet-survivor SECS SCRIPT`, naming the bench's script. (On macOS the
/// listed command goes on with the environment, so `octet` itself names the
/// script too; only a survivor's command starts so.)
pub(crate) fn survivors(bench: &Bench) -> Vec<ProcessInfo> {
    let start = format!(
        "{} fleet-survivor ",
        octet_testkit::protocol_child().display()
    );
    let script = format!(" {}", bench.script.display());
    processes()
        .into_iter()
        .filter(|process| process.command.starts_with(&start) && process.command.contains(&script))
        .collect()
}

impl Drop for Bench {
    /// Kills, however the test ended, every process carrying a marker of one
    /// of the bench's runs, and the survivors its fakes started (one without
    /// the marker included).
    fn drop(&mut self) {
        kill_marked(&self.data);
        for survivor in survivors(self) {
            if let Ok(pid) = i32::try_from(survivor.pid) {
                // SAFETY: kill(2) of a survivor naming this bench's script.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }
}

/// The PIDs the gate registered for `attempt` of run `run`.
pub(crate) fn registered(bench: &Bench, run: &str, attempt: AttemptId) -> Vec<u32> {
    let registry = bench
        .run_dir_of(run)
        .join("attempts")
        .join(attempt.0.to_string())
        .join("processes");
    std::fs::read_to_string(registry)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

// ---- synchronising with octet ------------------------------------------

/// Waits until `path` exists while `octet` runs. Fails the test, with what
/// was seen, when `octet` ends first or [`PATIENCE`] passes.
pub(crate) fn await_path(octet: &mut Octet, path: &Path) {
    let deadline = Instant::now() + PATIENCE;
    while !path.exists() {
        assert!(
            octet.running(),
            "octet ended before {}: {}",
            path.display(),
            octet.output_so_far()
        );
        assert!(
            Instant::now() < deadline,
            "no {} within {PATIENCE:?}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Waits until `holds` while `octet` runs. Fails the test, with what was
/// seen, when `octet` ends first or [`PATIENCE`] passes.
pub(crate) fn await_until(octet: &mut Octet, what: &str, holds: impl Fn() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !holds() {
        assert!(
            octet.running(),
            "octet ended before {what}: {}",
            octet.output_so_far()
        );
        assert!(Instant::now() < deadline, "no {what} within {PATIENCE:?}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// The path a fake's `wait_for: path` turn creates once it waits.
pub(crate) fn waiting(path: &Path) -> PathBuf {
    let mut waiting = path.as_os_str().to_owned();
    waiting.push(".waiting");
    PathBuf::from(waiting)
}

/// A test command that appends a line to `runs` and, the first time only,
/// then becomes a survivor sleeping for minutes (`exec`, keeping its PID):
/// a command that is still running whenever the test acts.
pub(crate) fn blocking_once(bench: &Bench, runs: &Path) -> String {
    format!(
        "echo ran >> '{runs}'; n=$(wc -l < '{runs}'); [ $n -gt 1 ] || exec '{child}' \
         fleet-survivor 300 '{script}'",
        runs = runs.display(),
        child = octet_testkit::protocol_child().display(),
        script = bench.script.display(),
    )
}

/// The lines of `path`; none when it does not exist.
pub(crate) fn lines_of(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .count()
}

/// The environment that turns the test pauses on, in `dir`.
pub(crate) fn pause_env(dir: &Path) -> [(&'static str, &OsStr); 1] {
    [(TEST_PAUSE_ENV, dir.as_os_str())]
}

/// Lets `octet` through every test pause in `dir` until it holds at a point
/// `target` accepts (`held-<point>`), and returns that point, still held.
/// Fails the test when `octet` ends first or [`PATIENCE`] passes.
pub(crate) fn pause_at(octet: &mut Octet, dir: &Path, target: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + PATIENCE;
    loop {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(point) = name.strip_prefix("held-") else {
                continue;
            };
            if target(point) {
                return point.to_owned();
            }
            let go = dir.join(format!("continue-{point}"));
            if !go.exists() {
                std::fs::write(go, "").unwrap();
            }
        }
        assert!(
            octet.running(),
            "octet ended before the pause: {}",
            octet.output_so_far()
        );
        assert!(Instant::now() < deadline, "no pause within {PATIENCE:?}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// The attempt of `step` started last in `records`, if any.
pub(crate) fn last_attempt_of(records: &[Record], step: &str) -> Option<AttemptId> {
    started(records)
        .into_iter()
        .rev()
        .find(|(_, s)| *s == step)
        .map(|(attempt, _)| attempt)
}

/// A `PATH` with a `git` first on it that, the first time Octet runs `git`
/// while the shell condition `when` holds, SIGKILLs Octet (the process that
/// ran it) instead of running git, and records that it did in the file it
/// returns. Otherwise it runs the real `git`. Octet runs no git while a
/// vendor's turn runs, so "the first git after X" is an exact point.
pub(crate) fn git_tripwire(bench: &Bench, when: &str) -> (std::ffi::OsString, PathBuf) {
    let real_git = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real_git = String::from_utf8(real_git.stdout)
        .unwrap()
        .trim()
        .to_owned();
    assert!(Path::new(&real_git).is_absolute(), "{real_git:?}");
    let fired = bench.side("tripwire-fired");
    let bin = bench.side("tripwire-bin");
    octet_testkit::write_script(
        &bin,
        "git",
        &format!(
            "if [ ! -e '{fired}' ] && {{ {when} ; }}; then\n\
             : > '{fired}'\n\
             kill -9 $PPID\n\
             exit 1\n\
             fi\n\
             exec '{real_git}' \"$@\"",
            fired = fired.display(),
        ),
    );
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    (path, fired)
}

/// A `PATH` with a `sleep` first on it that, while the file it returns
/// first exists, holds every caller before sleeping, appending the caller's
/// parent PID to the second file as it starts to hold (for at most five
/// minutes). A gate polls with `sleep`: held there, it cannot see that Octet
/// is gone, and once let go it does, and exits.
pub(crate) fn sleep_hold(bench: &Bench) -> (std::ffi::OsString, PathBuf, PathBuf) {
    let real_sleep = Command::new("sh")
        .args(["-c", "command -v sleep"])
        .output()
        .unwrap();
    let real_sleep = String::from_utf8(real_sleep.stdout)
        .unwrap()
        .trim()
        .to_owned();
    assert!(Path::new(&real_sleep).is_absolute(), "{real_sleep:?}");
    let hold = bench.side("sleep-hold");
    let held = bench.side("sleep-held");
    let bin = bench.side("sleep-bin");
    octet_testkit::write_script(
        &bin,
        "sleep",
        &format!(
            "if [ -e '{hold}' ]; then\n\
             echo $PPID >> '{held}'\n\
             n=0\n\
             while [ -e '{hold}' ] && [ $n -lt 30000 ]; do '{real_sleep}' 0.01; n=$((n + 1)); done\n\
             fi\n\
             exec '{real_sleep}' \"$@\"",
            hold = hold.display(),
            held = held.display(),
        ),
    );
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    (path, hold, held)
}

/// The shell condition "the fake has played turn `key`".
pub(crate) fn played_turn(bench: &Bench, key: &str) -> String {
    format!(
        "grep -q '\"key\":\"{key}\"' '{}' 2>/dev/null",
        bench.turns.display()
    )
}

/// The shell condition "a run's log holds a line with `text`".
pub(crate) fn logged(bench: &Bench, text: &str) -> String {
    format!(
        "cat '{}'/fleet/*/log.jsonl 2>/dev/null | grep -q '{text}'",
        bench.data.display()
    )
}
